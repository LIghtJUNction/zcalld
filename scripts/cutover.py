#!/usr/bin/env python3
"""Explicit Linux user-service cutover from a running Magpie to a staged zcalld.

prepare writes a private recovery journal; apply performs the switch; rollback
drains zcalld and restores the old launch. No command sends a generation request.
"""
import argparse
import configparser
import contextlib
import ctypes
import datetime
import fcntl
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from urllib.parse import urlsplit

JSON_LIMIT = 8 << 20
BLOCK = 1 << 20
UNIT = "zcalld.service"
MCP_PATH = "/mcp/magpie/tools"
MCP_PROTOCOL = "2025-06-18"
BOOT_ID = Path("/proc/sys/kernel/random/boot_id").read_text().strip() if sys.platform == "linux" else ""


class CutoverError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CutoverError(message)


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()


def digest(value):
    return hashlib.sha256(encoded(value)).hexdigest()


def regular(path):
    info = Path(path).lstat()
    require(stat.S_ISREG(info.st_mode), f"expected a regular file: {path}")
    return info


def read_json(path, default=None):
    path = Path(path)
    if not path.exists() and default is not None:
        return default
    require(regular(path).st_size <= JSON_LIMIT, "JSON input exceeds 8 MiB")
    with path.open("rb") as stream:
        value = stream.read(JSON_LIMIT + 1)
    require(len(value) <= JSON_LIMIT, "JSON input grew beyond its limit")
    try:
        return json.loads(value)
    except (ValueError, UnicodeError):
        raise CutoverError(f"invalid JSON in {path.name}") from None


def file_hash(path, limit=None):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        remaining = limit
        while remaining is None or remaining:
            part = stream.read(BLOCK if remaining is None else min(BLOCK, remaining))
            if not part:
                require(remaining in (None, 0), "file is shorter than its recorded prefix")
                break
            h.update(part)
            if remaining is not None:
                remaining -= len(part)
    return h.hexdigest()


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


@contextlib.contextmanager
def replacement(path, mode=0o600):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.exists() or path.is_symlink():
        regular(path)
    fd, temporary = tempfile.mkstemp(prefix=".zcalld-cutover-", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            yield stream
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def atomic(path, data, mode=0o600):
    with replacement(path, mode) as stream:
        stream.write(data)


def save_json(path, value):
    atomic(path, encoded(value) + b"\n")


def copy_file(source, destination):
    regular(source)
    with Path(source).open("rb") as incoming, replacement(destination) as outgoing:
        shutil.copyfileobj(incoming, outgoing, BLOCK)


def is_prefix(short, long):
    short, long = Path(short), Path(long)
    if not short.exists():
        return True
    if not long.exists() or regular(short).st_size > regular(long).st_size:
        return False
    with short.open("rb") as a, long.open("rb") as b:
        while True:
            part = a.read(BLOCK)
            if not part:
                return True
            if b.read(len(part)) != part:
                return False


def complete_jsonl(path):
    if Path(path).exists() and regular(path).st_size:
        with Path(path).open("rb") as stream:
            stream.seek(-1, os.SEEK_END)
            require(stream.read(1) == b"\n", "usage ledger has an unfinished final record")


# Magpie maskrec uses daily JSONL records and atomically replaced daily counters.
# Keep bytes and deletion semantics; never treat the counters or a fallback key
# as append-only logs. These bounds exceed its 30 day / 100 MiB retention window.
HISTORY_LIMIT = 512 << 20
HISTORY_FILES = 4096
HISTORY_RECORD = 16 << 20


def history_kind(name):
    if name == "redact.key":
        return "key"
    match = re.fullmatch(r"(\d{4}-\d{2}-\d{2})\.jsonl", name)
    kind = "records"
    if not match:
        match = re.fullmatch(r"checked-(\d{4}-\d{2}-\d{2})\.json", name)
        kind = "checked"
    require(match is not None, "privacy history contains an unsupported entry")
    try:
        datetime.date.fromisoformat(match.group(1))
    except ValueError:
        raise CutoverError("privacy history has an invalid day") from None
    return kind


def history_directory(path):
    path = Path(path)
    require(path.is_absolute() and path.parent.resolve(strict=True) == path.parent,
            "privacy history parent must not traverse a symlink")
    try:
        info = path.lstat()
    except FileNotFoundError:
        return False
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and not stat.S_IMODE(info.st_mode) & 0o077,
            "privacy history must be a private owned directory")
    return True


@contextlib.contextmanager
def history_reader(path, dir_fd=None):
    # O_NONBLOCK makes a raced FIFO fail the regular-file check without hanging.
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dir_fd)
    except OSError:
        raise CutoverError("privacy history path is unsafe or unavailable") from None
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and info.st_nlink == 1 and not stat.S_IMODE(info.st_mode) & 0o077,
                "privacy history and keys must be private regular files without links")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            yield stream, info
            after = os.fstat(fd)
            require((info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
                    == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
                    "privacy history changed while it was being read")
    finally:
        os.close(fd)


def history_json(body):
    def reject_constant(_):
        raise ValueError
    try:
        value = json.loads(body, parse_constant=reject_constant)
    except (ValueError, UnicodeError):
        raise CutoverError("privacy history contains invalid JSON") from None
    require(isinstance(value, dict), "privacy history records must be JSON objects")
    return value


def history_file(path, kind, dir_fd=None):
    h = hashlib.sha256()
    with history_reader(path, dir_fd) as (stream, info):
        require(info.st_size <= HISTORY_LIMIT, "privacy history file exceeds its limit")
        if kind == "records":
            size = 0
            while line := stream.readline(HISTORY_RECORD + 1):
                require(len(line) <= HISTORY_RECORD and line.endswith(b"\n"),
                        "privacy history has an incomplete or oversized JSONL record")
                if line.strip():
                    history_json(line)
                size += len(line)
                require(size <= HISTORY_LIMIT, "privacy history grew beyond its limit")
                h.update(line)
        else:
            limit = 4096 if kind == "key" else JSON_LIMIT
            require(info.st_size <= limit, "privacy history metadata exceeds its limit")
            body = stream.read(limit + 1)
            require(len(body) <= limit, "privacy history metadata grew beyond its limit")
            if kind == "key":
                require(len(body) >= 32, "privacy key is missing or invalid")
            else:
                counters = history_json(body)
                require(all(isinstance(value, dict)
                            and all(type(count) is int and count >= 0 for count in value.values())
                            for value in counters.values()),
                        "privacy history checked counters are invalid")
            size = len(body)
            h.update(body)
    return {"bytes": size, "sha256": h.hexdigest()}


def history_manifest(path):
    path = Path(path)
    if not history_directory(path):
        return {}
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        before = os.fstat(fd)
        require(before.st_uid == os.getuid() and not stat.S_IMODE(before.st_mode) & 0o077,
                "privacy history directory ownership changed")
        names = sorted(os.listdir(fd))
        require(len(names) <= HISTORY_FILES, "privacy history has too many files")
        result, total = {}, 0
        for name in names:
            value = history_file(name, history_kind(name), fd)
            total += value["bytes"]
            require(total <= HISTORY_LIMIT, "privacy history exceeds its total limit")
            result[name] = value
        after = os.fstat(fd)
        current = path.lstat()
        require((before.st_dev, before.st_ino, before.st_mtime_ns, before.st_ctime_ns)
                == (after.st_dev, after.st_ino, after.st_mtime_ns, after.st_ctime_ns)
                and (current.st_dev, current.st_ino) == (before.st_dev, before.st_ino),
                "privacy history directory changed while it was being read")
        return result
    finally:
        os.close(fd)


def history_lock_path(directory):
    directory = Path(directory)
    require(directory.is_absolute() and directory.parent.resolve(strict=True) == directory.parent,
            "privacy history lock parent must not traverse a symlink")
    return directory.parent / ("." + directory.name + ".zcalld-audit.lock")


@contextlib.contextmanager
def history_locks(*directories, create=False):
    """Lock a stable sibling inode; exchanging the history never moves it."""
    held = []
    try:
        for directory in sorted({str(Path(p)) for p in directories if p is not None}):
            history_directory(directory)  # reject an unsafe existing root
            path = history_lock_path(directory)
            if not create and not path.exists() and not path.is_symlink():
                continue
            try:
                flags = os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK
                fd = os.open(path, flags | (os.O_CREAT if create else 0), 0o600)
            except OSError:
                raise CutoverError("privacy history writer lock is unsafe or unavailable") from None
            try:
                info = os.fstat(fd)
                require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                        and info.st_nlink == 1 and info.st_size in (0, 8)
                        and not stat.S_IMODE(info.st_mode) & 0o077,
                        "privacy history writer lock is invalid")
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                current = path.lstat()
                require((current.st_dev, current.st_ino) == (info.st_dev, info.st_ino),
                        "privacy history writer lock changed during acquisition")
            except (OSError, CutoverError):
                os.close(fd)
                raise CutoverError("privacy history has an active writer or unsafe lock") from None
            held.append(fd)
        yield
    finally:
        for fd in reversed(held):
            os.close(fd)


