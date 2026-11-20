#!/usr/bin/env python3
"""Real HTTP -> routing -> recovery ABI tests, with no provider network access."""
import argparse
import contextlib
import http.client
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest


class Recovery(unittest.TestCase):
    args = None

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-recovery-")
        self.root = Path(self.temp.name).resolve()
        self.process = None
        self.log = tempfile.TemporaryFile()
        self.settings = {}
        self.count = 2
        self.aliases = {}
        self.group_definition = None
        self.explicit = True
        self.timeout = 2500
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("ZCALLD_", "CODEX_", "XDG_"))
            and key != "NOTIFY_SOCKET"
        }
        self.environment.update(HOME=str(self.root), USERPROFILE=str(self.root),
                                APPDATA=str(self.root / "AppData/Roaming"),
                                XDG_CONFIG_HOME=str(self.root / ".config"))
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]

    def start(self):
        (self.root / "settings.json").write_text(json.dumps(self.settings), encoding="utf-8")
        groups = self.root / ("routing.json" if self.group_definition is not None else "groups.json")
        groups.write_text(json.dumps({"groups": [{
            "name": "group/test", "mode": "order", "original": {},
            "members": [{"model": f"route-{i}"} for i in range(self.count)],
        }]}), encoding="utf-8")
        config = (f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\n"
                  f"workers=1\nqueue=4\nupstream_timeout_ms={self.timeout}\n")
        for index in range(self.count):
            config += (f"[provider provider-{index}]\nbase_url=https://fixture.invalid/v1\n"
                       "protocol=chat\nauth=none\nplugin=fixture\n")
            if self.explicit:
                config += f"plugin_account=configured-account-{index}\n"
            target = self.aliases.get(index, index)
            config += (f"[route route-{index}]\nprovider=provider-{target}\nmodel=model-{target}\n")
        config += "[route group/test]\nprovider=provider-0\nmodel=model-0\n"
        config += f"[plugin fixture]\npath={self.args.fixture_plugin}\nconfig={self.root}\n"
        if self.group_definition is None:
            config += f"[plugin routing]\npath={self.args.routing_plugin}\nconfig={groups}\n"
        else:
            self.environment["ZCALLD_PLUGIN_ROUTING_PLUGIN"] = str(self.args.routing_plugin)
        self.config = self.root / "config.conf"
        self.config.write_text(config, encoding="utf-8", newline="\n")
        if self.group_definition is not None:
            definition = self.root / "group-definition.json"
            definition.write_text(json.dumps(self.group_definition), encoding="utf-8")
            result = subprocess.run(
                [str(self.args.binary), "groups", "set", "group/test", "--file",
                 str(definition), "--config", str(self.config), "--json"],
                capture_output=True, text=True, encoding="utf-8",
                env=self.environment, timeout=20, check=False)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.process = subprocess.Popen(
            [str(self.args.binary), "serve", "--config", str(self.config)],
            stdout=self.log, stderr=self.log, env=self.environment)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode("utf-8", "replace"))
            try:
                with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=.5)) as c:
                    c.request("GET", "/healthz")
                    if c.getresponse().status == 200:
                        return
            except OSError:
                pass
            time.sleep(.01)
        self.fail("fixture gateway did not start")

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                self.fail("fixture gateway failed to stop")
        self.log.close()
        self.temp.cleanup()

    def body(self):
        return {"model": "group/test", "messages": [{
            "role": "user", "content": "synthetic prompt; account=untrusted-body-account"}],
            "account": "untrusted-body-account"}

    def call(self):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)) as c:
            c.request("POST", "/v1/chat/completions", json.dumps(self.body()), {
                "Content-Type": "application/json", "ChatGPT-Account-Id": "untrusted-header-account"})
            response = c.getresponse()
            return response.status, response.read()

    def rows(self, name):
        path = self.root / name
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def fetches(self):
        return [json.loads(row) for row in self.rows("fetch.log")]

    def wait_file(self, name):
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if (self.root / name).exists():
                return
            time.sleep(.01)
        self.fail(f"missing fixture observation {name}")

    def test_healthy_backup_is_tried_before_recovery(self):
        self.settings["before"] = {"model-1": 200}
        self.start()
        self.assertEqual(self.call()[0], 200)
        self.assertEqual([v["model"] for v in self.fetches()], ["model-0", "model-1"])
        self.assertEqual(self.rows("recovery.log"), [])

    def test_cli_group_definition_applies_weights_to_real_requests(self):
        self.settings["before"] = {"model-0": 200, "model-1": 200}
        self.group_definition = {
            "routing": "weight", "affinity": "none",
            "members": [{"model": "route-0", "weight": 3},
                        {"model": "route-1", "weight": 1}],
        }
        self.start()
        for _ in range(4):
            status, body = self.call()
            self.assertEqual(status, 200, body)
        self.assertEqual([v["model"] for v in self.fetches()],
                         ["model-0", "model-0", "model-0", "model-1"])
        self.assertEqual(self.rows("recovery.log"), [])

    def test_cli_group_definition_never_sends_to_disabled_member(self):
        self.settings["before"] = {"model-0": 200, "model-1": 200}
        self.group_definition = {
            "mode": "order", "affinity": "none",
            "members": [{"model": "route-0", "off": True},
                        {"model": "route-1", "off": False}],
        }
        self.start()
        status, body = self.call()
        self.assertEqual(status, 200, body)
        self.assertEqual([v["model"] for v in self.fetches()], ["model-1"])
        self.assertEqual(self.rows("recovery.log"), [])

    def test_duplicate_upstream_alias_is_skipped_before_a_healthy_backup(self):
        self.count = 3
        self.aliases[1] = 0
        self.settings["before"] = {"model-2": 200}
        self.start()
        status, body = self.call()
        self.assertEqual(status, 200, body)
        self.assertEqual([v["model"] for v in self.fetches()], ["model-0", "model-2"])
        self.assertEqual(self.rows("recovery.log"), [])

    def test_duplicate_alias_does_not_prevent_recovery_after_real_candidates(self):
        self.count = 3
        self.aliases[1] = 0
        self.start()
        status, body = self.call()
        self.assertEqual(status, 200, body)
        self.assertEqual([v["model"] for v in self.fetches()],
                         ["model-0", "model-2", "model-0"])
        metadata = json.loads(self.rows("recovery.log")[0])
        self.assertEqual([row["route"] for row in metadata["attempts"]],
                         ["route-0", "route-2"])
        self.assertEqual(self.rows("request.log"), ["request"])

    def test_skipped_alias_does_not_consume_or_extend_generation_attempt_budget(self):
        self.count = 6
        self.aliases[1] = 0
        self.settings["before"] = {"model-5": 200}
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual([v["model"] for v in self.fetches()],
                         ["model-0", "model-2", "model-3", "model-4"])
        self.assertEqual(self.rows("recovery.log"), [])

    def test_recovery_resumes_exact_earlier_route_without_replaying_request_hooks(self):
        self.start()
        status, body = self.call()
        self.assertEqual(status, 200, body)
        self.assertEqual([v["model"] for v in self.fetches()], ["model-0", "model-1", "model-0"])
        self.assertEqual(self.rows("request.log"), ["request"])
        self.assertEqual(self.rows("resume.log"), ["resume"])
        metadata = json.loads(self.rows("recovery.log")[0])
        self.assertEqual(metadata["phase"], "routes_exhausted")
        self.assertEqual([row["account"] for row in metadata["attempts"]],
                         ["configured-account-0", "configured-account-1"])
        self.assertEqual(set(metadata), {"phase", "timeout_ms", "attempts"})
        self.assertNotIn("untrusted", json.dumps(metadata))
        self.assertNotIn("synthetic prompt", json.dumps(metadata))
        for row in metadata["attempts"]:
            self.assertEqual(set(row), {"route", "provider", "account", "model", "status"})
        self.assertGreater(metadata["timeout_ms"], 0)
        self.assertLessEqual(metadata["timeout_ms"], self.timeout)

    def test_recovery_can_select_the_last_actual_candidate(self):
        self.settings["candidate"] = 1
        self.start()
        self.assertEqual(self.call()[0], 200)
        self.assertEqual([v["model"] for v in self.fetches()], ["model-0", "model-1", "model-1"])

    def test_declined_or_unknown_recovery_never_replays(self):
        self.settings["mode"] = "unknown"
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("resume.log"), [])

    def test_unattempted_candidate_is_rejected(self):
        self.settings["candidate"] = 2
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("resume.log"), [])

    def test_malformed_success_is_rejected(self):
        self.settings["mode"] = "malformed"
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("resume.log"), [])

    def test_one_recovery_allows_at_most_one_final_generation(self):
        self.settings["after"] = {"model-0": 429, "model-1": 429}
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 3)
        self.assertEqual(len(self.rows("recovery.log")), 1)

    def test_implicit_account_never_enters_recovery(self):
        self.explicit = False
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("recovery.log"), [])

    def test_transport_failure_does_not_authorize_recovery(self):
        self.settings["fail_head"] = True
        self.start()
        self.assertGreaterEqual(self.call()[0], 500)
        self.assertEqual(len(self.fetches()), 1)
        self.assertEqual(self.rows("recovery.log"), [])

    def test_incomplete_refusal_does_not_authorize_recovery(self):
        self.settings["fail_body"] = True
        self.start()
        self.assertGreaterEqual(self.call()[0], 500)
        self.assertEqual(len(self.fetches()), 1)
        self.assertEqual(self.rows("recovery.log"), [])

    def test_recovery_uses_original_deadline_and_cancels_resource(self):
        self.settings["mode"] = "pending"
        self.timeout = 200
        self.start()
        before = time.monotonic()
        self.call()
        self.assertLess(time.monotonic() - before, 2)
        self.wait_file("cancelled")
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("resume.log"), [])

    def test_disconnect_cancels_recovery_before_another_generation(self):
        self.settings["mode"] = "pending"
        self.start()
        connection = socket.create_connection(("127.0.0.1", self.port), timeout=3)
        try:
            body = json.dumps(self.body()).encode()
            connection.sendall((f"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                                f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n").encode() + body)
            self.wait_file("recovery.log")
            linger = struct.pack("HH" if os.name == "nt" else "ii", 1, 0)
            connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
        finally:
            connection.close()
        self.wait_file("cancelled")
        self.assertEqual(len(self.fetches()), 2)
        self.assertEqual(self.rows("resume.log"), [])

    def test_attempt_cap_cannot_be_extended_by_recovery(self):
        self.count = 6
        self.start()
        self.assertEqual(self.call()[0], 429)
        self.assertEqual(len(self.fetches()), 4)
        self.assertEqual(self.rows("recovery.log"), [])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release"
    prefix, suffix = ("", ".dll") if os.name == "nt" else ("lib", ".dylib" if sys.platform == "darwin" else ".so")
    defaults = {
        "binary": target / ("zcalld.exe" if os.name == "nt" else "zcalld"),
        "fixture-plugin": target / "examples" / (prefix + "recovery_fixture" + suffix),
        "routing-plugin": target / (prefix + "zcalld_plugin_routing" + suffix),
    }
    for name, default in defaults.items():
        parser.add_argument("--" + name, default=default.resolve(), type=lambda value: Path(value).resolve())
    Recovery.args, rest = parser.parse_known_args()
    unittest.main(argv=[__file__, *rest])
