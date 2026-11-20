#!/usr/bin/env python3
"""Verify native Codex metadata with the installed Codex app-server parser.

Only initialize, model/list and account/rateLimits/read are sent. No model turn,
tool call, real account request or user's Codex configuration is involved.
"""
import argparse
import contextlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import unittest


def fixture_module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


metadata = fixture_module("codex_metadata_fixture", "test-codex-metadata.py")
transport = fixture_module("codex_transport_fixture", "test-codex-transport.py")
MAX_RPC_LINE = 1 << 20
MAX_RPC_OUTPUT = 8 << 20
RPC_TIMEOUT = 20


class DenyExternalHandler(http.server.BaseHTTPRequestHandler):
    def reject(self):
        # Requests have only fixture identities. Retain no headers or URLs.
        self.server.attempts.append(self.command)
        self.send_response(502)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    do_CONNECT = do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = reject

    def log_message(self, *_):
        pass


class DenyExternalServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), DenyExternalHandler)
        self.attempts = []


class AppServer:
    """Small bounded stdio client; never dispatches server tool requests."""
    def __init__(self, binary, cwd, env):
        self.inbox = queue.Queue(maxsize=128)
        self.next_id = 0
        self.error_log = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [str(binary), "app-server", "--stdio"], cwd=cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.error_log,
        )
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        total = 0
        try:
            while True:
                line = self.process.stdout.readline(MAX_RPC_LINE + 1)
                if not line:
                    self.inbox.put(None, timeout=1)
                    return
                total += len(line)
                if len(line) > MAX_RPC_LINE or total > MAX_RPC_OUTPUT:
                    raise ValueError("Codex RPC output exceeded fixture bound")
                self.inbox.put(json.loads(line), timeout=1)
        except (ValueError, OSError, queue.Full) as error:
            try:
                self.inbox.put(error, timeout=1)
            except queue.Full:
                pass

    def send(self, value):
        self.process.stdin.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params=None):
        if method not in ("initialize", "account/rateLimits/read", "model/list"):
            raise ValueError("the metadata fixture cannot start model turns")
        self.next_id += 1
        request_id = self.next_id
        self.send({"id": request_id, "method": method, "params": params or {}})
        deadline = time.monotonic() + RPC_TIMEOUT
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"Codex RPC timed out: {method}")
            try:
                value = self.inbox.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError(f"Codex RPC timed out: {method}") from None
            if value is None:
                raise AssertionError(f"Codex app-server exited before {method}")
            if isinstance(value, Exception):
                raise AssertionError("Codex app-server returned invalid or excessive RPC output") from value
            if "method" in value:
                if "id" in value:
                    raise AssertionError("Codex requested an unexpected client operation")
                continue
            if value.get("id") == request_id:
                return value

    def initialize(self):
        reply = self.request("initialize", {
            "clientInfo": {"name": "zcalld_metadata_fixture", "version": "1"},
            "capabilities": {"experimentalApi": True},
        })
        if "error" in reply:
            raise AssertionError("Codex app-server initialization failed: " + json.dumps(reply["error"]))
        self.send({"method": "initialized"})

    def close(self):
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.reader.join(timeout=2)
        self.process.stdout.close()
        self.error_log.close()