def privacy_key_path(directory):
    primary = Path(directory) / "redact.key"
    return primary if primary.exists() or primary.is_symlink() else Path(directory) / "masking/redact.key"


def privacy_key(path):
    path = Path(path)
    require(path.is_absolute() and path.parent.resolve(strict=True) == path.parent,
            "privacy key parent must not traverse a symlink")
    return history_file(path, "key")


def history_copy(source, destination, expected):
    """Fill a journal-owned, unpublished directory; both live trees stay intact."""
    source, destination = Path(source), Path(destination)
    require(history_directory(destination), "private history staging directory is missing")
    # A crash may leave an incomplete file here. Only registered, uncommitted
    # staging names may be regenerated; never clean a live or exchanged tree.
    for entry in destination.iterdir():
        require(entry.name in expected, "unrecognized file in privacy history staging")
        with history_reader(entry):
            pass
        entry.unlink()
    for name, metadata in expected.items():
        with history_reader(source / name) as (incoming, _):
            fd = os.open(destination / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "wb") as outgoing:
                remaining = metadata["bytes"]
                while remaining:
                    block = incoming.read(min(BLOCK, remaining))
                    require(block, "privacy history source became shorter during copying")
                    outgoing.write(block)
                    remaining -= len(block)
                require(not incoming.read(1), "privacy history source grew during copying")
                outgoing.flush()
                os.fsync(outgoing.fileno())
    sync_directory(destination)
    require(history_manifest(destination) == expected and history_manifest(source) == expected,
            "privacy history changed while its private snapshot was being prepared")


def history_move(source, destination, exchange):
    """Linux renameat2 keeps both full trees or publishes into an absent path."""
    require(sys.platform == "linux", "atomic privacy history handover requires Linux")
    libc = ctypes.CDLL(None, use_errno=True)
    move = getattr(libc, "renameat2", None)
    require(move is not None, "renameat2 is required for atomic privacy history handover")
    move.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    move.restype = ctypes.c_int
    # RENAME_EXCHANGE=2; RENAME_NOREPLACE=1. Never replace an unverified tree.
    if move(-100, os.fsencode(source), -100, os.fsencode(destination), 2 if exchange else 1):
        raise CutoverError(f"atomic privacy history move failed (errno {ctypes.get_errno()})")
    sync_directory(Path(destination).parent)
    if Path(source).parent != Path(destination).parent:
        sync_directory(Path(source).parent)


def catalog(value):
    require(isinstance(value, dict) and isinstance(value.get("data"), list), "invalid model catalog")
    result = {}
    for model in value["data"]:
        require(isinstance(model, dict) and isinstance(model.get("id"), str), "invalid catalog model")
        require(model["id"] not in result, "duplicate catalog model")
        result[model["id"]] = model
    require(result, "empty model catalog")
    return result


def settings_fingerprint(source):
    value = read_json(Path(source) / "settings.json")
    relevant = {k: v for k, v in value.items()
                if k in ("lan", "lanKey", "pluginOptions") or k.startswith(("redact", "mask"))}
    return digest(relevant)


def required_mcp(source):
    """Only preserve an assigned, enabled Magpie-owned HTTP library endpoint."""
    library = read_json(Path(source) / "library.json", {})
    require(isinstance(library, dict), "MCP library must be an object")
    entries = library.get("mcp") or []
    require(isinstance(entries, list), "MCP library entries must be an array")
    for entry in entries:
        require(isinstance(entry, dict), "MCP library entry must be an object")
        if any(entry.get(key) is True for key in ("disabled", "hidden", "offHere")):
            continue
        agents = entry.get("agents")
        if agents is not None:
            require(isinstance(agents, list) and all(isinstance(x, str) for x in agents),
                    "MCP library agent assignments are invalid")
            if not agents:
                continue
        if entry.get("transport") == "stdio" or not isinstance(entry.get("url"), str):
            continue
        try:
            url = urlsplit(entry["url"])
            selected = (url.scheme == "http" and url.hostname in ("127.0.0.1", "localhost", "::1")
                        and url.port == 3425 and url.path == MCP_PATH
                        and not url.query and not url.fragment and not url.username and not url.password)
        except ValueError:
            selected = False
        if selected:
            return True
    return False


def mcp_catalog(tools):
    """Compare complete tool definitions, without making list order significant."""
    require(isinstance(tools, list), "MCP tool catalog must be an array")
    result = {}
    for tool in tools:
        require(isinstance(tool, dict) and isinstance(tool.get("name"), str) and tool["name"],
                "MCP tool definition has no name")
        require(tool["name"] not in result, "MCP tool catalog contains duplicate names")
        result[tool["name"]] = tool
    require(len(encoded(result)) <= JSON_LIMIT, "MCP tool catalog exceeds the metadata limit")
    return result


def config_values(path):
    parser = configparser.ConfigParser(interpolation=None)
    parser.read_string(Path(path).read_text())
    return parser


def unit_word(value):
    require(not any(c in value for c in "\x00\r\n"), "invalid service path")
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


