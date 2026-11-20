#!/usr/bin/env python3
"""Real Codex transport regression, with no credentials or external networking.

Linux-only explicit fixture: bubblewrap creates a new network namespace and mounts
only the selected executables, this script, /usr, and a new temporary fixture tree.
It never reads the invoking user's Codex configuration. No model is invoked: every
response comes from the local fixed SSE server. It is not a routine CI dependency.

Usage:
  python3 scripts/test-codex-transport.py --codex /path/to/codex \
      --binary /path/to/zcalld --output /private/receipt.json
Omit --binary to run only the HTTP 400/426 control pair.
"""
from __future__ import annotations

import argparse
import base64
import ctypes
import datetime
import gzip
import io
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

MODEL = "zcalld-transport-fixture"
PROMPT = "Reply exactly OK. Do not use any tools."
MAX_REQUEST = 2 * 1024 * 1024
MAX_RESPONSE = 64 * 1024
CASE_TIMEOUT = 25
SOURCE_REF = "rust-v0.170.0"
SOURCE_URL = (
    "https://github.com/openai/codex/blob/"
    + SOURCE_REF
    + "/codex-rs/core/src/client.rs"
)


def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def decode_body(body, encoding):
    if encoding in ("", "identity"):
        return body
    if encoding == "gzip":
        with gzip.GzipFile(fileobj=io.BytesIO(body)) as stream:
            result = stream.read(MAX_REQUEST + 1)
        if len(result) > MAX_REQUEST:
            raise ValueError("decompressed fixture request too large")
        return result
    if encoding == "zstd":
        lib = ctypes.CDLL("libzstd.so.1")
        lib.ZSTD_decompress.argtypes = [
            ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t
        ]
        lib.ZSTD_decompress.restype = ctypes.c_size_t
        lib.ZSTD_isError.argtypes = [ctypes.c_size_t]
        lib.ZSTD_isError.restype = ctypes.c_uint
        output = ctypes.create_string_buffer(MAX_REQUEST)
        source = ctypes.create_string_buffer(body)
        count = lib.ZSTD_decompress(output, MAX_REQUEST, source, len(body))
        if lib.ZSTD_isError(count) or count > MAX_REQUEST:
            raise ValueError("invalid or oversized fixture zstd request")
        return output.raw[:count]
    raise ValueError("unknown fixture content encoding")


def fake_auth(root, shape):
    path = root / "auth.json"
    if shape == "api_key":
        data = {"auth_mode": "apikey", "OPENAI_API_KEY": "fixture-api-key"}
    else:
        def token(claims):
            def part(value):
                return base64.urlsafe_b64encode(json.dumps(value).encode()).rstrip(b"=").decode()
            return part({"alg": "none"}) + "." + part(claims) + ".fixture"
        claims = {
            "exp": int(time.time()) + 86400, "email": "fixture@example.invalid",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "fixture-account",
                "chatgpt_user_id": "fixture-user", "chatgpt_plan_type": "plus",
            },
        }
        jwt = token(claims)
        data = {
            "auth_mode": "chatgpt", "OPENAI_API_KEY": None,
            "tokens": {"id_token": jwt, "access_token": jwt,
                       "refresh_token": "fixture-refresh", "account_id": "fixture-account"},
            "last_refresh": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        }
    path.write_text(json.dumps(data))
    path.chmod(0o600)


def event_bytes():
    message = {
        "type": "message", "role": "assistant", "id": "msg_fixture",
        "status": "completed",
        "content": [{"type": "output_text", "text": "OK", "annotations": []}],
    }
    events = [
        {"type": "response.created", "response": {
            "id": "resp_fixture", "object": "response", "created_at": 1,
            "status": "in_progress", "model": MODEL, "output": []}},
        {"type": "response.output_item.done", "output_index": 0, "item": message},
        {"type": "response.completed", "response": {
            "id": "resp_fixture", "object": "response", "created_at": 1,
            "status": "completed", "model": MODEL, "output": [message],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}},
    ]
    return b"".join(
        ("event: " + event["type"] + "\ndata: "
         + json.dumps(event, separators=(",", ":")) + "\n\n").encode()
        for event in events
    )


class MockServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, mode, upstream=None):
        super().__init__(("127.0.0.1", 0), MockHandler)
        self.mode = mode
        self.upstream = upstream
        self.records = []
        self.errors = []
        self.mutex = threading.Lock()
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)

    def record(self, value):
        with self.mutex:
            self.records.append(value)

    @property
    def port(self):
        return self.server_address[1]


class MockHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, body, content_type="application/json", headers=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        for name, value in (headers or []):
            if name.lower() not in {
                "connection", "content-length", "transfer-encoding",
                "content-type", "server", "date",
            }:
                self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self):
        self.handle_request()

    def do_POST(self):
        self.handle_request()

    def handle_request(self):
        upgrade = self.headers.get("Upgrade", "").lower() == "websocket"
        encoding = self.headers.get("Content-Encoding", "").lower()
        record = {
            "method": self.command, "path": self.path, "websocket": upgrade,
            "content_encoding": encoding if encoding in ("", "identity", "gzip", "zstd") else "other",
            "authorization_present": bool(self.headers.get("Authorization")),
        }
        try:
            if self.path != "/v1/responses":
                record["status"] = 404
                self.server.record(record)
                self.reply(404, b"{}")
                return
            if self.headers.get("Transfer-Encoding"):
                raise ValueError("unexpected fixture request transfer encoding")
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= MAX_REQUEST:
                raise ValueError("fixture request exceeded its bound")
            body = self.rfile.read(length)
            if len(body) != length:
                raise ValueError("short fixture request")
            if self.server.mode == "proxy":
                conn = http.client.HTTPConnection(
                    "127.0.0.1", self.server.upstream, timeout=10
                )
                try:
                    headers = dict(self.headers.items())
                    # Preserve actual Codex handshake fields; only route to our
                    # isolated gateway. No Authorization value is ever recorded.
                    conn.request(self.command, self.path, body=body, headers=headers)
                    response = conn.getresponse()
                    output = response.read(MAX_RESPONSE + 1)
                    if len(output) > MAX_RESPONSE:
                        raise ValueError("fixture response exceeded its bound")
                    record["status"] = response.status
                    self.server.record(record)
                    self.reply(
                        response.status, output,
                        response.getheader("Content-Type", "application/json"),
                        response.getheaders(),
                    )
                finally:
                    conn.close()
                return
            if self.command == "GET" and upgrade:
                status = 400 if self.server.mode == "400" else 426
                record["status"] = status
                self.server.record(record)
                self.reply(status, b'{"error":{"message":"fixture transport"}}',
                           headers=[("Upgrade", "websocket")])
                return
            if self.command != "POST" or upgrade:
                raise ValueError("unexpected fixture request")
            request = json.loads(decode_body(body, encoding))
            record["model_is_fixture"] = request.get("model") == MODEL
            record["stream"] = request.get("stream") is True
            if not record["model_is_fixture"] or not record["stream"]:
                raise ValueError("unexpected model or stream flag")
            record["status"] = 200
            self.server.record(record)
            self.reply(200, event_bytes(), "text/event-stream")
        except Exception as exc:
            self.server.errors.append(type(exc).__name__)
            try:
                self.reply(500, b'{"error":{"message":"fixture failure"}}')
            except (BrokenPipeError, ConnectionResetError):
                pass


