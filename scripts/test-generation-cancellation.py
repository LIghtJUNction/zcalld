#!/usr/bin/env python3
"""Exercise generation cancellation and half-close semantics with local mock providers."""
import argparse
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest


RECOVERY_SECONDS = 2.5
UPSTREAM_TIMEOUT_MS = 8000
FIRST_EVENT = (b'data: {"choices":[{"delta":{"content":"first-fragment"},'
               b'"index":0}]}\n\n')
LAST_EVENT = (b'data: {"choices":[{"delta":{"content":"last-fragment"},'
              b'"index":0}]}\n\ndata: [DONE]\n\n')
BODY = FIRST_EVENT + LAST_EVENT


class PendingRequest:
    def __init__(self):
        self.ready = threading.Event()
        self.release = threading.Event()
        self.disconnected = threading.Event()
        self.completed = threading.Event()


class MockProvider(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with self.server.request_lock:
            self.server.requests.append(json.loads(body))
            first = len(self.server.requests) == 1
        pending = self.server.pending if first else None
        self.close_connection = True
        try:
            if pending and self.server.phase == "head":
                pending.ready.set()
                if self.wait_for_release(pending):
                    return
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(BODY)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(FIRST_EVENT)
            self.wfile.flush()
            if pending and self.server.phase == "body":
                pending.ready.set()
                if self.wait_for_release(pending):
                    return
            self.wfile.write(LAST_EVENT)
            self.wfile.flush()
            if pending:
                pending.completed.set()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            if pending:
                pending.disconnected.set()

    def wait_for_release(self, pending):
        # Only an actual FIN/RST counts as cancellation. A deadline or test
        # teardown release never supplies the cancellation evidence.
        while not pending.release.is_set():
            try:
                readable, _, _ = select.select([self.connection], [], [], 0.02)
                if readable:
                    if not self.connection.recv(1, socket.MSG_PEEK):
                        pending.disconnected.set()
                        return True
                    # These fixtures send one complete request per connection.
                    # Unexpected bytes are a fixture failure, not cancellation.
                    self.server.unexpected_input.set()
                    return True
            except (ConnectionResetError, ConnectionAbortedError):
                pending.disconnected.set()
                return True
        return False


class GenerationCancellationTests(unittest.TestCase):
    binary = None
    community_plugin = None
    bun = None
    community_host = None
    community_pi = None

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zcalld-cancel-")
        self.root = Path(self.directory.name).resolve()
        self.socket_path = self.root / "gateway.sock"
        self.config = self.root / "config.conf"
        self.process = None
        self.log = tempfile.TemporaryFile()
        self.upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockProvider)
        self.upstream.daemon_threads = True
        self.upstream.requests = []
        self.upstream.request_lock = threading.Lock()
        self.upstream.pending = PendingRequest()
        self.upstream.unexpected_input = threading.Event()
        self.thread = threading.Thread(
            target=lambda: self.upstream.serve_forever(poll_interval=0.02), daemon=True)
        self.thread.start()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]

    def tearDown(self):
        self.upstream.pending.release.set()
        stop_timed_out = False
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            try:
                self.process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
                stop_timed_out = True
        self.upstream.shutdown()
        self.upstream.server_close()
        self.thread.join(timeout=2)
        self.log.close()
        self.directory.cleanup()
        self.assertFalse(stop_timed_out, "gateway did not stop within four seconds")

    def prepare(self, community, unix, phase):
        if unix and (not hasattr(socket, "AF_UNIX") or os.name == "nt"):
            self.skipTest("Unix sockets required")
        if community and not all((self.community_plugin, self.bun,
                                  self.community_host, self.community_pi)):
            self.skipTest("community cancellation requires plugin, Bun, host and pi paths")
        self.upstream.phase = phase
        self.config.write_text(
            f"[server]\nlisten=127.0.0.1:{self.port}\n"
            f"unix_socket={self.socket_path if unix else 'none'}\n"
            f"workers=1\nqueue=4\nupstream_timeout_ms={UPSTREAM_TIMEOUT_MS}\n"
            f"[provider mock]\nbase_url=http://127.0.0.1:{self.upstream.server_port}/v1\n"
            "protocol=chat\n"
            + ("plugin=fixture\nplugin_npm=@ai-sdk/openai-compatible\n" if community else "")
            + "[route test]\nprovider=mock\nmodel=upstream-test\n",
            encoding="utf-8", newline="\n")
        if community:
            provider = self.root / "provider.mjs"
            provider.write_text(
                "export default async()=>({auth:{provider:'fixture',methods:[{type:'api',label:'Fixture'}],"
                "loader:async()=>({apiKey:'fixture-upstream',fetch:async(url,init)=>fetch(url,init)})}})",
                encoding="utf-8", newline="\n")
            registry = self.root / "registry.json"
            registry.write_text(json.dumps({
                "version": 1,
                "plugins": [{"name": "fixture", "source": "fixture", "path": str(provider),
                             "kind": "community", "enabled": True}],
                "config": {"provider": {"fixture": {
                    "name": "Fixture", "npm": "@ai-sdk/openai-compatible",
                    "models": {"upstream-test": {"name": "Model"}}}}}}),
                encoding="utf-8", newline="\n")
            auth = self.root / "plugin-auth.json"
            auth.write_text(json.dumps({"fixture": {"type": "api", "key": "fixture-key"}}),
                            encoding="utf-8", newline="\n")
            if os.name == "posix":
                auth.chmod(0o600)
            settings = self.root / "community.json"
            settings.write_text(json.dumps({
                "bun": str(self.bun), "host": str(self.community_host),
                "pi": str(self.community_pi), "registry": str(registry),
                "auth_path": str(auth)}), encoding="utf-8", newline="\n")
            with self.config.open("a", encoding="utf-8", newline="\n") as config:
                config.write(f"\n[plugin community]\npath={self.community_plugin}\nconfig_file={settings}\n")
        environment = os.environ.copy()
        environment.pop("NOTIFY_SOCKET", None)
        kwargs = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}
        self.process = subprocess.Popen(
            [str(self.binary), "serve", "--config", str(self.config)],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log, env=environment, **kwargs)
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail("gateway startup failed: " + self.log.read().decode(errors="replace")[-4000:])
            try:
                if self.health(timeout=0.3) == 200:
                    return
            except OSError:
                pass
            time.sleep(0.02)
        self.fail("gateway readiness timed out")

    def health(self, timeout=RECOVERY_SECONDS):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)) as conn:
            conn.request("GET", "/healthz")
            response = conn.getresponse()
            response.read()
            return response.status

    def connection(self, unix):
        if unix:
            conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            conn.settimeout(4)
            conn.connect(str(self.socket_path))
            return conn
        return socket.create_connection(("127.0.0.1", self.port), timeout=4)

    @staticmethod
    def send_generation(conn):
        body = json.dumps({"model": "test", "messages": [
            {"role": "user", "content": "cancellation fixture"}], "stream": True}).encode()
        conn.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                     b"Content-Type: application/json\r\nConnection: close\r\n"
                     + f"Content-Length: {len(body)}\r\n\r\n".encode() + body)

    def wait_pending(self, conn, phase):
        self.assertTrue(self.upstream.pending.ready.wait(4), "upstream never reached the held stage")
        if phase == "body":
            received = bytearray()
            while b"first-fragment" not in received:
                data = conn.recv(4096)
                self.assertTrue(data, "generation closed before the first SSE fragment")
                received.extend(data)
                self.assertLess(len(received), 16384, "unexpectedly large first SSE event")
            self.assertIn(b" 200 ", received.split(b"\r\n", 1)[0])

    def followup(self):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=3)) as conn:
            body = json.dumps({"model": "test", "messages": [
                {"role": "user", "content": "next fixture"}], "stream": True})
            conn.request("POST", "/v1/chat/completions", body, {"Content-Type": "application/json"})
            response = conn.getresponse()
            result = response.read()
            self.assertEqual(response.status, 200, result.decode(errors="replace"))
            self.assertIn(b"last-fragment", result)
            self.assertTrue(result.endswith(b"data: [DONE]\n\n"), result)
        self.assertEqual(len(self.upstream.requests), 2, "abandoned generation was replayed")
        self.assertFalse(self.upstream.unexpected_input.is_set())

    def check_cancelled(self, community, unix, phase):
        self.prepare(community, unix, phase)
        with contextlib.closing(self.connection(unix)) as conn:
            self.send_generation(conn)
            self.wait_pending(conn, phase)
            if not unix:
                linger = struct.pack("hh" if os.name == "nt" else "ii", 1, 0)
                conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
        started = time.monotonic()
        self.assertTrue(self.upstream.pending.disconnected.wait(RECOVERY_SECONDS),
                        "confirmed caller disconnect left generation upstream alive")
        self.assertEqual(self.health(timeout=RECOVERY_SECONDS), 200)
        self.assertLess(time.monotonic() - started, RECOVERY_SECONDS,
                        "cancelled generation did not release the sole worker promptly")
        self.assertFalse(self.upstream.pending.completed.is_set(), "held upstream unexpectedly completed")
        self.followup()

    def check_half_close(self, community, unix, phase):
        self.prepare(community, unix, phase)
        with contextlib.closing(self.connection(unix)) as conn:
            self.send_generation(conn)
            self.assertTrue(self.upstream.pending.ready.wait(4))
            conn.shutdown(socket.SHUT_WR)
            self.assertFalse(self.upstream.pending.disconnected.wait(0.15),
                             "write-half-close was mistaken for cancellation")
            self.upstream.pending.release.set()
            with http.client.HTTPResponse(conn) as response:
                response.begin()
                body = response.read()
                self.assertEqual(response.status, 200, body.decode(errors="replace"))
                self.assertIn(b"first-fragment", body)
                self.assertTrue(body.endswith(b"data: [DONE]\n\n"), body)
        self.assertTrue(self.upstream.pending.completed.wait(1))
        self.assertFalse(self.upstream.pending.disconnected.is_set())
        self.assertEqual(self.health(), 200)
        self.followup()


def add_cases():
    for community in (False, True):
        for unix in (False, True):
            for phase in ("head", "body"):
                provider = "community" if community else "curl"
                transport = "unix" if unix else "tcp"
                def cancelled(self, community=community, unix=unix, phase=phase):
                    self.check_cancelled(community, unix, phase)
                def half_close(self, community=community, unix=unix, phase=phase):
                    self.check_half_close(community, unix, phase)
                suffix = f"{provider}_{transport}_pending_{phase}"
                setattr(GenerationCancellationTests, f"test_cancel_{suffix}", cancelled)
                setattr(GenerationCancellationTests, f"test_half_close_{suffix}", half_close)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/zcalld")
    for name in ("community-plugin", "bun", "community-host", "community-pi"):
        parser.add_argument("--" + name)
    arguments, remaining = parser.parse_known_args()
    GenerationCancellationTests.binary = Path(arguments.binary).resolve(strict=True)
    for name in ("community_plugin", "bun", "community_host", "community_pi"):
        value = getattr(arguments, name)
        setattr(GenerationCancellationTests, name, Path(value).resolve(strict=True) if value else None)
    add_cases()
    unittest.main(argv=[__file__, *remaining], verbosity=2)