def process_identity(pid, launch=False):
    root = Path("/proc") / str(pid)
    try:
        text = (root / "stat").read_text()
        fields = text[text.rfind(")") + 1:].split()
        if fields[0] in ("Z", "X"):
            return None
        result = {"pid": pid, "start_ticks": int(fields[19]), "ppid": int(fields[1]), "boot_id": BOOT_ID,
                  "uid": root.stat().st_uid, "exe": os.readlink(root / "exe")}
        if launch:
            result["argv"] = [os.fsdecode(v) for v in (root / "cmdline").read_bytes().split(b"\0") if v]
            result["env"] = dict(os.fsdecode(v).split("=", 1) for v in
                                 (root / "environ").read_bytes().split(b"\0") if b"=" in v)
            result["cwd"] = os.readlink(root / "cwd")
        return result
    except (FileNotFoundError, ProcessLookupError):
        return None


def same_process(saved):
    if saved.get("boot_id", BOOT_ID) != BOOT_ID:
        return False
    current = process_identity(saved["pid"])
    return current is not None and current["start_ticks"] == saved["start_ticks"]


def process_tree(main, exclusions=()):
    rows = {}
    for name in Path("/proc").iterdir():
        if name.name.isdigit():
            try:
                row = process_identity(int(name.name))
                if row and row["uid"] == os.getuid():
                    rows[row["pid"]] = row
            except PermissionError:
                continue
    chosen, excluded = {main["pid"]}, {v["pid"] for v in exclusions if same_process(v)}
    while True:
        children = {pid for pid, row in rows.items() if row["ppid"] in chosen}
        skip = {pid for pid, row in rows.items() if row["ppid"] in excluded}
        next_chosen, next_excluded = chosen | children, excluded | skip
        if next_chosen == chosen and next_excluded == excluded:
            break
        chosen, excluded = next_chosen, next_excluded
    return [rows[pid] for pid in sorted(chosen - excluded) if pid in rows]


def listener_inodes(port):
    result = set()
    for name in ("tcp", "tcp6"):
        for line in (Path("/proc/net") / name).read_text().splitlines()[1:]:
            fields = line.split()
            if fields[3] == "0A" and int(fields[1].rsplit(":", 1)[1], 16) == port:
                result.add(fields[9])
    return result


