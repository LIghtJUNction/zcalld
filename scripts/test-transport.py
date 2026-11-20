#!/usr/bin/env python3
"""Exercise real listener startup, HTTP, ownership, and shutdown with a local mock."""
import argparse
import contextlib
import errno
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import unittest

from fixture_private import create_privacy_key


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append({"body": request, "headers": dict(self.headers)})
        mode = self.server.mode
        if mode == "refresh" and self.path == "/token":
            self.server.refresh_started.set()
            self.server.refresh_release.wait(3)
            body = json.dumps({"access_token": "fixture-rotated-access", "refresh_token": "fixture-rotated-refresh", "expires_in": 3600}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if mode == "delayed":
            time.sleep(0.2)
        text = request["messages"][0]["content"] if mode in ("echo", "delayed", "hold") else "socket works"
        if request.get("stream"):
            parts = [text[index:index + 3] for index in range(0, len(text), 3)] if mode == "echo" else [text]
            body = b"".join(("data: " + json.dumps({"choices": [{"delta": {"content": part}, "index": 0}]}) + "\n\n").encode() for part in parts)
            body += b"data: [DONE]\n\n"
            content_type = "text/event-stream"
        else:
            body = json.dumps({"model": request["model"], "choices": [{"message": {"role": "assistant", "content": text}}]}).encode()
            content_type = "application/json"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body[:19])
            self.wfile.flush()
            if mode in ("delayed", "hold"):
                time.sleep(0.2 if mode == "delayed" else 3)
            self.wfile.write(body[19:])
        except (BrokenPipeError, ConnectionResetError):
            pass


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=3)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.path))


def request(connection, path="/healthz", body=None, headers=None):
    with contextlib.closing(connection):
        connection.request("POST" if body is not None else "GET", path,
                           body=None if body is None else json.dumps(body),
                           headers={**({"Content-Type": "application/json"} if body is not None else {}), **(headers or {})})
        response = connection.getresponse()
        return response.status, response.read()


