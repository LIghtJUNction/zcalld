#!/usr/bin/env python3
"""Black-box zcalld integration tests; all upstream traffic stays on loopback."""

from __future__ import annotations

import argparse
import contextlib
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

LOCAL_TOKEN = "zcalld-test-local-token"
UPSTREAM_KEY = "zcalld-test-provider-key"
BINARY = Path("target/release/zcalld")
PLUGIN_DIR: Path | None = None


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def rss_kib(pid: int) -> int:
    for line in Path(f"/proc/{pid}/status").read_text(encoding="utf-8").splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    return 0


def post_body(model: str = "local", stream: bool = False) -> dict:
    return {"model": model, "messages": [{"role": "user", "content": "hello"}], "stream": stream}


class MockState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.records: list[dict] = []
        self.gated_started = threading.Event()
        self.gated_release = threading.Event()
        self.cancel_started = threading.Event()
        self.cancel_disconnected = threading.Event()
        self.cancel_written = 0
        self.stopping = threading.Event()

    def record(self, request: dict) -> None:
        with self.lock:
            self.records.append(request)

    def count(self, model: str) -> int:
        with self.lock:
            return sum(item["body"].get("model") == model for item in self.records)

    def latest(self) -> dict:
        with self.lock:
            return self.records[-1]


ANTHROPIC_RESPONSE = {
    "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
    "content": [{"type": "text", "text": "hello"}],
    "stop_reason": "end_turn", "stop_sequence": None,
    "usage": {"input_tokens": 3, "output_tokens": 2},
}


def anthropic_sse() -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 3, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "text_delta", "text": "hello"}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                           "usage": {"output_tokens": 2}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return b"".join(
        f"event: {kind}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n".encode()
        for kind, data in events
    )


class MockHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def log_message(self, *_args: object) -> None:
        pass

    @property
    def state(self) -> MockState:
        return self.server.state  # type: ignore[attr-defined]

    def send_json(self, status: int, data: dict, **headers: str) -> None:
        raw = json.dumps(data, separators=(",", ":"), ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in headers.items():
            self.send_header(name.replace("_", "-"), value)
        self.end_headers()
        self.wfile.write(raw)
        self.wfile.flush()

    def begin_stream(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    def do_POST(self) -> None:
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            body = json.loads(raw)
        except ValueError:
            self.send_json(400, {"error": "mock expected JSON"})
            return
        self.state.record({"path": self.path, "headers": dict(self.headers), "body": body})
        model = body.get("model", "")
        if model == "fail":
            self.send_json(503, {"error": {"message": "mock rejection"}}, Retry_After="1")
            return
        if model == "redirect":
            self.send_json(307, {"error": "redirect"},
                           Location=f"http://127.0.0.1:{self.server.server_port}/redirect-target")
            return
        if model == "close":
            self.close_connection = True
            with contextlib.suppress(OSError):
                self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if body.get("stream") or ":streamGenerateContent" in self.path:
            self.begin_stream()
            try:
                if model == "cancel":
                    chunk = b'data: {"padding":"' + b"x" * 65500 + b'"}\n\n'
                    self.state.cancel_started.set()
                    for _ in range(512):
                        if self.state.stopping.is_set():
                            break
                        self.wfile.write(chunk)
                        self.wfile.flush()
                        with self.state.lock:
                            self.state.cancel_written += len(chunk)
                        time.sleep(0.001)
                    return
                if self.path.endswith("/messages"):
                    wire = anthropic_sse()
                    for offset in range(0, len(wire), 7):
                        self.wfile.write(wire[offset:offset + 7])
                        self.wfile.flush()
                    return
                wire = ('data: {"id":"stream_1","object":"chat.completion.chunk",'
                        '"created":1,"model":"upstream-model","choices":[{"index":0,'
                        '"delta":{"content":"hello ✓"},"finish_reason":null}]}\n\n').encode()
                split = wire.index("✓".encode()) + 1
                self.wfile.write(wire[:split])
                self.wfile.flush()
                time.sleep(0.01)
                self.wfile.write(wire[split:])
                self.wfile.flush()
                if model == "gated":
                    self.state.gated_started.set()
                    self.state.gated_release.wait(5)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                if model == "cancel":
                    self.state.cancel_disconnected.set()
            return
        if self.path.endswith("/messages"):
            self.send_json(200, ANTHROPIC_RESPONSE)
        elif self.path.endswith("/responses"):
            self.send_json(200, {"id": "resp_1", "object": "response", "status": "completed",
                                "model": model, "output": [{"type": "message", "role": "assistant",
                                "content": [{"type": "output_text", "text": "hello"}]}]})
        elif "/models/" in self.path:
            self.send_json(200, {"candidates": [{"content": {"role": "model",
                                "parts": [{"text": "hello"}]}, "finishReason": "STOP"}]})
        elif self.path.endswith("/interactions"):
            self.send_json(200, {"id": "interaction_1", "status": "completed",
                                "outputs": [{"type": "text", "text": "hello"}]})
        elif self.path.endswith("/completions") and not self.path.endswith("/chat/completions"):
            self.send_json(200, {"id": "cmpl_1", "object": "text_completion", "created": 1,
                                "model": model, "choices": [{"index": 0, "text": "hello",
                                "finish_reason": "stop"}]})
        else:
            self.send_json(200, {"id": "chat_1", "object": "chat.completion", "created": 1,
                                "model": model, "choices": [{"index": 0, "message": {
                                "role": "assistant", "content": "hello ✓"}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})


class MockUpstream:
    def __init__(self) -> None:
        self.state = MockState()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockHandler)
        self.server.daemon_threads = True
        self.server.state = self.state  # type: ignore[attr-defined]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self) -> None:
        self.state.stopping.set()
        self.state.gated_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class Gateway:
    def __init__(
        self, binary: Path, upstream: str, *, plugins: bool = False,
        bad_plugin: bool = False, workers: int = 4, queue: int = 16,
    ) -> None:
        self.binary = binary.resolve()
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-test-")
        self.root = Path(self.temp.name).resolve()
        self.port = free_port()
        self.config = self.root / "config.conf"
        self.env = os.environ.copy()
        self.env.update(ZCALLD_TEST_TOKEN=LOCAL_TOKEN, ZCALLD_TEST_UPSTREAM_KEY=UPSTREAM_KEY)
        self.log = (self.root / "gateway.log").open("w+")
        self.process: subprocess.Popen | None = None
        self.killed = False
        self.startup_ms = 0.0
        text = (
            f"[server]\nlisten=127.0.0.1:{self.port}\nworkers={workers}\nqueue={queue}\n"
            "max_body_bytes=8192\nmax_response_bytes=16777216\nrequest_timeout_ms=1000\n"
            "upstream_timeout_ms=10000\nconnect_timeout_ms=1000\ndefault_max_tokens=4096\n"
            "token_env=ZCALLD_TEST_TOKEN\n"
        )
        for name, protocol in [("upstream", "chat"), ("anthropic", "anthropic"),
                               ("responses", "responses"), ("gemini", "gemini"),
                               ("completions", "completions"), ("interactions", "interactions")]:
            text += (f"\n[provider {name}]\nbase_url={upstream}\nprotocol={protocol}\n"
                     "api_key_env=ZCALLD_TEST_UPSTREAM_KEY\n")
        for name, model, provider in [
            ("local", "upstream-model", "upstream"), ("gated", "gated", "upstream"),
            ("cancel", "cancel", "upstream"), ("failure", "fail", "upstream"),
            ("redirect", "redirect", "upstream"), ("close", "close", "upstream"),
            ("cross", "claude", "anthropic"), ("responses", "upstream-model", "responses"),
            ("gemini", "upstream-model", "gemini"), ("completions", "upstream-model", "completions"),
            ("interactions", "upstream-model", "interactions"),
        ]:
            text += f"\n[route {name}]\nprovider={provider}\nmodel={model}\n"
        if plugins:
            directory = (PLUGIN_DIR or self.binary.parent).resolve()
            for name in ["aliases", "policy"]:
                library = directory / f"libzcalld_{name}.so"
                if not library.is_file():
                    raise RuntimeError(f"build --workspace first: missing {library.name}")
                text += f"\n[plugin {name}]\npath={library}\nconfig_file={name}.conf\n"
            (self.root / "aliases.conf").write_text("fast=local\n", encoding="utf-8", newline="\n")
            (self.root / "policy.conf").write_text("allow=local\nmax_bytes=2048\n", encoding="utf-8", newline="\n")
        if bad_plugin:
            library = self.root / "invalid.so"
            library.write_text("not an ELF shared library\n", encoding="utf-8", newline="\n")
            text += f"\n[plugin invalid]\npath={library}\n"
        self.config.write_text(text, encoding="utf-8", newline="\n")
        self.config.chmod(0o600)

    def start(self) -> "Gateway":
        started = time.perf_counter()
        self.process = subprocess.Popen(
            [str(self.binary), "serve", "--config", str(self.config)],
            env=self.env, stdout=self.log, stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"gateway exited during startup: {self.diagnostics()}")
            try:
                status, _, _ = self.request("GET", "/healthz", auth=False, timeout=0.2)
                if status == 200:
                    self.startup_ms = (time.perf_counter() - started) * 1000
                    return self
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.01)
        raise RuntimeError(f"gateway did not become ready: {self.diagnostics()}")

    def diagnostics(self) -> str:
        self.log.flush()
        return (self.root / "gateway.log").read_text(encoding="utf-8")[-4000:]

    def request(
        self, method: str, path: str, data: object = None, *, auth: bool = True,
        headers: dict | None = None, timeout: float = 4,
    ) -> tuple[int, dict, bytes]:
        request_headers = {"Authorization": f"Bearer {LOCAL_TOKEN}"} if auth else {}
        if data is not None:
            request_headers["Content-Type"] = "application/json"
        request_headers.update(headers or {})
        body = data if isinstance(data, bytes) else (
            json.dumps(data, ensure_ascii=False).encode() if data is not None else None
        )
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            connection.request(method, path, body=body, headers=request_headers)
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()

    def raw(self, wire: bytes) -> tuple[int, bytes]:
        with socket.create_connection(("127.0.0.1", self.port), timeout=4) as sock:
            sock.sendall(wire)
            response = http.client.HTTPResponse(sock)
            response.begin()
            return response.status, response.read()

    def cli(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(self.binary), *args, "--config", str(self.config)],
            env=self.env, capture_output=True, text=True, timeout=8, encoding="utf-8",
        )

    def stop(self) -> int | None:
        if self.process is None:
            return None
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.killed = True
                self.process.kill()
                self.process.wait(timeout=3)
        return self.process.returncode

    def close(self) -> None:
        self.stop()
        self.log.close()
        self.temp.cleanup()

    def __enter__(self) -> "Gateway":
        try:
            return self.start()
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_args: object) -> None:
        self.close()


class GatewayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.mock = MockUpstream()
        cls.gateway = Gateway(BINARY, cls.mock.url)
        try:
            cls.gateway.start()
        except BaseException:
            cls.gateway.close()
            cls.mock.close()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls.gateway.close()
        cls.mock.close()

    def test_health_models_and_authentication(self) -> None:
        self.assertEqual(self.gateway.request("GET", "/healthz", auth=False)[0], 200)
        self.assertEqual(self.gateway.request("GET", "/v1/models", auth=False)[0], 401)
        status, _, body = self.gateway.request("GET", "/v1/models")
        self.assertEqual(status, 200)
        self.assertIn("local", [model["id"] for model in json.loads(body)["data"]])

    def test_routes_model_and_separates_credentials(self) -> None:
        status, _, body = self.gateway.request("POST", "/v1/chat/completions", post_body(),
                                              headers={"Cookie": "private=test"})
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)["model"], "upstream-model")
        record = self.mock.state.latest()
        self.assertEqual(record["body"]["model"], "upstream-model")
        headers = {key.lower(): value for key, value in record["headers"].items()}
        self.assertEqual(headers.get("authorization"), f"Bearer {UPSTREAM_KEY}")
        self.assertNotIn("cookie", headers)
        self.assertNotIn(LOCAL_TOKEN, json.dumps(record))

    def test_alternate_gateway_auth_headers(self) -> None:
        for header in ["x-api-key", "x-goog-api-key"]:
            with self.subTest(header=header):
                status, _, body = self.gateway.request("POST", "/v1/chat/completions",
                    post_body(), auth=False, headers={header: LOCAL_TOKEN})
                self.assertEqual(status, 200, body)

    def test_origin_and_unsupported_encodings(self) -> None:
        for headers, expected in [
            ({"Origin": "https://untrusted.invalid"}, 403),
            ({"Content-Type": "text/plain"}, 415),
            ({"Content-Encoding": "gzip"}, 400),  # Plain JSON is not a gzip frame.
            ({"Content-Encoding": "zstd"}, 400),
            ({"Content-Encoding": "br"}, 415),
        ]:
            with self.subTest(headers=headers):
                self.assertEqual(self.gateway.request("POST", "/v1/chat/completions",
                                 post_body(), headers=headers)[0], expected)

    def test_body_and_json_limits(self) -> None:
        self.assertEqual(self.gateway.request("POST", "/v1/chat/completions", b"x" * 8193)[0], 413)
        self.assertEqual(self.gateway.request("POST", "/v1/chat/completions", b"{invalid")[0], 400)
        self.assertEqual(self.gateway.request("POST", "/v1/chat/completions",
                         {"model": "missing", "messages": []})[0], 404)

    def test_ambiguous_headers_and_expect_are_rejected(self) -> None:
        base = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                f"Authorization: Bearer {LOCAL_TOKEN}\r\nContent-Type: application/json\r\n").encode()
        cases = [
            (b"Content-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}", 400),
            (b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}", 400),
            (b"Content-Length: 0\r\nExpect: 100-continue\r\n\r\n", 417),
        ]
        for tail, expected in cases:
            with self.subTest(tail=tail[:30]):
                self.assertEqual(self.gateway.raw(base + tail)[0], expected)

    def test_chunked_request_is_decoded(self) -> None:
        raw = json.dumps(post_body()).encode()
        parts = [raw[:7], raw[7:23], raw[23:]]
        body = b"".join(f"{len(part):x}\r\n".encode() + part + b"\r\n" for part in parts)
        header = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                  f"Authorization: Bearer {LOCAL_TOKEN}\r\nContent-Type: application/json\r\n"
                  "Transfer-Encoding: chunked\r\n\r\n").encode()
        status, reply = self.gateway.raw(header + body + b"0\r\n\r\n")
        self.assertEqual(status, 200, reply)

    def test_each_protocol_ingress_uses_its_configured_upstream(self) -> None:
        cases = [
            ("/v1/responses", {"model": "responses", "input": "hello"}, "/v1/responses"),
            ("/v1/messages", {"model": "cross", "max_tokens": 8,
                             "messages": [{"role": "user", "content": "hello"}]}, "/v1/messages"),
            ("/v1/completions", {"model": "completions", "prompt": "hello"}, "/v1/completions"),
            ("/v1beta/interactions", {"model": "interactions", "input": "hello"},
             "/v1beta/interactions"),
            ("/v1beta/models/gemini:generateContent",
             {"contents": [{"role": "user", "parts": [{"text": "hello"}]}]},
             "/v1beta/models/upstream-model:generateContent"),
        ]
        for path, request, expected_path in cases:
            with self.subTest(path=path):
                status, _, body = self.gateway.request("POST", path, request)
                self.assertEqual(status, 200, body)
                self.assertEqual(self.mock.state.latest()["path"], expected_path)
                json.loads(body)

    def test_same_protocol_preserves_provider_extensions_and_nested_model(self) -> None:
        request = post_body()
        request["vendor_extension"] = {"model": "nested-model", "enabled": True}
        status, _, body = self.gateway.request("POST", "/v1/chat/completions", request)
        self.assertEqual(status, 200, body)
        actual = self.mock.state.latest()["body"]
        self.assertEqual(actual["model"], "upstream-model")
        self.assertEqual(actual["vendor_extension"], request["vendor_extension"])

    def test_non_idempotent_failures_are_not_replayed(self) -> None:
        # One worker plus a warm-up guarantees the next POST uses a reusable connection.
        with Gateway(BINARY, self.mock.url, workers=1) as gateway:
            for route, model, expected in [("failure", "fail", 503), ("redirect", "redirect", 307),
                                           ("close", "close", 502)]:
                with self.subTest(route=route):
                    self.assertEqual(gateway.request("POST", "/v1/chat/completions", post_body())[0], 200)
                    before = self.mock.state.count(model)
                    status, _, _ = gateway.request("POST", "/v1/chat/completions", post_body(route))
                    self.assertEqual(status, expected)
                    self.assertEqual(self.mock.state.count(model) - before, 1)

    def test_stream_arrives_before_upstream_finishes_and_preserves_utf8(self) -> None:
        state = self.mock.state
        state.gated_started.clear()
        state.gated_release.clear()
        connection = http.client.HTTPConnection("127.0.0.1", self.gateway.port, timeout=2)
        try:
            connection.request("POST", "/v1/chat/completions", json.dumps(post_body("gated", True)),
                               {"Content-Type": "application/json", "Authorization": f"Bearer {LOCAL_TOKEN}"})
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            first = bytearray()
            while b"\n\n" not in first:
                part = response.read(1)
                self.assertTrue(part, "stream ended before first event")
                first.extend(part)
            self.assertTrue(state.gated_started.wait(1))
            self.assertFalse(state.gated_release.is_set())
            self.assertIn("hello ✓", first.decode())
            state.gated_release.set()
            self.assertIn(b"[DONE]", response.read())
        finally:
            state.gated_release.set()
            connection.close()

    def test_slow_client_backpressure_and_disconnect_cancel_upstream(self) -> None:
        state = self.mock.state
        state.cancel_started.clear()
        state.cancel_disconnected.clear()
        with state.lock:
            state.cancel_written = 0
        sock = socket.create_connection(("127.0.0.1", self.gateway.port), timeout=3)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        body = json.dumps(post_body("cancel", True)).encode()
        request = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                   f"Authorization: Bearer {LOCAL_TOKEN}\r\nContent-Type: application/json\r\n"
                   f"Content-Length: {len(body)}\r\n\r\n").encode() + body
        baseline = rss_kib(self.gateway.process.pid)
        try:
            sock.sendall(request)
            self.assertTrue(state.cancel_started.wait(2))
            time.sleep(0.4)
            with state.lock:
                written = state.cancel_written
            self.assertLess(written, 16 * 1024 * 1024, "upstream was drained while client was not reading")
            self.assertLess(rss_kib(self.gateway.process.pid) - baseline, 16 * 1024,
                            "gateway retained a large streaming response")
        finally:
            with contextlib.suppress(OSError):
                sock.shutdown(socket.SHUT_RDWR)
            sock.close()
        self.assertTrue(state.cancel_disconnected.wait(3), "upstream was not closed after client disconnect")

    def test_cross_protocol_request_and_response(self) -> None:
        status, _, raw = self.gateway.request("POST", "/v1/chat/completions", post_body("cross"))
        self.assertEqual(status, 200, raw)
        body = json.loads(raw)
        self.assertEqual(body["choices"][0]["message"]["content"], "hello")
        self.assertEqual(body["usage"]["prompt_tokens"], 3)
        self.assertEqual(body["usage"]["completion_tokens"], 2)
        record = self.mock.state.latest()
        self.assertTrue(record["path"].endswith("/messages"))
        self.assertEqual(record["body"]["model"], "claude")
        self.assertEqual(record["body"]["max_tokens"], 4096)

    def test_cross_protocol_fragmented_sse(self) -> None:
        status, _, body = self.gateway.request("POST", "/v1/chat/completions", post_body("cross", True))
        self.assertEqual(status, 200, body)
        self.assertIn(b"hello", body)
        self.assertIn(b"[DONE]", body)

    def test_native_aliases_and_policy(self) -> None:
        with Gateway(BINARY, self.mock.url, plugins=True) as gateway:
            status, _, body = gateway.request("POST", "/v1/chat/completions", post_body("fast"))
            self.assertEqual(status, 200, body)
            self.assertEqual(self.mock.state.latest()["body"]["model"], "upstream-model")
            status, _, _ = gateway.request("POST", "/v1/chat/completions", post_body("cross"))
            self.assertEqual(status, 403)

    def test_invalid_plugin_stops_startup(self) -> None:
        gateway = Gateway(BINARY, self.mock.url, bad_plugin=True)
        try:
            result = gateway.cli("check")
            self.assertNotEqual(result.returncode, 0)
            process = subprocess.Popen([str(gateway.binary), "serve", "--config", str(gateway.config)],
                                       env=gateway.env, stdout=gateway.log, stderr=subprocess.STDOUT)
            gateway.process = process
            self.assertNotEqual(process.wait(timeout=5), 0)
        finally:
            gateway.close()

    def test_cli_and_configuration_validation(self) -> None:
        version = subprocess.run([str(BINARY), "--version"], capture_output=True, text=True, timeout=5, encoding="utf-8")
        self.assertEqual(version.returncode, 0)
        self.assertIn("zcalld", version.stdout.lower())
        for command in [("check",), ("models", "--json"), ("providers", "--json"),
                        ("plugins",), ("env", "openai")]:
            with self.subTest(command=command):
                result = self.gateway.cli(*command)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn(UPSTREAM_KEY, result.stdout)
                if "--json" in command:
                    json.loads(result.stdout)
        original = self.gateway.config.read_text(encoding="utf-8")
        try:
            self.gateway.config.write_text(original + "\n[unknown]\nvalue=1\n", encoding="utf-8", newline="\n")
            self.assertNotEqual(self.gateway.cli("check").returncode, 0)
        finally:
            self.gateway.config.write_text(original, encoding="utf-8", newline="\n")

    def test_init_does_not_overwrite_existing_configuration(self) -> None:
        with tempfile.TemporaryDirectory(prefix="zcalld-init-") as directory:
            config = Path(directory).resolve() / "config.conf"
            command = [str(BINARY), "init", "--config", str(config)]
            first = subprocess.run(command, capture_output=True, text=True, timeout=5, encoding="utf-8")
            self.assertEqual(first.returncode, 0, first.stderr)
            original = config.read_bytes()
            second = subprocess.run(command, capture_output=True, text=True, timeout=5, encoding="utf-8")
            self.assertNotEqual(second.returncode, 0)
            self.assertEqual(config.read_bytes(), original)

    def test_sigterm_is_graceful(self) -> None:
        with Gateway(BINARY, self.mock.url) as gateway:
            self.assertEqual(gateway.stop(), 0)
            self.assertFalse(gateway.killed)


def main() -> int:
    global BINARY, PLUGIN_DIR
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/zcalld")
    parser.add_argument("--binary", dest="binary_option")
    parser.add_argument("--plugin-dir", type=Path)
    args = parser.parse_args()
    BINARY = Path(args.binary_option or args.binary).resolve()
    PLUGIN_DIR = args.plugin_dir
    if not BINARY.is_file():
        parser.error("build zcalld before running integration tests")
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(GatewayTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