def owns_listener(pid, port):
    wanted = {f"socket:[{inode}]" for inode in listener_inodes(port)}
    for fd in (Path("/proc") / str(pid) / "fd").iterdir():
        try:
            if os.readlink(fd) in wanted:
                return True
        except FileNotFoundError:
            pass
    return False


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=3)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class System:
    def __init__(self, state):
        self.state = Path(state)

    def run(self, args, label, timeout=90, env=None, accepted=(0,)):
        try:
            result = subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True,
                                    env=env, timeout=timeout, check=False)
        except subprocess.TimeoutExpired:
            raise CutoverError(f"{label} timed out; recorded processes were not force-killed") from None
        # Helpers can report private paths. Their output stays in the private journal.
        with (self.state / "commands.log").open("ab") as stream:
            os.chmod(self.state / "commands.log", 0o600)
            stream.write(encoded({"operation": label, "exit_code": result.returncode}) + b"\n")
            stream.write(result.stdout + result.stderr + b"\n")
        require(result.returncode in accepted, f"{label} failed; inspect the private commands.log")
        return result.stdout

    def unit(self, name):
        data = self.run(["systemctl", "--user", "show", name,
                         "--property=LoadState,ActiveState,MainPID,UnitFileState,FragmentPath,ControlGroup"],
                        "inspect user unit", accepted=(0, 1))
        return dict(line.split("=", 1) for line in data.decode().splitlines() if "=" in line)

    def control(self, verb, unit=None, timeout=90):
        args = ["systemctl", "--user", verb]
        if unit:
            args.append(unit)
        return self.run(args, f"service {verb}", timeout)

    def service_processes(self, name):
        status = self.unit(name)
        group = status.get("ControlGroup", "")
        if group:
            require(group.endswith("/" + name), "unexpected service control group")
            directory = Path("/sys/fs/cgroup") / group.lstrip("/")
            pids = set()
            for file in directory.rglob("cgroup.procs"):
                pids.update(int(value) for value in file.read_text().split())
            return [row for pid in pids if (row := process_identity(pid))]
        pid = int(status.get("MainPID", "0"))
        row = process_identity(pid) if pid else None
        return process_tree(row) if row else []

    def get(self, port, path, unix=None):
        connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port, timeout=3)
        with contextlib.closing(connection):
            connection.request("GET", path, headers={"Host": "localhost"})
            reply = connection.getresponse()
            data = reply.read(JSON_LIMIT + 1)
            require(reply.status == 200 and len(data) <= JSON_LIMIT, "local gateway metadata check failed")
            return json.loads(data)

    def mcp(self, port, path=MCP_PATH, unix=None, protocol=MCP_PROTOCOL):
        """Read protocol and tool metadata only; never send tools/call."""
        require(path == MCP_PATH, "unexpected MCP verification path")
        deadline = time.monotonic() + 15
        session = None
        negotiated = None

        def post(method, request_id, params=None):
            nonlocal session
            require(method in ("initialize", "notifications/initialized", "ping", "tools/list"),
                    "MCP verification cannot execute a tool")
            remaining = deadline - time.monotonic()
            require(remaining > 0, "local MCP metadata verification timed out")
            connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port)
            connection.timeout = min(3, remaining)
            headers = {"Host": "localhost", "Content-Type": "application/json",
                       "Accept": "application/json, text/event-stream"}
            if negotiated:
                headers["MCP-Protocol-Version"] = negotiated
            if session:
                headers["Mcp-Session-Id"] = session
            request = {"jsonrpc": "2.0", "method": method}
            if request_id is not None:
                request["id"] = request_id
            if params is not None:
                request["params"] = params
            with contextlib.closing(connection):
                connection.request("POST", path, body=encoded(request), headers=headers)
                with contextlib.closing(connection.getresponse()) as response:
                    if request_id is None:
                        require(response.status in (202, 204),
                                "local MCP initialization notification was rejected")
                    else:
                        require(response.status == 200, "local MCP " + method + " HTTP request was rejected")
                        require(response.getheader("Content-Type", "").split(";", 1)[0].strip().lower()
                                == "application/json", "local MCP metadata requires a finite JSON response")
                    body = response.read(JSON_LIMIT + 1)
                    require(len(body) <= JSON_LIMIT, "local MCP metadata response is too large")
                    if request_id is None:
                        require(not body.strip(), "local MCP initialization notification returned a body")
                        return None
                    try:
                        reply = json.loads(body)
                    except (ValueError, UnicodeError):
                        raise CutoverError("local MCP " + method + " returned invalid JSON") from None
                    require(isinstance(reply, dict) and reply.get("jsonrpc") == "2.0"
                            and reply.get("id") == request_id and "error" not in reply
                            and isinstance(reply.get("result"), dict),
                            "local MCP " + method + " JSON-RPC request was rejected")
                    if method == "initialize":
                        session = response.getheader("Mcp-Session-Id")
                        require(session is None or (0 < len(session) <= 1024
                                and all(0x21 <= ord(c) <= 0x7e for c in session)),
                                "local MCP returned an invalid session identifier")
                    return reply["result"]

        initialized = post("initialize", "cutover-initialize", {
            "protocolVersion": protocol, "capabilities": {},
            "clientInfo": {"name": "zcalld-cutover", "version": "1"}})
        negotiated = initialized.get("protocolVersion")
        require(isinstance(negotiated, str) and re.fullmatch(r"\d{4}-\d{2}-\d{2}", negotiated),
                "local MCP returned no valid protocol version")
        post("notifications/initialized", None)
        require(post("ping", "cutover-ping") == {}, "local MCP ping returned an invalid result")
        tools, cursors, cursor = [], set(), None
        for page in range(128):
            result = post("tools/list", "cutover-tools-" + str(page),
                          {} if cursor is None else {"cursor": cursor})
            require(isinstance(result.get("tools"), list), "local MCP returned no tool catalog")
            tools.extend(result["tools"])
            mcp_catalog(tools)
            cursor = result.get("nextCursor")
            if cursor is None:
                break
            require(isinstance(cursor, str) and cursor and cursor not in cursors,
                    "local MCP tool pagination did not advance")
            cursors.add(cursor)
        else:
            raise CutoverError("local MCP tool pagination exceeds the metadata limit")
        return {"protocolVersion": negotiated,
                "tools": [tool for _, tool in sorted(mcp_catalog(tools).items())]}

    def terminate(self, identity):
        require(same_process(identity), "recorded Magpie process changed; prepare a fresh transaction")
        require(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "pidfd signaling is required")
        fd = os.pidfd_open(identity["pid"])
        try:
            require(same_process(identity), "Magpie PID was reused before signaling")
            signal.pidfd_send_signal(fd, signal.SIGTERM)
        finally:
            os.close(fd)

    def drain(self, identities, timeout):
        until = time.monotonic() + timeout
        while any(same_process(item) for item in identities):
            require(time.monotonic() < until, "old process or credential worker has not drained; no forced kill was sent")
            time.sleep(0.1)

    def restart_old(self, data):
        launch = data["old_process"]
        require(file_hash(launch["exe"]) == data["old_binary_hash"], "old Magpie binary changed; restore its package before rollback")
        if data.get("old_unit"):
            self.control("start", data["old_unit"])
            return
        unit = "zcalld-rollback-" + data["id"] + ".service"
        env = launch["env"]
        # --setenv=NAME takes its value from this process's environment. Values
        # never appear in command arguments, terminal output, or public logs.
        args = ["systemd-run", "--user", "--collect", "--unit=" + unit,
                "--property=Type=exec", "--working-directory=" + launch["cwd"]]
        args += ["--setenv=" + key for key in env if re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", key)]
        args += ["--", *launch["argv"]]
        self.run(args, "restore original Magpie launch", env=env)

    def restored_old(self, data):
        if not data.get("old_restart_attempted"):
            return False
        name = data.get("old_unit") or "zcalld-rollback-" + data["id"] + ".service"
        status = self.unit(name)
        pid = int(status.get("MainPID", "0"))
        current = process_identity(pid) if pid else None
        return bool(current and current["exe"] == data["old_process"]["exe"] and owns_listener(pid, 3425))


class Transaction:
    def __init__(self, state, ops=None):
        self.state = Path(state)
        self.path = self.state / "journal.json"
        self.ops = ops or System(self.state)
        self.data = read_json(self.path) if self.path.exists() else {}

    def save(self, phase=None):
        if phase:
            self.data["phase"] = phase
        save_json(self.path, self.data)

    def event(self, name):
        with contextlib.suppress(BrokenPipeError):
            print(json.dumps({"phase": name}), flush=True)

    def change(self, path, body):
        path = Path(path)
        key = str(path)
        changes = self.data.setdefault("changes", {})
        if key not in changes:
            existed = path.exists() or path.is_symlink()
            backup = self.state / "files" / str(len(changes))
            old_hash, mode = None, 0o600
            if existed:
                mode = stat.S_IMODE(regular(path).st_mode)
                copy_file(path, backup)
                old_hash = file_hash(path)
            changes[key] = {"backup": str(backup), "existed": existed, "mode": mode,
                            "old_hash": old_hash, "new_hash": hashlib.sha256(body).hexdigest()}
            self.save()
        atomic(path, body)

    def restore_changes(self):
        for name, info in reversed(list(self.data.get("changes", {}).items())):
            path = Path(name)
            current = file_hash(path) if path.exists() else None
            if current == info["old_hash"]:
                continue
            require(current == info["new_hash"], "a launch file changed outside this transaction; refusing to overwrite it")
            if info["existed"]:
                atomic(path, Path(info["backup"]).read_bytes(), info["mode"])
            else:
                path.unlink()
                sync_directory(path.parent)

    def candidate(self):
        return Path(self.data["config"]).parent

    def helper(self, verb, *args):
        return self.ops.run([self.data["pluginctl"], "--root", str(self.candidate() / "extensions"),
                             verb, *args, "--json"], "credential " + verb)

    def save_recovery(self):
        source = Path(__file__).resolve(strict=True)
        regular(source)
        recovery = self.state / "recovery.py"
        atomic(recovery, source.read_bytes(), 0o700)
        self.data["recovery_file"] = str(recovery)
        self.data["recovery_sha256"] = file_hash(recovery)

    def prepare_privacy(self, source, target, snapshot):
        with history_locks(Path(source) / "masking", Path(target) / "privacy/history", Path(snapshot) / "masking"):
            return self._prepare_privacy(source, target, snapshot)

    def _prepare_privacy(self, source, target, snapshot):
        source, target, snapshot = Path(source), Path(target), Path(snapshot)
        binding_path = target / "privacy/plugin.json"
        binding = read_json(binding_path)
        settings = Path(binding.get("settings", ""))
        key = Path(binding.get("key", ""))
        configured = binding.get("history_dir")
        require(configured is None or isinstance(configured, str),
                "privacy history_dir must be an absolute path")
        history = Path(configured) if configured else settings.parent / "history"
        require(settings.is_absolute() and key == target / "privacy/redact.key"
                and history == target / "privacy/history",
                "privacy binding must use the candidate privacy key and history directory")
        regular(settings)
        source_key = privacy_key_path(source)
        fingerprint = privacy_key(source_key)
        require(privacy_key(key) == fingerprint
                and privacy_key(privacy_key_path(snapshot)) == fingerprint,
                "privacy key differs from the original migration snapshot")
        expected = history_manifest(snapshot / "masking")
        staged = history_manifest(history)
        require(staged == expected, "candidate privacy history changed since migration")
        original = history_manifest(source / "masking")
        require(original.get("redact.key") == expected.get("redact.key"),
                "archived fallback privacy key changed since migration")
        return {"source_key": str(source_key), "key": str(key), "key_fingerprint": fingerprint,
                "binding": str(binding_path), "binding_hash": file_hash(binding_path),
                "source_history": str(source / "masking"), "history": str(history),
                "staged": staged, "staged_exists": history_directory(history)}

    def check_privacy(self, staged=False):
        privacy = self.data.get("privacy")
        require(isinstance(privacy, dict), "prepared journal has no privacy baseline; prepare a new transaction")
        require(file_hash(privacy["binding"]) == privacy["binding_hash"],
                "privacy worker binding changed after preparation")
        require(privacy_key_path(self.data["source"]) == Path(privacy["source_key"])
                and privacy_key(privacy["source_key"]) == privacy["key_fingerprint"]
                and privacy_key(privacy["key"]) == privacy["key_fingerprint"],
                "privacy key identity changed; refusing to overwrite history")
        if staged:
            with history_locks(privacy["history"]):
                require(history_directory(privacy["history"]) == privacy["staged_exists"]
                        and history_manifest(privacy["history"]) == privacy["staged"],
                        "candidate privacy history changed under another writer")
        return privacy

    def history_operation(self, direction, incoming, destination, before, before_exists):
        with history_locks(incoming, destination, create=True):
            return self._history_operation(direction, incoming, destination, before, before_exists)

    def _history_operation(self, direction, incoming, destination, before, before_exists):
        """A persisted three-way comparison, with an atomic whole-tree exchange.

        The source owner has stopped. A live destination must still equal its
        recorded base, or the complete result of this same journaled operation.
        This preserves append bytes, day deletion/recreation and checked JSON
        replacements without guessing how to merge two independent writers.
        """
        privacy = self.check_privacy()
        incoming, destination = Path(incoming), Path(destination)
        require(incoming != destination, "privacy history source and destination must differ")
        require(re.fullmatch(r"[A-Za-z0-9_-]{1,32}", self.data["id"]) is not None,
                "invalid privacy history transaction identifier")
        plan = privacy.get(direction)
        if plan is None:
            require(history_directory(destination) == before_exists
                    and history_manifest(destination) == before,
                    "original privacy history changed under another writer" if direction == "rollback"
                    else "candidate privacy history changed under another writer")
            after = history_manifest(incoming)
            require(after.get("redact.key") == before.get("redact.key"),
                    "archived fallback privacy key changed; refusing history replacement")
            # The immutable key, not a placeholder dictionary, is the only key
            # input. Historical rows and counters retain their original bytes.
            stage = None
            if after != before:
                state = self.state.resolve(strict=True)
                parent = state if state.stat().st_dev == destination.parent.stat().st_dev else destination.parent
                stage = tempfile.mkdtemp(prefix=f".zcalld-history-{self.data['id']}-", dir=parent)
            plan = {"incoming": str(incoming), "incoming_exists": history_directory(incoming),
                    "destination": str(destination), "before": before, "before_exists": before_exists,
                    "after": after, "stage": stage, "ready": stage is None, "committed": False}
            privacy[direction] = plan
            self.save()
        require(plan["incoming"] == str(incoming) and plan["destination"] == str(destination)
                and plan["before"] == before and plan["before_exists"] == before_exists,
                "privacy history recovery plan does not match its recorded base")
        require(history_directory(incoming) == plan["incoming_exists"]
                and history_manifest(incoming) == plan["after"],
                "privacy history source changed after its recovery snapshot")
        stage = Path(plan["stage"]) if plan["stage"] else None
        if stage is None:
            require(history_directory(destination) == before_exists
                    and history_manifest(destination) == before,
                    "privacy history changed under another writer")
            plan["committed"] = True
            self.save()
            return plan["after"]
        require(stage.parent in (self.state.resolve(strict=True), destination.parent)
                and stage.parent.stat().st_dev == destination.parent.stat().st_dev
                and stage.name.startswith(f".zcalld-history-{self.data['id']}-"),
                "invalid private privacy history staging path")
        if not plan["ready"]:
            require(history_directory(destination) == before_exists
                    and history_manifest(destination) == before,
                    "privacy history changed while staging its replacement")
            history_copy(incoming, stage, plan["after"])
            plan["ready"] = True
            self.save()  # durable before any live tree is moved
        current_exists = history_directory(destination)
        current = history_manifest(destination)
        if before_exists:
            require(history_directory(stage), "privacy history recovery directory is missing")
            previous = history_manifest(stage)
            if current == plan["after"] and current_exists:
                require(previous == before, "exchanged privacy history was changed by another writer")
            else:
                require(current_exists and current == before and previous == plan["after"],
                        "privacy history changed under another writer")
                history_move(stage, destination, exchange=True)
                try:
                    require(history_manifest(stage) == before
                            and history_manifest(destination) == plan["after"]
                            and history_manifest(incoming) == plan["after"],
                            "privacy history changed during atomic exchange")
                except (OSError, CutoverError):
                    # EXCHANGE retained the displaced tree. Restore that tree
                    # if our published snapshot is still untouched, including
                    # changes made by a writer immediately before the move.
                    if history_manifest(destination) == plan["after"]:
                        history_move(stage, destination, exchange=True)
                    raise CutoverError("privacy history changed during atomic exchange; recovery needs review") from None
        else:
            if current_exists:
                require(current == plan["after"] and not stage.exists(),
                        "privacy history appeared under another writer")
            else:
                require(history_manifest(stage) == plan["after"],
                        "private privacy history snapshot was changed")
                history_move(stage, destination, exchange=False)
                try:
                    require(history_manifest(destination) == plan["after"]
                            and history_manifest(incoming) == plan["after"],
                            "privacy history changed during atomic publication")
                except (OSError, CutoverError):
                    if history_manifest(destination) == plan["after"] and not stage.exists():
                        history_move(destination, stage, exchange=False)
                    raise CutoverError("privacy history changed during atomic publication; recovery needs review") from None
        self.check_privacy()
        plan["committed"] = True
        self.save()
        return plan["after"]

    def sync_privacy_in(self):
        privacy = self.check_privacy(staged="forward" not in self.data.get("privacy", {}))
        before_exists = history_directory(privacy["source_history"])
        privacy["base"] = self.history_operation("forward", privacy["source_history"], privacy["history"],
                                                 privacy["staged"], privacy["staged_exists"])
        privacy["base_exists"] = before_exists
        self.save()

    def sync_privacy_back(self):
        # Older recovery scripts had no runtime audit writer. Their journals
        # can still recover credentials/usage without inventing a history base.
        if "privacy" not in self.data:
            return
        privacy = self.check_privacy()
        require("base" in privacy, "privacy history has no completed handover baseline")
        self.history_operation("rollback", privacy["history"], privacy["source_history"],
                               privacy["base"], privacy["base_exists"])

    def prepare(self, args):
        require(not self.path.exists(), "transaction already exists; use status or rollback")
        require(sys.platform == "linux" and os.getuid() != 0, "run as the Linux desktop user, without sudo")
        config = Path(args.config).resolve(strict=True)
        source = Path(args.source).resolve(strict=True)
        require(not config.parent.is_relative_to(source) and not source.is_relative_to(config.parent), "source and destination must be separate")
        require(not self.state.resolve().is_relative_to(source), "transaction journal must be outside the original Magpie directory")
        migration = read_json(config.parent / "migration.json")
        require(migration.get("source") == str(source) and not migration.get("cutover_complete"), "candidate must be a fresh Magpie migration")
        require(not migration.get("unresolved") and not migration.get("limitations"), "migration has unresolved models or compatibility limitations")
        values = config_values(config)
        server = values["server"]
        require(server.get("listen", "").rsplit(":", 1)[-1] == "3425", "candidate must preserve TCP port 3425")
        require(server.get("trust_loopback") == "true", "migration requires explicit loopback trust")
        unix = server.get("unix_socket", "none")
        require(unix != "none", "candidate must provide its Unix socket")
        runtime = os.environ.get("XDG_RUNTIME_DIR")
        require(runtime and Path(runtime).is_dir(), "a live user session runtime directory is required")
        unix = str(Path(runtime) / "zcalld/gateway.sock") if unix == "auto" else str((config.parent / unix).resolve())
        old = process_identity(args.magpie_pid, launch=True)
        require(old and old["uid"] == os.getuid() and Path(old["exe"]).name == "magpie", "PID is not this user's native Magpie")
        require(owns_listener(old["pid"], 3425), "the verified Magpie process does not own TCP 3425")
        expected = catalog(read_json(config.parent / server.get("catalog_file", "model-catalog.json")))
        old_models = catalog(self.ops.get(3425, "/v1/models"))
        require(digest(expected) == digest(old_models), "staged catalog differs from live Magpie metadata; remigrate first")
        exclusions = []
        if args.external_owners:
            owners = read_json(args.external_owners)
            require(owners.get("boot_id") == Path("/proc/sys/kernel/random/boot_id").read_text().strip(), "external owner manifest is from another boot")
            exclusions = owners.get("holders", [])
        new_unit = self.ops.unit(UNIT)
        require(new_unit.get("LoadState") == "loaded", "install the packaged zcalld user unit before preparing")
        require(new_unit.get("ActiveState") in ("inactive", "failed"), "candidate service is already running")
        old_unit = self.ops.unit(args.old_unit) if args.old_unit else None
        if old_unit:
            require(old_unit.get("MainPID") == str(old["pid"]), "old unit does not own the selected Magpie main process")
        snapshot = Path(migration["snapshot"])
        topology = {}
        for name in ("providers.json", "plugins.json", "caller-keys.json", "library.json"):
            if (source / name).exists():
                require((snapshot / name).exists() and file_hash(source / name) == file_hash(snapshot / name), "Magpie configuration changed since migration; remigrate before cutover")
                topology[name] = file_hash(source / name)
        require(settings_fingerprint(source) == settings_fingerprint(snapshot), "Magpie routing/privacy settings changed since migration")
        auth_path = config.parent / "extensions/plugin-auth.json"
        staged = read_json(auth_path)
        initial = read_json(snapshot / "plugin-auth.json", {})
        require(all(not key.startswith("builtin:") for key in initial), "source uses reserved builtin account identifiers")
        require({key: value for key, value in staged.items() if not key.startswith("builtin:")} == initial,
                "community credentials in the candidate have changed; do not overwrite an active copy")
        for path in (config.parent / "extensions").glob("plugin-auth.json*.lock"):
            raise CutoverError("candidate credential host is active or locked")
        for name in ("host.json",):
            host = read_json(config.parent / "extensions" / name)
            for key in ("bun", "host", "pi"):
                regular(Path(host[key]).resolve(strict=True))
        privacy = read_json(config.parent / "privacy/plugin.json")
        regular(Path(privacy["worker"]).resolve(strict=True))
        privacy_state = self.prepare_privacy(source, config.parent, snapshot)
        for binary in (args.binary, args.pluginctl):
            require(Path(binary).is_absolute() and os.access(binary, os.X_OK), "installed executables must be absolute and executable")
        require(is_prefix(config.parent / "usage.jsonl", source / "usage.jsonl"), "candidate usage is not an untouched prefix of Magpie usage")
        mcp_required = required_mcp(source)
        # This still addresses the verified old Magpie listener. No candidate
        # daemon or Bun worker is started until apply has drained that process.
        mcp = self.ops.mcp(3425) if mcp_required else None
        self.data = {"version": 1, "id": uuid.uuid4().hex[:16], "phase": "preparing",
                     "config": str(config), "source": str(source), "binary": args.binary,
                     "pluginctl": args.pluginctl, "timeout": args.timeout, "unix_socket": unix,
                     "old_process": old, "exclusions": exclusions, "old_unit": args.old_unit,
                     "old_unit_state": old_unit, "new_unit_state": new_unit,
                     "old_binary_hash": file_hash(old["exe"]), "new_binary_hash": file_hash(args.binary),
                     "helper_hash": file_hash(args.pluginctl), "topology": topology,
                     "settings_hash": settings_fingerprint(source), "community_keys": sorted(initial),
                     "community_staged": {key: digest(value) for key, value in initial.items()},
                     "catalog_hash": digest(expected), "catalog_count": len(expected),
                     "mcp_required": mcp_required, "mcp_hash": digest(mcp) if mcp else None,
                     "mcp_tool_count": len(mcp["tools"]) if mcp else None,
                     "config_hash": file_hash(config), "changes": {}, "new_start_attempted": False,
                     "privacy": privacy_state}
        self.save_recovery()
        self.save()
        save_json(self.state / "expected-models.json", {"object": "list", "data": list(expected.values())})
        if mcp is not None:
            save_json(self.state / "expected-mcp.json", mcp)
        self.ops.run([args.binary, "check", "--config", str(config)], "validate staged configuration")
        self.save("prepared")
        self.event("prepared")

    def check_prepared(self):
        require(self.data.get("phase") == "prepared", "apply requires a prepared transaction; recover an interrupted switch with rollback")
        require(file_hash(self.data["binary"]) == self.data["new_binary_hash"] and
                file_hash(self.data["pluginctl"]) == self.data["helper_hash"], "installed binaries changed after prepare")
        require(file_hash(self.data["config"]) == self.data["config_hash"], "candidate configuration changed after prepare")
        values = config_values(self.data["config"])
        expected = read_json(self.candidate() / values["server"].get("catalog_file", "model-catalog.json"))
        require(digest(catalog(expected)) == self.data["catalog_hash"], "candidate catalog changed after prepare")
        source = Path(self.data["source"])
        require(all(file_hash(source / name) == value for name, value in self.data["topology"].items()), "source topology changed after prepare")
        require(settings_fingerprint(source) == self.data["settings_hash"], "source privacy settings changed after prepare")
        self.check_privacy(staged=True)
        require(required_mcp(source) == self.data.get("mcp_required", False),
                "retained MCP endpoint changed after preparation; prepare a fresh transaction")
        if self.data.get("mcp_required"):
            self.expected_mcp()
        require(same_process(self.data["old_process"]) and owns_listener(self.data["old_process"]["pid"], 3425), "old gateway identity or port ownership changed")
        require(self.ops.unit(UNIT).get("ActiveState") in ("inactive", "failed"), "new service started outside the transaction")

    def sync_in(self):
        source, target = Path(self.data["source"]), self.candidate()
        self.sync_privacy_in()
        auth_path = target / "extensions/plugin-auth.json"
        with auth_lock(source / "plugin-auth.json"):
            incoming = read_json(source / "plugin-auth.json", {})
            require(set(incoming) == set(self.data["community_keys"]), "source community account membership changed; remigrate")
            with auth_lock(auth_path):
                current = read_json(auth_path)
                require(all(digest(current.get(key)) == value for key, value in self.data["community_staged"].items()), "candidate community auth was changed by another owner")
                self.data["community_source"] = {key: digest(value) for key, value in incoming.items()}
                self.save()
                current.update(incoming)
                save_json(auth_path, current)
        self.helper("import-builtin", "--source", str(source))
        current = read_json(auth_path)
        mapping = read_json(target / "extensions/builtin-auth-map.json")
        for entry in mapping.get("accounts", []):
            native = [item for item in entry.get("sources", []) if item.get("kind") == "codex"]
            if native:
                value = current.get(entry["key"], {})
                require(value.get("refreshOwner") == "external" and value.get("sourceAuthFile") in
                        [item["path"] for item in native], "active Codex ownership was not retained")
        old_usage, new_usage = source / "usage.jsonl", target / "usage.jsonl"
        self.data["usage_source_existed"] = old_usage.exists()
        if not old_usage.exists():
            require(not new_usage.exists() or new_usage.stat().st_size == 0, "unexpected staged usage")
            atomic(new_usage, b"")
            self.data["usage_base_size"] = 0
            self.data["usage_base_hash"] = hashlib.sha256(b"").hexdigest()
        else:
            complete_jsonl(old_usage)
            require(is_prefix(new_usage, old_usage), "usage changed outside the original gateway")
            copy_file(old_usage, new_usage)
            self.data["usage_base_size"] = old_usage.stat().st_size
            self.data["usage_base_hash"] = file_hash(old_usage)
        for name in ("logins.json", "quotas.json", "quota-history.json"):
            if (source / name).exists():
                copy_file(source / name, target / name)
        self.save("synchronized")
        self.ops.run([self.data["binary"], "check", "--config", self.data["config"]], "validate final credentials")

    def expected_mcp(self):
        expected = read_json(self.state / "expected-mcp.json")
        require(digest(expected) == self.data.get("mcp_hash"),
                "private MCP baseline changed after preparation")
        return expected

    def verify_mcp(self, unix=None):
        if self.data.get("mcp_required"):
            expected = self.expected_mcp()
            current = self.ops.mcp(3425, unix=unix, protocol=expected["protocolVersion"])
            require(current.get("protocolVersion") == expected["protocolVersion"],
                    "served MCP protocol differs from the original endpoint")
            require(digest(mcp_catalog(current.get("tools"))) == digest(mcp_catalog(expected["tools"])),
                    "served MCP tool metadata differs from the original endpoint")

    def verify_new(self):
        expected = read_json(self.state / "expected-models.json")
        until = time.monotonic() + 30
        while True:
            try:
                status = self.ops.unit(UNIT)
                require(status.get("ActiveState") == "active", "new service is not active")
                pid = int(status.get("MainPID", "0"))
                require(pid > 0 and owns_listener(pid, 3425), "new service does not own TCP 3425")
                info = Path(self.data["unix_socket"]).lstat()
                require(stat.S_ISSOCK(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600 and info.st_uid == os.getuid(), "Unix socket is not private")
                for unix in (None, self.data["unix_socket"]):
                    require(self.ops.get(3425, "/healthz", unix).get("status") == "ok", "gateway is not healthy")
                    require(digest(catalog(self.ops.get(3425, "/v1/models", unix))) == digest(catalog(expected)), "served model metadata differs from the staged catalog")
                break
            except (OSError, ValueError, CutoverError, http.client.HTTPException):
                require(time.monotonic() < until, "new service failed TCP/Unix/catalog verification")
                time.sleep(0.2)
        # Metadata rejection or mismatch is terminal once the listener is ready.
        # Let apply use the existing drain/sync/restore rollback transaction.
        for unix in (None, self.data["unix_socket"]):
            self.verify_mcp(unix)

    def apply(self):
        self.check_prepared()
        self.save("pausing_old")
        try:
            self.event("pausing_old")
            config_home = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
            self.change(config_home / "autostart/magpie.desktop", b"[Desktop Entry]\nType=Application\nName=Magpie\nHidden=true\n")
            if self.data.get("old_unit"):
                old_override = config_home / "systemd/user" / (self.data["old_unit"] + ".d/95-zcalld-cutover.conf")
                self.change(old_override, b"[Service]\nRestart=no\nKillMode=process\nSendSIGKILL=no\n")
                self.ops.control("daemon-reload")
                self.ops.control("disable", self.data["old_unit"])
            self.data["old_tree"] = process_tree(self.data["old_process"], self.data["exclusions"])
            self.save()
            self.ops.terminate(self.data["old_process"])
            self.ops.drain(self.data["old_tree"], self.data["timeout"])
            require(not listener_inodes(3425), "TCP 3425 was reclaimed by another process; no process was killed")
            self.save("old_stopped")
            self.event("synchronizing_final_state")
            self.sync_in()
            override = ("[Service]\nExecStart=\nExecStart=:" + unit_word(self.data["binary"]) +
                        " serve --config " + unit_word(self.data["config"]) +
                        "\nKillMode=mixed\nTimeoutStopSec=60s\nSendSIGKILL=no\n")
            self.change(config_home / "systemd/user/zcalld.service.d/95-cutover.conf", override.encode())
            self.ops.control("daemon-reload")
            self.data["new_start_attempted"] = True
            self.save("starting_new")
            self.ops.control("start", UNIT)
            self.verify_new()
            self.ops.control("enable", UNIT)
            migration_path = self.candidate() / "migration.json"
            migration = read_json(migration_path)
            migration["cutover_complete"] = True
            migration["cutover_journal"] = str(self.path)
            save_json(migration_path, migration)
            self.save("active")
            self.event("active")
        except Exception:
            self.event("switch_failed_rolling_back")
            self.rollback()
            raise

    def sync_back(self):
        source, target = Path(self.data["source"]), self.candidate()
        self.sync_privacy_back()
        original = read_json(source / "plugin-auth.json", {})
        current = read_json(target / "extensions/plugin-auth.json")
        result = dict(original)
        for key, before in self.data.get("community_source", {}).items():
            require(key in current, "a migrated community account was removed; recovery needs review")
            require(digest(original.get(key)) in (before, digest(current[key])), "original community auth changed under another owner; refusing overwrite")
            result[key] = current[key]
        # Validate the community side before builtin.Sync performs its own full
        # prevalidation. Sync skips externally managed native Codex credentials.
        self.helper("sync-auth", "--to-magpie", str(source))
        with auth_lock(source / "plugin-auth.json"):
            require(read_json(source / "plugin-auth.json", {}) == original, "original community auth changed during rollback")
            save_json(source / "plugin-auth.json", result)
        self.restore_usage()

    def restore_usage(self):
        old = Path(self.data["source"]) / "usage.jsonl"
        new = self.candidate() / "usage.jsonl"
        current_hash = file_hash(old) if old.exists() else hashlib.sha256(b"").hexdigest()
        if self.data.get("usage_rollback_hash") == current_hash:
            return
        size = regular(old).st_size if old.exists() else 0
        require(size == self.data["usage_base_size"] and current_hash == self.data["usage_base_hash"], "original usage changed after cutover; refusing an ambiguous merge")
        require(is_prefix(old, new), "new usage history no longer preserves its imported prefix")
        complete_jsonl(new)
        prepared = self.state / "rollback-usage.jsonl"
        values = config_values(self.data["config"])
        with replacement(prepared) as outgoing:
            if old.exists():
                with old.open("rb") as incoming:
                    shutil.copyfileobj(incoming, outgoing, BLOCK)
            with new.open("rb") as tail:
                tail.seek(self.data["usage_base_size"])
                while True:
                    offset = tail.tell()
                    line = tail.readline(BLOCK + 1)
                    if not line:
                        break
                    require(len(line) <= BLOCK and line.endswith(b"\n"), "new usage record is incomplete or too large")
                    value = json.loads(line)
                    row = magpie_usage(value, values)
                    row["zcalld_cutover"] = {"transaction": self.data["id"], "offset": offset}
                    outgoing.write(encoded(row) + b"\n")
        self.data["usage_rollback_hash"] = file_hash(prepared)
        self.save()
        copy_file(prepared, old)

    def rollback(self):
        require(self.data.get("version") == 1, "unknown cutover journal")
        if self.data.get("phase") == "rolled_back":
            self.event("rolled_back")
            return
        try:
            self.save("rollback_draining")
            if self.data.get("new_start_attempted") and not self.data.get("rollback_synced"):
                tree = self.ops.service_processes(UNIT)
                self.ops.control("stop", UNIT, self.data["timeout"] + 5)
                self.ops.drain(tree, self.data["timeout"])
                require(not self.ops.service_processes(UNIT), "a new-service credential worker is still running")
                require(not listener_inodes(3425), "port is still occupied after new-service drain")
                self.save("rollback_syncing")
                self.sync_back()
                self.data["rollback_synced"] = True
                self.save()
            old_tree = self.data.get("old_tree", [])
            if not same_process(self.data["old_process"]):
                self.ops.drain(old_tree, self.data["timeout"])
            self.restore_changes()
            self.ops.control("daemon-reload")
            was_enabled = self.data["new_unit_state"].get("UnitFileState", "").startswith("enabled")
            self.ops.control("enable" if was_enabled else "disable", UNIT)
            if self.data.get("old_unit"):
                old_enabled = self.data["old_unit_state"].get("UnitFileState", "").startswith("enabled")
                self.ops.control("enable" if old_enabled else "disable", self.data["old_unit"])
            if not same_process(self.data["old_process"]) and not self.ops.restored_old(self.data):
                require(not listener_inodes(3425), "cannot restart Magpie while the port has another owner")
                self.data["old_restart_attempted"] = True
                self.save()
                self.ops.restart_old(self.data)
            until = time.monotonic() + 30
            expected = set(catalog(read_json(self.state / "expected-models.json")))
            while True:
                try:
                    require(set(catalog(self.ops.get(3425, "/v1/models"))) == expected, "old model IDs were not restored")
                    self.verify_mcp()
                    break
                except (OSError, ValueError, CutoverError, http.client.HTTPException):
                    require(time.monotonic() < until, "Magpie did not recover its model/MCP metadata")
                    time.sleep(0.2)
            migration_path = self.candidate() / "migration.json"
            migration = read_json(migration_path)
            migration["cutover_complete"] = False
            save_json(migration_path, migration)
            self.save("rolled_back")
            self.event("rolled_back")
        except Exception:
            self.save("recovery_required")
            self.event("recovery_required")
            raise


@contextlib.contextmanager
def auth_lock(path):
    path = Path(path)
    require(not list(path.parent.glob(path.name + ".renew-*.lock")), "credential renewal is still in progress")
    lock = Path(str(path) + ".lock")
    fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    owned = os.fstat(fd)
    try:
        require(not list(path.parent.glob(path.name + ".renew-*.lock")), "credential renewal began during handoff")
        yield
    finally:
        os.close(fd)
        with contextlib.suppress(FileNotFoundError):
            current = lock.lstat()
            if (current.st_dev, current.st_ino) == (owned.st_dev, owned.st_ino):
                lock.unlink()


def magpie_usage(value, config):
    require(isinstance(value, dict), "usage record must be an object")
    if "t" in value:
        return dict(value)
    require(isinstance(value.get("time"), int) and isinstance(value.get("model"), str), "unknown new usage format")
    selected = value.get("upstream", value["model"])
    section = "route " + selected
    wire = config[section].get("model", selected) if section in config else selected
    timestamp = datetime.datetime.fromtimestamp(value["time"], datetime.timezone.utc).isoformat().replace("+00:00", "Z")
    return {"t": timestamp, "agent": "zcalld", "provider": selected.split("/", 1)[0] if "/" in selected else "zcalld",
            "model": wire, "req": value["model"], "in": value.get("input_tokens", 0),
            "out": value.get("output_tokens", 0), "ms": value.get("elapsed_ms", 0),
            "status": value.get("status", 0), "cache_read": 0, "cache_write": 0,
            "cache_unknown": True, "zcalld_record": value}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    prepare = sub.add_parser("prepare", help="validate a staged migration and save a private recovery journal")
    prepare.add_argument("--config", required=True)
    prepare.add_argument("--source", default=str(Path.home() / ".config/magpie"))
    prepare.add_argument("--binary", default="/usr/bin/zcalld")
    prepare.add_argument("--pluginctl", default="/usr/libexec/zcalld/zcalld-pluginctl")
    prepare.add_argument("--magpie-pid", type=int, required=True)
    prepare.add_argument("--old-unit")
    prepare.add_argument("--external-owners")
    prepare.add_argument("--timeout", type=int, default=90)
    for command in (prepare, sub.add_parser("apply"), sub.add_parser("rollback"), sub.add_parser("status")):
        command.add_argument("--state", required=True, help="absolute private transaction directory")
    args = parser.parse_args()
    require(Path(args.state).is_absolute(), "--state must be absolute")
    if getattr(args, "old_unit", None):
        require(re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", args.old_unit) is not None and args.old_unit != UNIT, "invalid old service unit")
    if args.action == "prepare":
        require(60 <= args.timeout <= 600, "--timeout must be between 60 and 600 seconds")
    os.umask(0o077)
    state = Path(args.state)
    if args.action == "prepare":
        state.mkdir(parents=True, mode=0o700, exist_ok=False)
    require(state.is_dir() and not state.is_symlink() and stat.S_IMODE(state.stat().st_mode) == 0o700 and state.stat().st_uid == os.getuid(), "transaction directory must be owned by this user and mode 0700")
    with (state / ".lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        transaction = Transaction(state)
        if args.action == "prepare":
            transaction.prepare(args)
        elif args.action == "apply":
            transaction.apply()
        elif args.action == "rollback":
            transaction.rollback()
        else:
            print(json.dumps({"phase": transaction.data.get("phase"),
                              "catalog_models": transaction.data.get("catalog_count"),
                              "mcp_required": transaction.data.get("mcp_required", False),
                              "mcp_tools": transaction.data.get("mcp_tool_count"),
                              "journal": str(transaction.path),
                              "recovery": transaction.data.get("recovery_file"),
                              "recovery_sha256": transaction.data.get("recovery_sha256")}))


if __name__ == "__main__":
    try:
        main()
    except (CutoverError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"ok": False, "error": str(error)}), file=sys.stderr)
        sys.exit(1)