class CodexClientMetadataTests(metadata.CodexMetadataTests):
    codex = None

    def setUp(self):
        super().setUp()
        self.config.write_text(self.config.read_text() +
                               "\n[route public-alias]\nprovider=codex\nmodel=fixture\n")
        self.binding({"enabled": True, "loopback": {
            "provider": "codex", "routes": ["public-alias"],
        }})
        self.client_work = self.root / "client-work"
        self.client_work.mkdir(mode=0o700)
        self.client_env = dict(self.env)
        codex_home = Path(self.client_env["CODEX_HOME"])
        transport.fake_auth(codex_home, "chatgpt")
        gateway = f"http://127.0.0.1:{self.port}"
        config = (
            'model = "public-alias"\n'
            'model_provider = "openai"\n'
            'cli_auth_credentials_store = "file"\n'
            f'openai_base_url = "{gateway}/backend-api/codex"\n'
            f'chatgpt_base_url = "{gateway}/backend-api/"\n'
            'web_search = "disabled"\n'
            '[analytics]\nenabled = false\n'
            '[history]\npersistence = "none"\n'
            '[features]\napps = false\nhooks = false\nmemory_tool = false\n'
            'multi_agent = false\nskip_host_skill_discovery = true\n'
            'use_agent_identity = false\n'
        )
        self.client_config = codex_home / "config.toml"
        self.client_config.write_text(config)
        self.client_config.chmod(0o600)
        self.client_config_before = self.client_config.read_bytes()
        self.client_auth_before = (codex_home / "auth.json").read_bytes()
        self.deny = DenyExternalServer()
        self.deny_thread = threading.Thread(target=self.deny.serve_forever, daemon=True)
        self.deny_thread.start()
        proxy = f"http://127.0.0.1:{self.deny.server_address[1]}"
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            self.client_env[key] = proxy
        self.client_env["NO_PROXY"] = self.client_env["no_proxy"] = "127.0.0.1,localhost,::1"

    def tearDown(self):
        try:
            self.deny.shutdown()
            self.deny.server_close()
            self.deny_thread.join(timeout=2)
            self.assertEqual(self.deny.attempts, [], "Codex attempted an external connection")
            self.assertEqual(self.recorded("forbidden_network"), [])
            self.assertEqual(self.recorded("fetch"), [], "metadata test started generation")
            self.assertEqual(self.client_config.read_bytes(), self.client_config_before)
            self.assertEqual((self.client_config.parent / "auth.json").read_bytes(),
                             self.client_auth_before)
        finally:
            super().tearDown()

    @contextlib.contextmanager
    def client(self):
        client = AppServer(self.codex, self.client_work, self.client_env)
        try:
            client.initialize()
            yield client
        finally:
            client.close()

    def test_real_client_reads_account_windows_and_bound_model_catalog(self):
        self.start()
        with self.client() as client:
            usage = client.request("account/rateLimits/read", {"excludeResetCreditDetails": True})
            self.assertNotIn("error", usage, usage)
            limits = usage["result"]["rateLimits"]
            self.assertEqual(limits["limitId"], "codex")
            self.assertEqual(limits["planType"], "plus")
            self.assertEqual(limits["primary"]["usedPercent"], 37)
            self.assertEqual(limits["primary"]["windowDurationMins"], 300)
            self.assertEqual(limits["primary"]["resetsAt"], 1900000000)
            self.assertEqual(usage["result"]["rateLimitsByLimitId"]["codex"], limits)
            models = client.request("model/list", {"includeHidden": True})
            self.assertNotIn("error", models, models)
            entries = models["result"]["data"]
            found = [m for m in entries if m["model"] == "public-alias"]
            self.assertEqual(len(found), 1, models)
            self.assertEqual(found[0]["id"], "public-alias")
            self.assertEqual(found[0]["displayName"], "Official fixture")
            self.assertEqual(found[0]["defaultReasoningEffort"], "high")
            self.assertEqual(found[0]["supportedReasoningEfforts"],
                             [{"reasoningEffort": "high", "description": "Official"}])
            self.assertFalse(any(m["model"] in ("fixture", "not-bound") for m in entries))
        requests = self.recorded("metadata")
        self.assertTrue(any(r["path"] == "/backend-api/wham/usage" for r in requests))
        self.assertTrue(any(r["path"] == "/backend-api/codex/models" and
                            r["query"] == "?client_version=0.170.0" for r in requests))

    def test_real_client_rejects_non_integral_usage_instead_of_reporting_success(self):
        # HTTP 200 alone previously accepted this invalid Codex payload.
        self.mock(bad_usage_schema=True)
        self.start()
        with self.client() as client:
            reply = client.request("account/rateLimits/read", {"excludeResetCreditDetails": True})
            self.assertIn("error", reply, reply)
            self.assertNotIn("result", reply)
        self.assertTrue(any(r["path"] == "/backend-api/wham/usage"
                            for r in self.recorded("metadata")))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("binary", "community-plugin", "pluginctl", "redact-plugin",
                "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + arg, required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    args = parser.parse_args()
    for key, value in vars(args).items():
        if not value:
            parser.error(f"missing {key}")
        path = Path(value).resolve()
        if not path.is_file():
            parser.error(f"{key} must name an existing executable or plugin")
        setattr(CodexClientMetadataTests, key, path)
    version = subprocess.run([str(CodexClientMetadataTests.codex), "--version"],
                             check=True, capture_output=True, text=True, timeout=10)
    print("Client: " + version.stdout.strip(), flush=True)
    names = sorted(n for n in CodexClientMetadataTests.__dict__ if n.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.TestSuite(CodexClientMetadataTests(name) for name in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