def private_environment(root):
    env = {
        "PATH": "/usr/bin:/bin", "HOME": str(root / "home"),
        "CODEX_HOME": str(root / "codex"), "LANG": "C.UTF-8",
        "XDG_CONFIG_HOME": str(root / "xdg/config"),
        "XDG_CACHE_HOME": str(root / "xdg/cache"),
        "XDG_DATA_HOME": str(root / "xdg/data"),
        "XDG_STATE_HOME": str(root / "xdg/state"),
        "XDG_RUNTIME_DIR": str(root / "runtime"),
        "TMPDIR": "/tmp",
    }
    for key in ("HOME", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
                "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
        Path(env[key]).mkdir(parents=True, mode=0o700, exist_ok=True)
    return env


def codex_case(name, port, expected, root, codex, auth_shape="custom"):
    case = root / name
    case.mkdir(mode=0o700)
    work = case / "work"
    work.mkdir(mode=0o700)
    env = private_environment(case)
    instructions = case / "instructions.txt"
    instructions.write_text("Return the assistant message. Do not call tools.\n")
    builtin = auth_shape != "custom"
    provider = "openai" if builtin else "fixture"
    config = (
        'model = "' + MODEL + '"\n'
        'model_provider = "' + provider + '"\n'
        'approval_policy = "never"\n'
        'sandbox_mode = "read-only"\n'
        'web_search = "disabled"\n'
        'model_reasoning_summary = "none"\n'
        'cli_auth_credentials_store = "file"\n'
        'model_instructions_file = ' + json.dumps(str(instructions)) + '\n'
    )
    if builtin:
        config += 'openai_base_url = "http://127.0.0.1:' + str(port) + '/v1"\n'
        fake_auth(Path(env["CODEX_HOME"]), auth_shape)
    config += (
        '[analytics]\nenabled = false\n'
        '[history]\npersistence = "none"\n'
        '[features]\nshell_tool = false\napps = false\nhooks = false\n'
        'memory_tool = false\nmulti_agent = false\nskip_host_skill_discovery = true\n'
        'enable_request_compression = true\nuse_agent_identity = false\n'
    )
    if not builtin:
        config += (
            '[model_providers.fixture]\nname = "Isolated transport fixture"\n'
            'base_url = "http://127.0.0.1:' + str(port) + '/v1"\n'
            'wire_api = "responses"\nrequires_openai_auth = false\n'
            'supports_websockets = true\nrequest_max_retries = 0\n'
            'stream_max_retries = 0\nstream_idle_timeout_ms = 5000\n'
            'websocket_connect_timeout_ms = 2000\n'
        )
    Path(env["CODEX_HOME"], "config.toml").write_text(config)
    last = case / "last.txt"
    command = [
        codex, "exec", "--skip-git-repo-check", "--ephemeral", "--ignore-rules",
        "--json", "--color", "never", "--cd", str(work),
        "--output-last-message", str(last), PROMPT,
    ]
    start = time.monotonic()
    process = subprocess.Popen(
        command, env=env, cwd=work, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=CASE_TIMEOUT)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
    # Debug text remains inside the fresh fixture; the published receipt excludes it.
    (case / "stdout.txt").write_bytes(stdout[:256 * 1024])
    (case / "stderr.txt").write_bytes(stderr[:64 * 1024])
    events = []
    for line in stdout.splitlines():
        try:
            value = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        if isinstance(value, dict):
            events.append(value)
    completed = any(item.get("type") == "turn.completed" for item in events)
    tool_output = any(
        item.get("type") in ("item.started", "item.completed")
        and item.get("item", {}).get("type") in (
            "command_execution", "mcp_tool_call", "web_search", "file_change"
        )
        for item in events
    )
    exact = last.is_file() and last.read_text().strip() == "OK"
    success = not timed_out and not tool_output and (
        (process.returncode == 0 and completed and exact) if expected
        else (process.returncode != 0 and not completed and not exact)
    )
    return {
        "case": name, "auth_shape": auth_shape, "passed": success, "exit_code": process.returncode,
        "expected_completion": expected,
        "retry_policy": "builtin_defaults" if builtin else "http_0_stream_0",
        "timed_out": timed_out, "completed": completed, "exact_ok": exact,
        "tool_outputs": tool_output,
        "elapsed_seconds": round(time.monotonic() - start, 3),
        "event_types": sorted({str(item.get("type")) for item in events}),
        "stderr_bytes": len(stderr),
    }


def choose_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def start_gateway(binary, upstream_port, root):
    port = choose_port()
    config = root / "gateway.conf"
    config.write_text(
        "[server]\nlisten=127.0.0.1:" + str(port) + "\nworkers=2\nqueue=8\n"
        "max_body_bytes=2097152\nmax_response_bytes=65536\n"
        "request_timeout_ms=10000\nupstream_timeout_ms=10000\n"
        "connect_timeout_ms=1000\n"
        "[provider fixture]\nbase_url=http://127.0.0.1:"
        + str(upstream_port) + "/v1\nprotocol=responses\n"
        "[route " + MODEL + "]\nprovider=fixture\nmodel=" + MODEL + "\n"
    )
    env = private_environment(root / "gateway-env")
    stdout = open(root / "gateway.stdout", "wb")
    stderr = open(root / "gateway.stderr", "wb")
    process = subprocess.Popen(
        [binary, "serve", "--config", str(config)], env=env,
        stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
    )
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("isolated gateway exited before ready")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                    return process, port, stdout, stderr
            except OSError:
                time.sleep(0.02)
        raise RuntimeError("isolated gateway readiness deadline")
    except Exception:
        process.terminate()
        process.wait(timeout=5)
        stdout.close()
        stderr.close()
        raise


def inner(args):
    root = Path("/fixture")
    net_inode = os.stat("/proc/self/ns/net").st_ino
    if net_inode == int(args.host_net_inode):
        raise RuntimeError("fixture must run in a separate network namespace")
    version = subprocess.run(
        ["/codex", "--version"], capture_output=True, text=True, timeout=10,
        env=private_environment(root / "version"),
    )
    if version.returncode != 0 or version.stdout.strip() != "codex-cli 0.170.0":
        raise RuntimeError("fixture requires the reviewed Codex 0.170.0")
    cases = []
    for auth_shape, status in [
        ("custom", "400"), ("custom", "426"), ("api_key", "426"),
        ("chatgpt", "400"), ("chatgpt", "426"),
    ]:
        with MockServer(status) as server:
            result = codex_case("mock_" + auth_shape + "_" + status, server.port,
                                status == "426", root, "/codex", auth_shape)
            result["http"] = server.records
            gets = [r for r in server.records if r["method"] == "GET" and r["websocket"]]
            posts = [r for r in server.records if r["method"] == "POST"]
            result["passed"] &= bool(gets) and not server.errors
            if status == "400":
                result["passed"] &= not posts and all(r["status"] == 400 for r in gets)
            else:
                result["passed"] &= len(posts) == 1 and all(r["status"] == 426 for r in gets)
                result["passed"] &= bool(posts) and posts[0]["status"] == 200
                result["passed"] &= bool(posts) and posts[0]["content_encoding"] == (
                    "zstd" if auth_shape == "chatgpt" else ""
                )
            cases.append(result)
    if args.binary:
        for auth_shape in ("api_key", "chatgpt"):
            with MockServer("upstream") as upstream:
                process, port, stdout, stderr = start_gateway(
                    "/zcalld", upstream.port, root
                )
                try:
                    with MockServer("proxy", port) as front:
                        result = codex_case(
                            "gateway_" + auth_shape + "_426", front.port,
                            True, root, "/codex", auth_shape
                        )
                        result["http"] = front.records
                        result["upstream_http"] = upstream.records
                        gets = [
                            r for r in front.records
                            if r["method"] == "GET" and r["websocket"]
                        ]
                        posts = [r for r in front.records if r["method"] == "POST"]
                        result["passed"] &= (
                            bool(gets) and all(r["status"] == 426 for r in gets)
                            and len(posts) == 1 and posts[0]["status"] == 200
                            and posts[0]["content_encoding"] == (
                                "zstd" if auth_shape == "chatgpt" else ""
                            )
                            and len(upstream.records) == 1
                            and upstream.records[0]["method"] == "POST"
                            and upstream.records[0]["status"] == 200
                            and upstream.records[0]["content_encoding"] in ("", "identity")
                            and not upstream.records[0]["authorization_present"]
                            and not front.errors and not upstream.errors
                        )
                        cases.append(result)
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                    stdout.close()
                    stderr.close()
    result = {
        "schema_version": 1, "kind": "isolated_real_codex_transport",
        "codex_version": version.stdout.strip(), "source_ref": SOURCE_REF,
        "source_url": SOURCE_URL,
        "separate_network_namespace": True, "inherited_credentials": False,
        "real_model_requests": 0, "fixture_replays": 0,
        "passed": all(case["passed"] for case in cases),
        "cases": cases,
    }
    (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if result["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--binary")
    parser.add_argument("--output")
    parser.add_argument("--keep-debug", action="store_true",
                        help="retain only the generated isolated fixture files on failure")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--host-net-inode", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        return inner(args)
    if not sys.platform.startswith("linux") or not shutil.which("bwrap"):
        parser.error("this explicit fixture requires Linux bubblewrap")
    if not args.codex:
        parser.error("provide --codex with the reviewed installed client")
    codex = Path(args.codex).resolve(strict=True)
    binary = Path(args.binary).resolve(strict=True) if args.binary else None
    if args.output:
        output = Path(args.output).resolve()
        output.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        if output.exists():
            parser.error("--output must be a new receipt path")
    else:
        output = None
    root = Path(tempfile.mkdtemp(prefix="zcalld-codex-", dir="/var/tmp"))
    os.chmod(root, 0o700)
    command = [
        shutil.which("bwrap"), "--unshare-all", "--die-with-parent", "--new-session",
        "--ro-bind", "/usr", "/usr",
        "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib",
        "--symlink", "usr/lib", "/lib64",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--dir", "/etc", "--dir", "/home",
        "--bind", str(root), "/fixture",
        "--ro-bind", str(Path(__file__).resolve()), "/fixture-run.py",
        "--ro-bind", str(codex), "/codex",
    ]
    if binary:
        command += ["--ro-bind", str(binary), "/zcalld"]
    command += [
        "--clearenv", "--setenv", "PATH", "/usr/bin:/bin",
        "--setenv", "HOME", "/fixture/home", "--setenv", "LANG", "C.UTF-8",
        "--chdir", "/fixture", "/usr/bin/python3", "/fixture-run.py",
        "--inside", "--host-net-inode", str(os.stat("/proc/self/ns/net").st_ino),
    ]
    if binary:
        command += ["--binary", "/zcalld"]
    try:
        execution = subprocess.run(command, capture_output=True, timeout=110)
        result_path = root / "result.json"
        if not result_path.is_file():
            sys.stderr.write(execution.stderr.decode(errors="replace")[-3000:])
            raise RuntimeError("isolated fixture did not produce a receipt")
        result = json.loads(result_path.read_text())
        result["codex_sha256"] = digest(codex)
        result["gateway_sha256"] = digest(binary) if binary else None
        result["script_sha256"] = digest(__file__)
        if output:
            with open(output, "x", encoding="utf-8") as handle:
                os.chmod(output, 0o600)
                json.dump(result, handle, indent=2)
                handle.write("\n")
        print(json.dumps(result, indent=2))
        if not result["passed"] and args.keep_debug:
            print("Isolated fixture debug directory: " + str(root), file=sys.stderr)
            root = None
        return 0 if execution.returncode == 0 and result["passed"] else 1
    finally:
        if root is not None:
            shutil.rmtree(root)


if __name__ == "__main__":
    raise SystemExit(main())