class TransportTests(unittest.TestCase):
    binary = None
    redact_plugin = None
    privacy_worker = None
    community_plugin = None
    bun = None
    community_host = None
    community_pi = None

    @classmethod
    def setUpClass(cls):
        cls.upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        cls.upstream.daemon_threads = True
        cls.upstream.requests = []
        cls.upstream.mode = "default"
        cls.thread = threading.Thread(target=cls.upstream.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.upstream.shutdown()
        cls.upstream.server_close()
        cls.thread.join(3)

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zcalld-transport-")
        self.root = Path(self.directory.name).resolve()
        self.socket_path = self.root / "run" / "gateway.sock"
        self.config = self.root / "config.conf"
        self.processes = []
        self.log_files = []
        self.upstream.requests.clear()
        self.upstream.mode = "default"
        self.upstream.refresh_started = threading.Event()
        self.upstream.refresh_release = threading.Event()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None:
                self.stop(process)
        for file in self.log_files:
            file.close()
        self.directory.cleanup()

    def test_origin_rejection_preserves_response_before_split_body(self):
        self.write_config()
        self.start()
        with socket.create_connection(("127.0.0.1", self.port), timeout=3) as client:
            client.sendall(
                b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                b"Origin: https://example.invalid\r\nContent-Type: application/json\r\n"
                b"Content-Length: 2\r\n\r\n")
            response = http.client.HTTPResponse(client)
            response.begin()
            self.assertEqual(response.status, 403)
            self.assertEqual(json.loads(response.read())["error"]["code"], 403)
            self.assertEqual(client.recv(1), b"")
            # The body is deliberately sent after the rejection's FIN.
            try:
                client.sendall(b"{}")
                client.shutdown(socket.SHUT_WR)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                # A delayed test thread can exceed the bounded close grace.
                pass
            except OSError as error:
                if error.errno != errno.ENOTCONN:
                    raise
        self.assertEqual(self.upstream.requests, [])

    def test_incomplete_rejected_bodies_release_all_workers(self):
        self.write_config()
        self.start()
        with contextlib.ExitStack() as stack:
            for _ in range(2):
                client = stack.enter_context(socket.create_connection(
                    ("127.0.0.1", self.port), timeout=3))
                client.sendall(
                    b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                    b"Origin: https://example.invalid\r\nContent-Type: application/json\r\n"
                    b"Content-Length: 1048576\r\n\r\n")
                response = http.client.HTTPResponse(client)
                response.begin()
                self.assertEqual(response.status, 403)
                self.assertEqual(json.loads(response.read())["error"]["code"], 403)
            # Both sending sides remain open, but health must regain a worker.
            status, _ = request(http.client.HTTPConnection(
                "127.0.0.1", self.port, timeout=2))
            self.assertEqual(status, 200)
        self.assertEqual(self.upstream.requests, [])

    def write_config(self, tcp=True, unix=False, extra=""):
        self.config.write_text(
            f"[server]\nlisten={'127.0.0.1:' + str(self.port) if tcp else 'none'}\n"
            f"unix_socket={self.socket_path if unix else 'none'}\nworkers=2\nqueue=4\n"
            f"[provider mock]\nbase_url=http://127.0.0.1:{self.upstream.server_port}/v1\nprotocol=chat\n"
            "[route test]\nprovider=mock\nmodel=upstream-test\n" + extra,
            encoding="utf-8", newline="\n",
        )

    def start(self, ready=True, unix=False):
        log = tempfile.TemporaryFile()
        self.log_files.append(log)
        environment = os.environ.copy()
        environment.pop("NOTIFY_SOCKET", None)
        kwargs = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}
        process = subprocess.Popen([str(self.binary), "serve", "--config", str(self.config)],
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                   env=environment, **kwargs)
        self.processes.append(process)
        if ready:
            deadline = time.monotonic() + 6
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    log.seek(0)
                    self.fail("gateway failed to start: " + log.read().decode(errors="replace"))
                try:
                    status, _ = request(UnixConnection(self.socket_path) if unix else http.client.HTTPConnection("127.0.0.1", self.port, timeout=1))
                    if status == 200:
                        return process
                except OSError:
                    pass
                time.sleep(0.02)
            self.fail("gateway readiness timed out")
        return process

    def stop(self, process):
        if process.poll() is None:
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            try:
                process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
                self.fail("gateway did not stop within four seconds")

    def health_cli(self):
        return subprocess.run([str(self.binary), "health", "--config", str(self.config)],
                              capture_output=True, timeout=5, check=False)

    def add_plugin(self, name, library, config):
        path = self.root / f"{name}.json"
        path.write_text(json.dumps(config), encoding="utf-8", newline="\n")
        with self.config.open("a") as file:
            file.write(f"\n[plugin {name}]\npath={library}\nconfig_file={path}\n")

    def privacy(self, missing_worker=False):
        if not self.redact_plugin or not self.privacy_worker:
            self.skipTest("requires --redact-plugin and --privacy-worker")
        self.write_config()
        settings = self.root / "privacy-settings.json"
        settings.write_text(json.dumps({"redact": True, "redactPersonal": True, "redactModelOff": True}), encoding="utf-8", newline="\n")
        key = create_privacy_key(self.root, binary=self.binary,
                                 redact_plugin=self.redact_plugin,
                                 privacy_worker=self.privacy_worker)
        worker = self.privacy_worker
        if missing_worker:
            if os.name != "posix":
                self.skipTest("runtime worker failure fixture uses a POSIX executable")
            worker = self.root / "failed-worker"
            worker.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8", newline="\n")
            worker.chmod(0o700)
        self.add_plugin("redact", self.redact_plugin, {"worker": str(worker), "settings": str(settings), "key": str(key)})

    def community(self):
        if not all((self.community_plugin, self.bun, self.community_host, self.community_pi)):
            self.skipTest("requires community plugin, Bun, host and pi paths")
        self.write_config()
        self.config.write_text(self.config.read_text(encoding="utf-8").replace("protocol=chat", "protocol=chat\nplugin=fixture\nplugin_npm=@ai-sdk/openai-compatible"), encoding="utf-8", newline="\n")
        provider = self.root / "provider.mjs"
        provider.write_text("export default async()=>({auth:{provider:'fixture',methods:[{type:'api',label:'Fixture'}],loader:async()=>({apiKey:'fixture-upstream',fetch:async(url,init)=>{const headers=new Headers(init.headers);headers.set('x-zcalld-fixture','community');return fetch(url,{...init,headers})}})}})", encoding="utf-8", newline="\n")
        registry = self.root / "registry.json"
        registry.write_text(json.dumps({"version": 1, "plugins": [{"name": "fixture", "source": "fixture", "path": str(provider), "kind": "community", "enabled": True}], "config": {"provider": {"fixture": {"name": "Fixture", "npm": "@ai-sdk/openai-compatible", "models": {"upstream-test": {"name": "Model"}}}}}}), encoding="utf-8", newline="\n")
        auth = self.root / "plugin-auth.json"
        auth.write_text(json.dumps({"fixture": {"type": "api", "key": "fixture-key"}}), encoding="utf-8", newline="\n")
        if os.name == "posix":
            auth.chmod(0o600)
        self.add_plugin("community", self.community_plugin, {"bun": str(self.bun), "host": str(self.community_host), "pi": str(self.community_pi), "registry": str(registry), "auth_path": str(auth)})

    def test_privacy_restores_json_and_fragmented_sse(self):
        self.privacy()
        self.upstream.mode = "echo"
        self.start()
        original = "Contact jane.doe@acme.io for the fixture."
        for stream in (False, True):
            status, body = request(http.client.HTTPConnection("127.0.0.1", self.port, timeout=5), "/v1/chat/completions", {"model": "test", "messages": [{"role": "user", "content": original}], "stream": stream})
            self.assertEqual(status, 200, body.decode(errors="replace"))
            upstream = self.upstream.requests[-1]["body"]["messages"][0]["content"]
            self.assertNotIn("jane.doe@acme.io", upstream)
            if stream:
                text = "".join(json.loads(line[6:])["choices"][0]["delta"].get("content", "") for line in body.decode().splitlines() if line.startswith("data: ") and line != "data: [DONE]")
            else:
                text = json.loads(body)["choices"][0]["message"]["content"]
            self.assertEqual(text, original)

    def test_unavailable_privacy_worker_prevents_egress(self):
        self.privacy(missing_worker=True)
        self.start()
        status, _ = request(http.client.HTTPConnection("127.0.0.1", self.port, timeout=3), "/v1/chat/completions", {"model": "test", "messages": [{"role": "user", "content": "Contact jane.doe@acme.io"}]})
        self.assertEqual(status, 503)
        self.assertEqual(self.upstream.requests, [])

    def test_community_provider_handles_pending_head_and_stream(self):
        self.community()
        self.upstream.mode = "delayed"
        self.start()
        status, body = request(http.client.HTTPConnection("127.0.0.1", self.port, timeout=5), "/v1/chat/completions", {"model": "test", "messages": [{"role": "user", "content": "community fixture"}], "stream": True})
        self.assertEqual(status, 200, body.decode(errors="replace"))
        self.assertIn(b"community fixture", body)
        self.assertTrue(body.endswith(b"data: [DONE]\n\n"))
        headers = {key.lower(): value for key, value in self.upstream.requests[-1]["headers"].items()}
        self.assertEqual(headers.get("x-zcalld-fixture"), "community")

    def test_community_pending_body_cancels_on_shutdown(self):
        self.community()
        self.upstream.mode = "hold"
        process = self.start()
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=4)
        connection.request("POST", "/v1/chat/completions", json.dumps({"model": "test", "messages": [{"role": "user", "content": "held fixture"}], "stream": True}), {"Content-Type": "application/json"})
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.read(1), b"d")
        started = time.monotonic()
        self.stop(process)
        self.assertLess(time.monotonic() - started, 3)
        connection.close()

    def test_pending_codex_refresh_is_saved_before_gateway_shutdown(self):
        self.community()
        codex = Path(__file__).resolve().parents[1] / "extensions" / "compat" / "community" / "builtin" / "codex.mjs"
        if not codex.is_file():
            self.skipTest("requires the optional builtin provider adapter source")
        origin = f"http://127.0.0.1:{self.upstream.server_port}"
        (self.root / "provider.mjs").write_text(
            f"import {{createCodex}} from {json.dumps(str(codex))};export default async()=>createCodex({{baseURL:{json.dumps(origin)},tokenURL:{json.dumps(origin + '/token')},testMode:true}});", encoding="utf-8", newline="\n")
        self.config.write_text(self.config.read_text(encoding="utf-8").replace(
            "protocol=chat\nplugin=fixture\nplugin_npm=@ai-sdk/openai-compatible",
            "protocol=responses\nplugin=builtin:codex\nplugin_account=builtin:codex#fixture\nplugin_npm=@ai-sdk/openai"), encoding="utf-8", newline="\n")
        registry = json.loads((self.root / "registry.json").read_text(encoding="utf-8"))
        registry["config"] = {"provider": {"builtin:codex": {"npm": "@ai-sdk/openai", "models": {"upstream-test": {"name": "Fixture"}}}}}
        (self.root / "registry.json").write_text(json.dumps(registry), encoding="utf-8", newline="\n")
        auth = self.root / "plugin-auth.json"
        auth.write_text(json.dumps({"builtin:codex#fixture": {"type": "oauth", "access": "fixture-expired-access", "refresh": "fixture-old-refresh", "expires": 1}}), encoding="utf-8", newline="\n")
        self.upstream.mode = "refresh"
        process = self.start()
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        try:
            connection.request("POST", "/v1/responses", json.dumps({"model": "test", "input": "fixture", "stream": False}), {"Content-Type": "application/json"})
            self.assertTrue(self.upstream.refresh_started.wait(3), "token refresh did not start")
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            time.sleep(0.15)
            self.assertIsNone(process.poll(), "gateway killed its refreshing host before the token was saved")
            self.upstream.refresh_release.set()
            self.assertEqual(process.wait(timeout=4), 0)
            saved = json.loads(auth.read_text(encoding="utf-8"))["builtin:codex#fixture"]
            self.assertEqual(saved["refresh"], "fixture-rotated-refresh")
            self.assertGreater(saved["expires"], time.time() * 1000)
            self.assertEqual(len(self.upstream.requests), 1, "cancelled generation was replayed after refresh")
        finally:
            self.upstream.refresh_release.set()
            connection.close()

    def test_tcp_start_health_and_shutdown(self):
        self.write_config()
        process = self.start()
        result = self.health_cli()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(json.loads(result.stdout)["status"], "ok")
        self.stop(process)
        self.assertEqual(process.returncode, 0)

    def test_explicit_loopback_trust_still_rejects_browser_origin_and_host(self):
        self.write_config()
        credentials = self.root / "credentials"
        credentials.write_text('ZCALLD_TRANSPORT_TOKEN="test-local-token"\n', encoding="utf-8", newline="\n")
        if os.name == "posix":
            credentials.chmod(0o600)
        text = self.config.read_text(encoding="utf-8").replace("[server]\n", "[server]\ntoken_env=ZCALLD_TRANSPORT_TOKEN\ntrust_loopback=false\n")
        self.config.write_text(text, encoding="utf-8", newline="\n")
        process = self.start()
        connection = lambda: http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
        self.assertEqual(request(connection(), "/v1/models")[0], 401)
        self.stop(process)
        self.config.write_text(text.replace("trust_loopback=false", "trust_loopback=true"), encoding="utf-8", newline="\n")
        self.start()
        self.assertEqual(request(connection(), "/v1/models", headers={"Authorization": "Bearer old-agent-token"})[0], 200)
        self.assertEqual(request(connection(), "/v1/models", headers={"Origin": "http://evil.example"})[0], 403)
        self.assertEqual(request(connection(), "/v1/models", headers={"Host": "evil.example"})[0], 403)

    def test_private_caller_keys_authenticate_without_becoming_upstream_credentials(self):
        self.write_config()
        keys = self.root / "caller-keys.json"
        keys.write_text(json.dumps([
            {"id": "local", "name": "Local fixture", "lan": False, "secret": "fixture-caller-local"},
            {"id": "lan", "name": "LAN fixture", "lan": True, "secret": "fixture-caller-lan"},
        ]), encoding="utf-8", newline="\n")
        if os.name == "posix":
            keys.chmod(0o600)
        text = self.config.read_text(encoding="utf-8").replace("[server]\n", "[server]\ncaller_keys_file=caller-keys.json\n")
        self.config.write_text(text, encoding="utf-8", newline="\n")
        process = self.start()
        connection = lambda: http.client.HTTPConnection("127.0.0.1", self.port, timeout=3)
        for key, status in ((None, 401), ("fixture-caller-local", 200), ("fixture-caller-lan", 200), ("fixture-caller-wrong", 401)):
            headers = {} if key is None else {"Authorization": "Bearer " + key}
            self.assertEqual(request(connection(), "/v1/models", headers=headers)[0], status)
        status, _ = request(connection(), "/v1/chat/completions", {"model": "test", "messages": [{"role": "user", "content": "fixture"}]}, {"x-api-key": "fixture-caller-lan"})
        self.assertEqual(status, 200)
        forwarded = {key.lower(): value for key, value in self.upstream.requests[-1]["headers"].items()}
        self.assertNotIn("authorization", forwarded)
        self.assertNotIn("x-api-key", forwarded)
        self.stop(process)
        keys.write_text("[]", encoding="utf-8", newline="\n")
        self.start()
        self.assertEqual(request(connection(), "/v1/models")[0], 401, "an explicitly empty key file must not open the gateway")

    def test_nonloopback_listener_requires_a_lan_enabled_credential(self):
        self.write_config()
        keys = self.root / "caller-keys.json"
        keys.write_text(json.dumps([{"id": "local", "secret": "fixture-local", "lan": False}]), encoding="utf-8", newline="\n")
        if os.name == "posix":
            keys.chmod(0o600)
        self.config.write_text(self.config.read_text(encoding="utf-8").replace("[server]\n", "[server]\ncaller_keys_file=caller-keys.json\n").replace("listen=127.0.0.1:", "listen=0.0.0.0:"), encoding="utf-8", newline="\n")
        process = self.start(ready=False)
        self.assertNotEqual(process.wait(timeout=3), 0)

    @unittest.skipUnless(os.name == "posix", "Unix domain sockets")
    def test_dual_http_streaming_and_socket_preference(self):
        self.write_config(unix=True)
        self.start(unix=True)
        self.assertEqual(self.socket_path.stat().st_mode & 0o777, 0o600)
        for stream in (False, True):
            body = {"model": "test", "messages": [{"role": "user", "content": "hello"}], "stream": stream}
            status, reply = request(UnixConnection(self.socket_path), "/v1/chat/completions", body)
            self.assertEqual(status, 200)
            self.assertIn(b"socket works", reply)
            if stream:
                self.assertTrue(reply.endswith(b"data: [DONE]\n\n"))
        status, _ = request(http.client.HTTPConnection("127.0.0.1", self.port, timeout=2))
        self.assertEqual(status, 200)
        # Give the CLI an unreachable TCP address; health must still use Unix.
        text = self.config.read_text(encoding="utf-8")
        self.config.write_text(text.replace(f"listen=127.0.0.1:{self.port}", "listen=127.0.0.1:1"), encoding="utf-8", newline="\n")
        self.assertEqual(self.health_cli().returncode, 0)

    @unittest.skipUnless(os.name == "posix", "Unix domain sockets")
    def test_unix_only_duplicate_and_partial_request_shutdown(self):
        self.write_config(tcp=False, unix=True)
        process = self.start(unix=True)
        self.assertEqual(self.health_cli().returncode, 0)
        duplicate = self.start(ready=False)
        self.assertNotEqual(duplicate.wait(timeout=3), 0)
        self.assertEqual(request(UnixConnection(self.socket_path))[0], 200)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as partial:
            partial.connect(str(self.socket_path))
            partial.sendall(b"POST /v1/chat/completions HTTP/1.1\r\n")
            started = time.monotonic()
            self.stop(process)
            self.assertLess(time.monotonic() - started, 3)
        self.assertFalse(self.socket_path.exists())

    @unittest.skipUnless(os.name == "posix", "Unix domain sockets")
    def test_killed_process_stale_socket_is_recovered(self):
        self.write_config(tcp=False, unix=True)
        process = self.start(unix=True)
        process.kill()
        process.wait(timeout=3)
        self.assertTrue(self.socket_path.exists())
        restarted = self.start(unix=True)
        self.assertEqual(self.health_cli().returncode, 0)
        self.stop(restarted)
        self.assertFalse(self.socket_path.exists())

    @unittest.skipUnless(os.name == "posix", "Unix domain sockets")
    def test_worker_startup_failure_cleans_owned_socket(self):
        self.write_config(tcp=False, unix=True, extra=f"[plugin missing]\npath={self.root / 'missing.so'}\n")
        process = self.start(ready=False)
        self.assertNotEqual(process.wait(timeout=4), 0)
        self.assertFalse(self.socket_path.exists())

    @unittest.skipUnless(os.name == "posix", "Unix domain sockets")
    def test_cli_falls_back_to_tcp_when_unix_socket_is_absent(self):
        self.write_config(unix=False)
        self.start()
        self.write_config(unix=True)
        self.assertEqual(self.health_cli().returncode, 0)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/zcalld")
    for name in ("redact-plugin", "privacy-worker", "community-plugin", "bun", "community-host", "community-pi"):
        parser.add_argument("--" + name)
    arguments, remaining = parser.parse_known_args()
    TransportTests.binary = Path(arguments.binary).resolve(strict=True)
    for name in ("redact_plugin", "privacy_worker", "community_plugin", "bun", "community_host", "community_pi"):
        value = getattr(arguments, name)
        setattr(TransportTests, name, Path(value).resolve(strict=True) if value else None)
    unittest.main(argv=[__file__, *remaining], verbosity=2)
