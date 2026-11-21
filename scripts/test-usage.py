#!/usr/bin/env python3
"""Validate native gateway usage against deterministic loopback responses."""
import argparse
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        content_type, body = self.server.reply
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        for offset in range(0, len(body), 113):
            self.wfile.write(body[offset:offset + 113])
        self.wfile.flush()


class UsageTests(unittest.TestCase):
    args = None

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zcalld-usage-")
        self.root = Path(self.temporary.name).resolve()
        self.upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        self.upstream.daemon_threads = True
        self.upstream.reply = ("application/json", b"{}")
        self.thread = threading.Thread(target=self.upstream.serve_forever, daemon=True)
        self.thread.start()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        self.usage = self.root / "usage.jsonl"
        groups = self.root / "routing.json"
        groups.write_text(json.dumps({"groups": [], "usage_file": str(self.usage)}),
                          encoding="utf-8")
        self.config = self.root / "config.conf"
        config = (f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\n"
                  "workers=1\nqueue=4\n")
        for protocol in ("chat", "responses", "anthropic", "gemini"):
            base = "/v1beta" if protocol == "gemini" else "/v1"
            config += (f"[provider {protocol}]\n"
                       f"base_url=http://127.0.0.1:{self.upstream.server_port}{base}\n"
                       f"protocol={protocol}\nauth=none\n"
                       f"[route {protocol}]\nprovider={protocol}\nmodel=upstream\n")
        config += (f"[plugin routing]\npath={self.args.routing_plugin}\n"
                   f"config={groups}\n")
        self.config.write_text(config, encoding="utf-8")
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("ZCALLD_", "CODEX_", "XDG_"))}
        self.env.update(HOME=str(self.root), USERPROFILE=str(self.root),
                        XDG_CONFIG_HOME=str(self.root / ".config"),
                        APPDATA=str(self.root / ".config"),
                        ZCALLD_PLUGIN_ROUTING_PLUGIN=str(self.args.routing_plugin))
        self.env.pop("NOTIFY_SOCKET", None)
        self.start_gateway()

    def start_gateway(self):
        self.log = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [str(self.args.binary), "serve", "--config", str(self.config)],
            stdout=self.log, stderr=self.log, env=self.env)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode())
            try:
                with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=1)) as connection:
                    connection.request("GET", "/healthz")
                    if connection.getresponse().status == 200:
                        return
            except OSError:
                pass
            time.sleep(.02)
        self.fail("gateway did not start")

    def stop_gateway(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()

    def tearDown(self):
        self.stop_gateway()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.thread.join(2)
        self.temporary.cleanup()

    def cli(self, *args):
        result = subprocess.run([str(self.args.binary), "usage", *args, "--json",
                                 "--config", str(self.config)], env=self.env,
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def call(self, protocol, content_type, reply, stream=False, record=True):
        self.upstream.reply = (content_type, reply)
        body = {"model": protocol, "stream": stream}
        if protocol == "responses":
            path = "/v1/responses"
            body["input"] = "fixture"
        elif protocol == "gemini":
            path = f"/v1beta/models/{protocol}:" + ("streamGenerateContent" if stream else "generateContent")
            body = {"contents": [{"role": "user", "parts": [{"text": "fixture"}]}]}
        else:
            path = "/v1/messages" if protocol == "anthropic" else "/v1/chat/completions"
            body["messages"] = [{"role": "user", "content": "fixture"}]
            if protocol == "anthropic":
                body["max_tokens"] = 32
        before = len(self.records())
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)) as connection:
            connection.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
            response = connection.getresponse()
            result = response.read()
            self.assertEqual(response.status, 200, result)
            self.assertEqual(result, reply)
        if not record:
            return None
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            rows = self.records()
            if len(rows) > before:
                return rows[-1]
            time.sleep(.01)
        self.fail("usage was not recorded")

    def records(self):
        if not self.usage.exists():
            return []
        return [json.loads(line) for line in self.usage.read_text(encoding="utf-8").splitlines()]

    def test_long_response_collects_usage_without_a_group(self):
        reply = json.dumps({"choices": [{"message": {"content": "x" * (1024 * 1024)}}],
            "usage": {"prompt_tokens": 19, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 11},
                      "completion_tokens_details": {"reasoning_tokens": 3}}}).encode()
        row = self.call("chat", "application/json", reply)
        self.assertEqual(row["upstream"], "chat")
        self.assertEqual(tuple(row[key] for key in
            ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens")),
            (19, 7, 11, 0, 3))

    def test_protocol_streaming_details_and_summary(self):
        cases = [
            ("responses", [{"type": "response.completed", "response": {"usage": {
                "input_tokens": 19, "output_tokens": 7,
                "input_tokens_details": {"cached_tokens": 11},
                "output_tokens_details": {"reasoning_tokens": 3}}}}], (19, 7, 11, 0, 3)),
            ("anthropic", [{"type": "message_start", "message": {"usage": {
                "input_tokens": 19, "cache_read_input_tokens": 11, "cache_creation_input_tokens": 5}}},
                {"type": "message_delta", "usage": {"output_tokens": 7}}], (19, 7, 11, 5, 0)),
            ("gemini", [{"usageMetadata": {"promptTokenCount": 19, "candidatesTokenCount": 7,
                "cachedContentTokenCount": 11, "thoughtsTokenCount": 3}}], (19, 7, 11, 0, 3)),
        ]
        for protocol, events, expected in cases:
            with self.subTest(protocol=protocol):
                reply = "".join("data:" + json.dumps(event) + "\r\n\r\n" for event in events).encode()
                row = self.call(protocol, "text/event-stream", reply, stream=True)
                self.assertEqual(tuple(row[key] for key in
                    ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens")), expected)
        total = self.cli("summary")
        self.assertTrue(total["recording_configured"])
        self.assertEqual(tuple(total[key] for key in
            ("requests", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens")),
            (3, 57, 21, 33, 5, 6))

    def test_enable_disable_without_groups_or_explicit_plugin(self):
        self.stop_gateway()
        # Start from the ordinary provider-only configuration used by new installs.
        self.config.write_text(self.config.read_text(encoding="utf-8").split("[plugin routing]")[0],
                               encoding="utf-8")
        routing = self.root / "routing.json"
        routing.unlink()
        self.assertFalse(self.cli("summary")["recording_configured"])
        result = self.cli("enable")
        self.assertEqual(result, {"ok": True, "recording_configured": True, "restart_required": True})
        self.assertFalse(self.usage.exists())
        self.assertEqual(json.loads(routing.read_text(encoding="utf-8"))["groups"], [])
        if os.name == "posix":
            self.assertEqual(routing.stat().st_mode & 0o777, 0o600)
        self.start_gateway()
        reply = b'{"choices":[],"usage":{"prompt_tokens":19,"completion_tokens":7}}'
        row = self.call("chat", "application/json", reply)
        self.assertEqual((row["input_tokens"], row["output_tokens"]), (19, 7))
        self.assertEqual(self.cli("summary")["requests"], 1)
        self.assertTrue(self.cli("disable")["restart_required"])
        self.stop_gateway()
        history = self.usage.read_bytes()
        self.start_gateway()
        self.call("chat", "application/json", reply, record=False)
        self.stop_gateway()
        self.assertEqual(self.usage.read_bytes(), history)
        totals = self.cli("summary")
        self.assertFalse(totals["recording_configured"])
        self.assertEqual(totals["requests"], 1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--routing-plugin", type=Path, required=True)
    UsageTests.args = parser.parse_args()
    UsageTests.args.binary = UsageTests.args.binary.resolve()
    UsageTests.args.routing_plugin = UsageTests.args.routing_plugin.resolve()
    unittest.main(argv=[__file__])
