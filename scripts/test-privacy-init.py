#!/usr/bin/env python3
"""Validate first-time privacy activation with native plugins and a local echo server."""
import argparse
import http.client
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("transport_fixture", Path(__file__).with_name("test-transport.py"))
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)


class PrivacyInitTests(transport.TransportTests):
    def setUp(self):
        super().setUp()
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith(("ZCALLD_", "CODEX_", "XDG_"))}
        environment.update(HOME=str(self.root), USERPROFILE=str(self.root),
                           XDG_CONFIG_HOME=str(self.root / ".config"),
                           APPDATA=str(self.root / "AppData/Roaming"),
                           ZCALLD_REDACT_PLUGIN=str(self.redact_plugin),
                           ZCALLD_PRIVACY_WORKER=str(self.privacy_worker))
        self.environment = mock.patch.dict(os.environ, environment, clear=True)
        self.environment.start()
        self.write_config()
        self.upstream.mode = "echo"

    def tearDown(self):
        try:
            super().tearDown()
        finally:
            self.environment.stop()

    def cli(self, *args, ok=True):
        result = subprocess.run([str(self.binary), *args, "--config", str(self.config), "--json"],
                                capture_output=True, text=True, timeout=25, check=False, encoding="utf-8")
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return json.loads(result.stdout or "{}")

    def echo(self, original, stream=False, headers=None):
        status, body = transport.request(
            http.client.HTTPConnection("127.0.0.1", self.port, timeout=5),
            "/v1/chat/completions",
            {"model": "test", "messages": [{"role": "user", "content": original}], "stream": stream},
            headers=headers,
        )
        self.assertEqual(status, 200, body.decode(errors="replace"))
        if stream:
            restored = "".join(json.loads(line[6:])["choices"][0]["delta"].get("content", "")
                               for line in body.decode().splitlines()
                               if line.startswith("data: ") and line != "data: [DONE]")
        else:
            restored = json.loads(body)["choices"][0]["message"]["content"]
        self.assertEqual(restored, original)
        return self.upstream.requests[-1]["body"]["messages"][0]["content"]

    def test_first_enable_masks_and_off_preserves_key_binding_and_rules(self):
        state = self.cli("privacy", "show")
        self.assertFalse(state["configured"])
        self.assertFalse(state["enabled"])
        self.assertIsNone(state["restart_required"])
        state = self.cli("privacy", "set", "secrets")
        self.assertTrue(state["configured"])
        self.assertTrue(state["enabled"])
        self.assertTrue(state["restart_required"])
        directory = self.root / "privacy"
        binding_path = directory / "plugin.json"
        original_binding = binding_path.read_bytes()
        binding = json.loads(original_binding)
        self.assertEqual(Path(binding["history_dir"]), directory / "history")
        key = Path(binding["key"])
        original_key = key.read_bytes()
        self.assertEqual(len(original_key), 32)
        if os.name == "posix":
            self.assertEqual(key.stat().st_mode & 0o777, 0o600)
        checked = subprocess.run([str(self.binary), "check", "--config", str(self.config)],
                                 capture_output=True, text=True, timeout=10, check=False, encoding="utf-8")
        self.assertEqual(checked.returncode, 0, checked.stdout + checked.stderr)
        self.start()
        # Public, synthetic key from the vendored redactor's test corpus.
        secret = "sk-proj-Wn4rT7yQ2mLx9VbK3cZp"
        outgoing = self.echo("Use this fixture key " + secret)
        self.assertNotIn(secret, outgoing)
        self.assertIn("{{", outgoing)

        settings_path = Path(binding["settings"])
        settings = json.loads(settings_path.read_text(encoding="utf-8"))
        settings.update(redactWords=["vault-fixture"], futureField={"preserved": True})
        settings_path.write_text(json.dumps(settings), encoding="utf-8", newline="\n")
        os.environ["ZCALLD_PRIVACY_WORKER"] = str(self.root / "missing-worker")
        # An existing binding remains authoritative; a new discovery override
        # does not silently rewrite the migrated runtime.
        enabled = self.cli("privacy", "set", "personal")
        self.assertTrue(enabled["enabled"])
        self.assertFalse(enabled["restart_required"])
        self.assertEqual(binding_path.read_bytes(), original_binding)
        self.assertEqual(key.read_bytes(), original_key)
        for stream in (False, True):
            outgoing = self.echo("Contact jane.doe@acme.io about vault-fixture.", stream)
            self.assertNotIn("jane.doe@acme.io", outgoing)
            self.assertNotIn("vault-fixture", outgoing)

        self.assertFalse(self.cli("privacy", "set", "off")["enabled"])
        self.assertEqual(self.echo("vault-fixture " + secret), "vault-fixture " + secret)
        kept = json.loads(settings_path.read_text(encoding="utf-8"))
        self.assertEqual(kept["redactWords"], ["vault-fixture"])
        self.assertEqual(kept["futureField"], {"preserved": True})
        self.assertEqual(key.read_bytes(), original_key)
        self.assertEqual(binding_path.read_bytes(), original_binding)

    def test_private_audit_records_and_counters_survive_graceful_shutdown(self):
        self.cli("privacy", "set", "secrets")
        binding = json.loads((self.root / "privacy/plugin.json").read_text(encoding="utf-8"))
        history = Path(binding["history_dir"])
        self.assertEqual(list(history.glob("*.jsonl")), [])
        self.assertEqual(list(history.glob("checked-*.json")), [])
        process = self.start()
        headers = {"X-Agent-Name": "privacy-audit-fixture",
                   "X-Session-Id": "privacy-audit-session"}
        secret = "sk-proj-Wn4rT7yQ2mLx9VbK3cZp"
        original = "Use this fixture key " + secret
        for stream in (False, True):
            outgoing = self.echo(original, stream, headers=headers)
            self.assertNotIn(secret, outgoing)
        self.assertEqual(self.echo("Ordinary synthetic text.", headers=headers),
                         "Ordinary synthetic text.")
        self.cli("privacy", "set", "off")
        self.assertEqual(self.echo(original, headers=headers), original)
        self.stop(process)

        files = sorted(history.glob("????-??-??.jsonl"))
        records = [json.loads(line) for path in files
                   for line in path.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(records), 2)
        for record in records:
            self.assertEqual(record["agent"], "privacy-audit-fixture")
            self.assertEqual(record["session"], "privacy-audit-session")
            self.assertEqual(record["what"], "/v1/chat/completions")
            self.assertGreater(record["n"], 0)
            self.assertTrue(record["values"])
            for field in ("body", "original", "Snips", "Back", "Sent"):
                self.assertNotIn(field, record)
        checked_files = sorted(history.glob("checked-????-??-??.json"))
        self.assertTrue(checked_files, "shutdown must flush pending audit counters")
        counters = [json.loads(path.read_text(encoding="utf-8"))
                    for path in checked_files]
        self.assertTrue(all(set(value) == {"privacy-audit-fixture"} for value in counters))
        self.assertEqual(sum(value["privacy-audit-fixture"].get("looked", 0)
                             for value in counters), 3)
        self.assertEqual(sum(value["privacy-audit-fixture"].get("masked", 0)
                             for value in counters), 2)
        self.assertEqual(sum(value["privacy-audit-fixture"].get("whole", 0)
                             for value in counters), 0)
        for path in [*files, *checked_files]:
            raw = path.read_text(encoding="utf-8")
            self.assertNotIn(secret, raw)
            self.assertNotIn(original, raw)
            if os.name == "posix":
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        if os.name == "posix":
            self.assertEqual(history.stat().st_mode & 0o777, 0o700)

    def test_missing_runtime_and_missing_original_key_fail_without_enabling(self):
        os.environ["ZCALLD_PRIVACY_WORKER"] = str(self.root / "missing-worker")
        self.cli("privacy", "set", "personal", ok=False)
        directory = self.root / "privacy"
        self.assertFalse((directory / "settings.json").exists())
        self.assertFalse((directory / "plugin.json").exists())

        directory.mkdir(exist_ok=True)
        settings = directory / "settings.json"
        settings.write_text(json.dumps({"zcalldEnabled": False, "redactWords": ["keep-rule"]}), encoding="utf-8", newline="\n")
        binding = directory / "plugin.json"
        missing_key = directory / "old-key-missing"
        binding.write_text(json.dumps({"worker": str(self.privacy_worker),
                                       "settings": str(settings), "key": str(missing_key)}), encoding="utf-8", newline="\n")
        before = settings.read_bytes(), binding.read_bytes()
        self.cli("privacy", "set", "secrets", ok=False)
        self.assertEqual((settings.read_bytes(), binding.read_bytes()), before)
        self.assertFalse(missing_key.exists())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/zcalld"))
    parser.add_argument("--redact-plugin", type=Path, required=True)
    parser.add_argument("--privacy-worker", type=Path, required=True)
    args = parser.parse_args()
    for name in ("binary", "redact_plugin", "privacy_worker"):
        path = getattr(args, name).resolve(strict=True)
        setattr(PrivacyInitTests, name, path)
    names = [name for name in PrivacyInitTests.__dict__ if name.startswith("test_")]
    suite = unittest.TestSuite(PrivacyInitTests(name) for name in names)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
