#!/usr/bin/env python3
"""Isolated desktop consumer tests. The record backend never notifies/inhibits the OS."""
import argparse
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest


def private_write(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(fd, "wb") as out:
        out.write(data)
        out.flush()
        os.fsync(out.fileno())
    if os.name == "nt":
        # Test files inherit the already protected inbox; tighten the fixture file
        # explicitly as well. No command targets a path outside this temporary tree.
        user = subprocess.run(["whoami"], check=True, capture_output=True, text=True).stdout.strip()
        subprocess.run(["icacls", str(path), "/inheritance:r", "/grant:r", user + ":(F)", "*S-1-5-18:(F)"],
                       check=True, capture_output=True)


def wait_until(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.025)
    raise AssertionError("isolated desktop condition did not settle")


class Gateway:
    """Shared fixture surface for host lifecycle tests; all configuration is synthetic."""
    def __init__(self, binary, plugin, root, desktop=None, responder=None):
        self.binary, self.plugin = Path(binary).resolve(), Path(plugin).resolve()
        self.root = Path(root).resolve()
        self.state, self.inbox = self.root / "state", self.root / "inbox"
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.responder = responder or self._response
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                n = int(self.headers.get("Content-Length", "0"))
                if n > 1024 * 1024:
                    self.send_error(413)
                    return
                value = json.loads(self.rfile.read(n))
                status, content_type, body = owner.responder(value)
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                    self.wfile.write(body)
                self.close_connection = True

        self.upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.upstream.daemon_threads = True
        self.thread = threading.Thread(target=self.upstream.serve_forever, daemon=True)
        self.thread.start()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.port = listener.getsockname()[1]
        self.desktop = {
            "enabled": True, "keep_awake": True, "usage_alert": 90, "balance_alert": 5,
            "reset_reminder_hours": 2, "turn_done_seconds": 1, "mask_alert": True,
            "language": "en", "backend": "record", "state_dir": str(self.state), "inbox": str(self.inbox),
        }
        self.desktop.update(desktop or {})
        private_write(self.root / "desktop-fixture.json", json.dumps(self.desktop).encode())
        self.config = self.root / "config.conf"
        private_write(self.config, (
            f"[server]\nlisten=127.0.0.1:{self.port}\nworkers=2\nqueue=8\nupstream_timeout_ms=10000\n"
            f"[provider fixture]\nbase_url=http://127.0.0.1:{self.upstream.server_port}/v1\nprotocol=chat\n"
            "[route fixture]\nprovider=fixture\nmodel=fixture\n"
            f"[plugin desktop]\npath={self.plugin}\nconfig_file={self.root / 'desktop-fixture.json'}\n"
        ).encode())
        self.env = {k: os.environ[k] for k in ("PATH", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT") if k in os.environ}
        self.env.update(HOME=str(self.root / "home"), USERPROFILE=str(self.root / "home"),
                        XDG_CONFIG_HOME=str(self.root / "config"), XDG_STATE_HOME=str(self.root / "local-state"),
                        XDG_DATA_HOME=str(self.root / "data"), XDG_CACHE_HOME=str(self.root / "cache"))
        for name in ("home", "config", "local-state", "data", "cache"):
            (self.root / name).mkdir(mode=0o700)
        self.process = None
        self.log = None
        self.stopped = False

    @staticmethod
    def _response(body):
        if body.get("stream"):
            data = 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}\n\n'
            data += 'data: {"id":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
            return 200, "text/event-stream", data.encode()
        return 200, "application/json", b'{"id":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}'

    def start(self):
        self.log = (self.root / "gateway.log").open("wb")
        kwargs = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}
        self.process = subprocess.Popen([str(self.binary), "serve", "--config", str(self.config)],
                                        env=self.env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=subprocess.STDOUT, **kwargs)

        def ready():
            if self.process.poll() is not None:
                raise AssertionError("isolated gateway exited during startup: " + (self.root / "gateway.log").read_text()[-2000:])
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=0.2)
                c.request("GET", "/healthz")
                r = c.getresponse()
                r.read()
                c.close()
                return r.status == 200
            except (OSError, http.client.HTTPException):
                return False
        wait_until(ready)
        return self

    def post(self, body=None, headers=None):
        body = body or {"model": "fixture", "messages": [{"role": "user", "content": "SYNTHETIC_PRIVATE_CANARY"}]}
        h = {"Content-Type": "application/json", "User-Agent": "codex/fixture", "x-session-id": "isolated-fixture"}
        h.update(headers or {})
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=12)
        c.request("POST", "/v1/chat/completions", json.dumps(body), h)
        r = c.getresponse()
        result = r.status, r.read()
        c.close()
        return result

    def publish(self, kind, event, filename=None):
        wait_until(self.inbox.is_dir)
        name = filename or (os.urandom(16).hex() + ".json")
        temp = self.inbox / (".pending-" + os.urandom(16).hex())
        private_write(temp, json.dumps({"kind": kind, "event": event}, separators=(",", ":")).encode())
        os.replace(temp, self.inbox / name)

    def records(self):
        path = self.state / "recorded.jsonl"
        if not path.exists():
            return []
        # The writer appends one line; ignore an in-progress tail for polling only.
        data = path.read_bytes()
        return [json.loads(line) for line in data.split(b"\n")[:-1] if line]

    def stop(self):
        if self.stopped:
            return
        self.stopped = True
        if self.process and self.process.poll() is None:
            if os.name == "nt":
                self.process.send_signal(signal.CTRL_BREAK_EVENT)
            else:
                self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
                raise AssertionError("desktop worker prevented graceful gateway shutdown")
        if self.log:
            self.log.close()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.thread.join(timeout=2)


class DesktopTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zdesk-")
        self.root = Path(self.temp.name).resolve()
        self.gateway = None

    def tearDown(self):
        if self.gateway:
            self.gateway.stop()
        self.temp.cleanup()

    def make(self, **kwargs):
        self.gateway = Gateway(ARGS.binary, ARGS.desktop_plugin, self.root, **kwargs)
        return self.gateway

    def test_check_is_side_effect_free(self):
        g = self.make()
        p = subprocess.run([str(g.binary), "check", "--config", str(g.config)], env=g.env, capture_output=True, timeout=10)
        self.assertEqual(p.returncode, 0, p.stderr.decode()[-1000:])
        self.assertFalse(g.state.exists())
        self.assertFalse(g.inbox.exists())

    def test_inbox_closes_all_four_notice_categories_without_generation(self):
        g = self.make().start()
        now = int(time.time() * 1000)
        q = {"version": 1, "source_id": "opaque-fixture", "observation_id": "one", "observed_at_ms": now, "fresh": True, "success": True,
             "windows": [{"id": "used", "used_percent": 95, "span_seconds": 604800},
                         {"id": "reset", "used_percent": 10, "span_seconds": 86400, "reset_at_ms": now + 3600000}],
             "balance": {"amount": 2.5, "unit": "CNY"}}
        m = {"version": 1, "event_id": "first-record", "agent": "codex", "counts": [{"kind": "TOKEN", "new_values": 2}]}
        g.publish(1, q)
        g.publish(1, q)
        g.publish(2, m)
        g.publish(2, m)
        wait_until(lambda: {"usage", "balance", "reset", "mask"} <= {r["kind"] for r in g.records()})
        notices = [r for r in g.records() if r["kind"] in {"usage", "balance", "reset", "mask"}]
        self.assertEqual(len(notices), 4)
        self.assertFalse(any(r["kind"] == "inhibit" for r in g.records()))
        self.assertNotIn("opaque-fixture", json.dumps(notices))
        self.assertNotIn("first-record", json.dumps(notices))

    def test_fresh_below_threshold_rearms_stale_failure_does_not(self):
        g = self.make().start()
        now = int(time.time() * 1000)
        def event(n, used, fresh=True, success=True):
            return {"version": 1, "source_id": "opaque", "observation_id": str(n), "observed_at_ms": now + n,
                    "fresh": fresh, "success": success, "windows": [{"id": "week", "used_percent": used, "span_seconds": 604800}]}
        g.publish(1, event(1, 91))
        wait_until(lambda: len([r for r in g.records() if r["kind"] == "usage"]) == 1)
        for n, value in [(2, event(2, 0, success=False)), (3, event(3, 0, fresh=False)), (4, event(4, 95))]:
            g.publish(1, value, f"{n:032x}.json")
        wait_until(lambda: not list(g.inbox.glob("*.json")))
        self.assertEqual(len([r for r in g.records() if r["kind"] == "usage"]), 1)
        g.publish(1, event(5, 20), "00000000000000000000000000000005.json")
        wait_until(lambda: not list(g.inbox.glob("*.json")))
        g.publish(1, event(6, 95))
        wait_until(lambda: len([r for r in g.records() if r["kind"] == "usage"]) == 2)

    def test_consumer_claim_honors_cross_process_publication_lock(self):
        g = self.make().start()
        lock_path = g.inbox / ".publish.lock"
        wait_until(lock_path.exists)
        event = {"version": 1, "source_id": "opaque", "observation_id": "locked", "observed_at_ms": int(time.time() * 1000),
                 "fresh": True, "success": True, "windows": [{"id": "week", "used_percent": 95, "span_seconds": 604800}]}
        with lock_path.open("r+b") as held:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(held.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                g.publish(1, event, "f" * 32 + ".json")
                time.sleep(2.3)
                self.assertTrue((g.inbox / ("f" * 32 + ".json")).exists())
                self.assertFalse(list(g.inbox.glob(".claim-*.json")))
                self.assertFalse(any(r["kind"] == "usage" for r in g.records()))
            finally:
                if os.name == "nt":
                    held.seek(0)
                    msvcrt.locking(held.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    fcntl.flock(held.fileno(), fcntl.LOCK_UN)
        wait_until(lambda: any(r["kind"] == "usage" for r in g.records()))
        self.assertFalse((g.inbox / ("f" * 32 + ".json")).exists())

    def test_completed_response_produces_fifth_category_and_shutdown_releases(self):
        def slow(body):
            time.sleep(1.1)
            return Gateway._response(body)
        g = self.make(responder=slow).start()
        status, body = g.post()
        self.assertEqual(status, 200)
        self.assertIn(b"OK", body)
        wait_until(lambda: any(r["kind"] == "turn" for r in g.records()), timeout=8)
        self.assertTrue(any(r.get("level") == "system" for r in g.records()))
        self.assertTrue(any(r["kind"] == "state" and r["active_requests"] == 0 and r["completed"] == 1 for r in g.records()))
        g.stop()
        self.assertTrue(any(r.get("level") == "off" for r in g.records()))
        self.assertNotIn("SYNTHETIC_PRIVATE_CANARY", json.dumps(g.records()))

    @unittest.skipUnless(os.name == "posix" and os.uname().sysname == "Linux", "Linux sd-bus ABI fixture")
    def test_linux_native_backend_uses_only_private_mock_and_releases_owned_fd(self):
        compiler = shutil.which("cc")
        self.assertIsNotNone(compiler, "Linux native ABI fixture requires the platform C compiler")
        source = Path(__file__).resolve().parents[1] / "plugins/desktop/tests/mock-systemd.c"
        fake = self.root / "fake-native"
        fake.mkdir(mode=0o700)
        subprocess.run([compiler, "-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(fake / "libsystemd.so.0")], check=True, capture_output=True)
        log, hold = self.root / "native-mock.jsonl", self.root / "inhibitor"
        private_write(hold, b"fixture")
        g = self.make(desktop={"backend": "native", "turn_done_seconds": 0})
        g.env.update(LD_LIBRARY_PATH=str(fake), DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-private-fixture",
                     ZCALLD_DESKTOP_MOCK_LOG=str(log), ZCALLD_DESKTOP_MOCK_HOLD=str(hold))
        g.start()
        self.assertEqual(g.post()[0], 200)
        g.publish(1, {"version": 1, "source_id": "opaque", "observation_id": "mock", "observed_at_ms": int(time.time() * 1000),
                      "fresh": True, "success": True, "windows": [{"id": "week", "used_percent": 95, "span_seconds": 604800}]})
        def rows():
            return [json.loads(v) for v in log.read_text().splitlines()] if log.exists() else []
        wait_until(lambda: {"inhibit", "notify", "bounded_timeout"} <= {r["kind"] for r in rows()})
        fd_root = Path("/proc") / str(g.process.pid) / "fd"
        self.assertTrue(any(p.resolve() == hold for p in fd_root.iterdir()))
        g.stop()
        self.assertFalse(fd_root.exists())
        self.assertGreaterEqual(sum(r["kind"] == "close" for r in rows()), 2)

    @unittest.skipUnless(os.name == "posix" and os.uname().sysname == "Linux", "Linux private D-Bus fixture")
    def test_real_libsystemd_private_bus_serializes_and_releases_inhibitor(self):
        compiler, daemon, pkg_config = (shutil.which(v) for v in ("cc", "dbus-daemon", "pkg-config"))
        self.assertTrue(compiler and daemon and pkg_config, "Linux native API fixture requires cc, dbus-daemon and pkg-config")
        flags = subprocess.run([pkg_config, "--cflags", "--libs", "libsystemd"], capture_output=True, text=True, check=True).stdout.split()
        source = Path(__file__).resolve().parents[1] / "plugins/desktop/tests/private-bus.c"
        peer = self.root / "private-bus-peer"
        subprocess.run([compiler, "-O2", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(peer), *flags], check=True, capture_output=True)
        address = "unix:path=" + str(self.root / "bus")
        log = self.root / "private-bus.jsonl"
        bus = subprocess.Popen([daemon, "--session", "--nofork", "--address=" + address],
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        server = None
        try:
            wait_until(lambda: (self.root / "bus").exists())
            server = subprocess.Popen([str(peer), address, str(log)], stdin=subprocess.DEVNULL,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            def rows():
                return [json.loads(v) for v in log.read_text().splitlines()] if log.exists() else []
            wait_until(lambda: any(r["kind"] == "ready" for r in rows()))
            g = self.make(desktop={"backend": "native", "turn_done_seconds": 0})
            # Both buses point to the isolated peer. No process gets a real desktop bus address.
            g.env.update(DBUS_SESSION_BUS_ADDRESS=address, DBUS_SYSTEM_BUS_ADDRESS=address)
            g.start()
            self.assertEqual(g.post()[0], 200)
            g.publish(1, {"version": 1, "source_id": "opaque", "observation_id": "private-bus", "observed_at_ms": int(time.time() * 1000),
                          "fresh": True, "success": True, "windows": [{"id": "week", "used_percent": 95, "span_seconds": 604800}]})
            wait_until(lambda: {"inhibit", "notify"} <= {r["kind"] for r in rows()})
            # sd_bus_message_unref must not close the independently held duplicate.
            time.sleep(0.1)
            self.assertFalse(any(r["kind"] == "released" for r in rows()))
            g.stop()
            wait_until(lambda: any(r["kind"] == "released" for r in rows()))
            self.assertEqual(sum(r["kind"] == "inhibit" for r in rows()), 1)
            self.assertEqual(sum(r["kind"] == "notify" for r in rows()), 1)
        finally:
            if self.gateway:
                self.gateway.stop()
            for process in (server, bus):
                if process:
                    process.terminate()
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
                    if process.stderr:
                        process.stderr.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--desktop-plugin", type=Path, required=True)
    ARGS, rest = parser.parse_known_args()
    unittest.main(argv=[__file__, *rest])
