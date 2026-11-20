#!/usr/bin/env python3
"""Exercise actual CLI edits against isolated homes, never the developer's agents."""
import json
import http.server
import threading
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

BINARY = Path(os.environ.get("ZCALLD_TEST_BINARY", "target/debug/zcalld")).resolve()


class ControlTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zcalld-control-")
        self.home = Path(self.temporary.name).resolve()
        self.directory = self.home / ".config/zcalld"
        self.directory.mkdir(parents=True, mode=0o700)
        self.config = self.directory / "config.conf"
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith(("ZCALLD_", "CODEX_", "XDG_")):
                self.env.pop(key)
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home),
                        XDG_CONFIG_HOME=str(self.home / ".config"),
                        APPDATA=str(self.home / ".config"))
        self.config.write_text("[server]\nlisten = 127.0.0.1:39415\nunix_socket = none\n"
                               "[provider fixture]\nbase_url = http://127.0.0.1:39999/v1\n"
                               "protocol = chat\nauth = none\n"
                               "[route fixture/model]\nprovider = fixture\nmodel = model\n", encoding="utf-8", newline="\n")

    def tearDown(self):
        self.temporary.cleanup()

    def cli(self, *args, input=None, ok=True):
        result = subprocess.run([str(BINARY), *args, "--config", str(self.config), "--json"],
                                input=input, text=True, capture_output=True, env=self.env,
                                timeout=20, check=False, encoding="utf-8")
        if ok:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        else:
            self.assertNotEqual(result.returncode, 0)
        return json.loads(result.stdout)

    def test_provider_credentials_are_private_and_invalid_route_is_atomic(self):
        secret = "fixture-private-provider-secret"
        self.cli("providers", "add", "fixture-new", "--base-url", "https://example.invalid/v1",
                 "--api-key-stdin", "--model", "m", input=secret)
        credentials = self.directory / "credentials"
        if os.name == "posix":
            self.assertEqual(credentials.stat().st_mode & 0o777, 0o600)
        listed = self.cli("providers", "list")
        self.assertNotIn(secret, json.dumps(listed))
        old_credentials, old_config = credentials.read_bytes(), self.config.read_bytes()
        self.cli("providers", "edit", "fixture-new", "--api-key-stdin", "--model", "m",
                 "--route", "bad[section]", input="another-private-secret", ok=False)
        self.assertEqual(credentials.read_bytes(), old_credentials)
        self.assertEqual(self.config.read_bytes(), old_config)

    def test_claude_apply_preserves_jsonc_and_revert_checks_external_edits(self):
        path = self.home / ".claude/settings.json"
        path.parent.mkdir()
        original = '{\n // personal note\n "env": {"KEEP":"user-value",},\n "unknown": 1234567890123456789012345,\n}\n'
        path.write_text(original, encoding="utf-8", newline="\n")
        self.cli("agents", "apply", "claude", "--route", "fixture/model")
        written = path.read_text(encoding="utf-8")
        self.assertIn("// personal note", written)
        self.assertIn('"KEEP":"user-value"', written)
        self.assertIn("1234567890123456789012345", written)
        with path.open("ab") as stream:
            stream.write(b"\n// edited afterwards\n")
        self.cli("agents", "revert", "claude", ok=False)
        self.cli("agents", "revert", "claude", "--force")
        self.assertEqual(path.read_bytes(), original.encode("utf-8"))

    def test_codex_apply_preserves_unrelated_tables(self):
        path = self.home / ".codex/config.toml"
        path.parent.mkdir()
        original = '# user comment\nmodel="old" # chosen\n[projects."/project"]\ntrust_level="trusted"\n'
        path.write_text(original, encoding="utf-8", newline="\n")
        self.cli("agents", "apply", "codex", "--route", "fixture/model")
        self.assertIn('[projects."/project"]\ntrust_level="trusted"', path.read_text(encoding="utf-8"))
        self.assertIn('experimental_bearer_token = "zcalld"', path.read_text(encoding="utf-8"))
        self.cli("agents", "revert", "codex")
        self.assertEqual(path.read_bytes(), original.encode("utf-8"))

    def test_editor_catalog_preserves_other_provider_and_comment(self):
        path = self.home / ("Library/Application Support/Code/User/chatLanguageModels.json" if sys.platform == "darwin" else ".config/Code/User/chatLanguageModels.json")
        path.parent.mkdir(parents=True)
        path.write_text('[\n// keep this group\n{"name":"custom","models":[]},\n]\n', encoding="utf-8", newline="\n")
        self.cli("agents", "apply", "vscode", "--route", "fixture/model")
        self.assertIn("// keep this group", path.read_text(encoding="utf-8"))
        self.assertIn('"name":"custom"', path.read_text(encoding="utf-8"))
        self.assertIn('"vendor":"customendpoint"', path.read_text(encoding="utf-8"))
        self.assertIn("requestHeaders", path.read_text(encoding="utf-8"))

    def test_qwen_keeps_custom_entries(self):
        path = self.home / ".qwen/settings.json"
        path.parent.mkdir()
        path.write_text('{"modelProviders":{"openai":[{"id":"custom","envKey":"MY_KEY"}]}}', encoding="utf-8", newline="\n")
        self.cli("agents", "apply", "qwen", "--route", "fixture/model")
        state = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(state["modelProviders"]["openai"][0]["id"], "custom")
        self.assertEqual(len(state["modelProviders"]["openai"]), 2)

    def test_privacy_off_keeps_rules_and_key(self):
        directory = self.directory / "privacy"
        directory.mkdir()
        path = directory / "settings.json"
        path.write_text(json.dumps({"redactPersonal": True, "redactModelAuto": True,
                                    "redactWords": ["private-word"], "futureField": {"kept": True}}), encoding="utf-8", newline="\n")
        key = directory / "redact.key"
        key.write_bytes(bytes(range(32)))
        state = self.cli("privacy", "set", "off")
        self.assertEqual(state["enabled"], False)
        kept = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(kept["redactWords"], ["private-word"])
        self.assertEqual(kept["futureField"], {"kept": True})
        self.assertEqual(key.read_bytes(), bytes(range(32)))
        before = path.read_bytes(), key.read_bytes()
        self.env["ZCALLD_PRIVACY_WORKER"] = str(self.home / "missing-privacy-worker")
        self.cli("privacy", "set", "personal", ok=False)
        self.assertEqual((path.read_bytes(), key.read_bytes()), before)
        self.assertFalse((directory / "plugin.json").exists())
        self.assertFalse(self.cli("privacy", "show")["configured"])

    def test_custom_agent_refuses_path_escape(self):
        definition = self.home / "adapter.json"
        definition.write_text(json.dumps({"name":"external", "files":[{"path":"../../etc/config", "bindings":[]}]}), encoding="utf-8", newline="\n")
        self.cli("agents", "catalog", "--file", str(definition), ok=False)
        self.assertFalse((self.directory / "agents.d/external.json").exists())

    def test_library_apply_keeps_existing_servers(self):
        (self.directory / "library.json").write_text(json.dumps({"mcp":[{"name":"fixture", "transport":"stdio", "command":"echo", "args":["hi"], "env":{"KEEP":"secret"}}]}), encoding="utf-8", newline="\n")
        path = self.home / ".gemini/settings.json"
        path.parent.mkdir()
        path.write_text('{"mcpServers":{"existing":{"command":"true"}},"unrelated":7}', encoding="utf-8", newline="\n")
        self.cli("library", "apply", "--agent", "gemini")
        data = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(data["mcpServers"]["existing"]["command"], "true")
        self.assertEqual(data["mcpServers"]["fixture"]["env"]["KEEP"], "secret")
        self.assertNotIn("secret", json.dumps(self.cli("library", "list")))


    def test_group_creates_selectable_route_and_preserves_rules(self):
        self.cli("groups", "set", "group/work", "--members", "fixture/model", "--mode", "rotate")
        route = next(r for r in self.cli("routes", "list")["routes"] if r["name"] == "group/work")
        self.assertEqual(route["provider"], "fixture")
        path = self.directory / "routing.json"
        state = json.loads(path.read_text(encoding="utf-8"))
        state["groups"][0]["original"] = {"affinity": "turn", "rules": [{"when": "compact", "useEffort": "low"}]}
        path.write_text(json.dumps(state), encoding="utf-8", newline="\n")
        self.cli("groups", "set", "group/work", "--members", "fixture/model", "--mode", "order")
        self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["groups"][0]["original"], state["groups"][0]["original"])
        before = self.config.read_bytes(), path.read_bytes()
        self.cli("groups", "set", "group/work", "--members", "group/work", ok=False)
        self.assertEqual((self.config.read_bytes(), path.read_bytes()), before)
        self.cli("groups", "remove", "group/work")
        self.assertNotIn("group/work", [r["name"] for r in self.cli("routes", "list")["routes"]])

    def test_profile_without_credentials_clears_stale_private_state(self):
        self.cli("profiles", "save", "empty")
        credentials = self.directory / "credentials"
        credentials.write_text('ZCALLD_UNUSED_KEY="fixture-private"\n', encoding="utf-8", newline="\n")
        privacy = self.directory / "privacy/settings.json"
        privacy.parent.mkdir()
        privacy.write_text('{"redactWords":["fixture-private"]}', encoding="utf-8", newline="\n")
        self.cli("profiles", "save", "with-secret")
        self.cli("profiles", "use", "empty")
        self.assertFalse(credentials.exists())
        self.assertFalse(privacy.exists())
        self.cli("profiles", "save", "with-secret")
        self.assertFalse((self.directory / "profiles/with-secret/credentials").exists())


    def test_migrated_usage_and_group_listing_do_not_leak_credentials(self):
        (self.directory / "usage.jsonl").write_text(
            '{"in":17,"out":9,"status":200}\n{"input_tokens":3,"output_tokens":4,"status":503}\n', encoding="utf-8", newline="\n")
        totals = self.cli("usage", "summary")
        self.assertEqual((totals["requests"], totals["failed"], totals["input_tokens"], totals["output_tokens"]), (2, 1, 20, 13))
        (self.directory / "routing.json").write_text(json.dumps({"groups": [], "deciders": [
            {"id":"classifier", "url":"http://127.0.0.1", "key":"fixture-secret", "protocol":"chat"}]}), encoding="utf-8", newline="\n")
        listed = self.cli("groups", "list")
        self.assertNotIn("fixture-secret", json.dumps(listed))
        self.assertTrue(listed["deciders"][0]["credential_configured"])


    def test_usage_record_size_limit_and_line_endings(self):
        limit = 1024 * 1024
        record = b'{"input_tokens":3,"output_tokens":4,"status":503}'
        prefix = b'{"in":2,"out":1,"status":200}\r\n\nnot-json\n'
        following = b'{"in":5,"out":6,"status":200}'
        for ending in (b"\n", b"\r\n", b""):
            for size in (limit - 1, limit, limit + 1):
                with self.subTest(ending=ending, size=size):
                    (self.directory / "usage.jsonl").write_bytes(
                        prefix + record + b" " * (size - len(record)) + ending
                        + (following if ending else b""))
                    totals = self.cli("usage", "summary")
                    included = int(size <= limit)
                    after = int(bool(ending))
                    self.assertEqual(
                        tuple(totals[key] for key in
                              ("requests", "failed", "input_tokens", "output_tokens")),
                        (1 + included + after, included,
                         2 + 3 * included + 5 * after, 1 + 4 * included + 6 * after))

    def test_usage_invalid_utf8_is_an_error(self):
        (self.directory / "usage.jsonl").write_bytes(b'{"in":1}\n\xff\n')
        self.cli("usage", "summary", ok=False)

    @unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux address-space limits")
    def test_usage_large_record_has_bounded_memory(self):
        # Limit only this fixture child; a line larger than its entire address
        # space must be discarded without being allocated in full.
        with (self.directory / "usage.jsonl").open("wb") as history:
            chunk = b" " * (1024 * 1024)
            for _ in range(128):
                history.write(chunk)
            history.write(b'\n{"in":7,"out":11,"status":200}\n')
            history.write(chunk)
            history.write(b" ")
        runner = (
            "import os,resource,sys; "
            "resource.setrlimit(resource.RLIMIT_CORE,(0,0)); "
            "resource.setrlimit(resource.RLIMIT_AS,(96*1024*1024,)*2); "
            "os.execv(sys.argv[1],sys.argv[1:])"
        )
        result = subprocess.run(
            [sys.executable, "-c", runner, str(BINARY), "usage", "summary",
             "--config", str(self.config), "--json"],
            text=True, capture_output=True, env=self.env, timeout=20, check=False)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        totals = json.loads(result.stdout)
        self.assertEqual(
            tuple(totals[key] for key in
                  ("requests", "failed", "input_tokens", "output_tokens")),
            (1, 0, 7, 11))

    def test_usage_export_missing_source_leaves_destination_available(self):
        target = self.home / "usage-export.jsonl"
        self.cli("usage", "export", "--file", str(target), ok=False)
        self.assertFalse(target.exists())
        original = b'{"in":17,"out":9,"status":200}\r\nnot-json\n'
        (self.directory / "usage.jsonl").write_bytes(original)
        self.assertTrue(self.cli("usage", "export", "--file", str(target))["ok"])
        self.assertEqual(target.read_bytes(), original)
        if os.name == "posix":
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)

    def test_usage_export_does_not_overwrite_existing_destination(self):
        original = b'{"in":1}\n'
        target = self.home / "usage-export.jsonl"
        target.write_bytes(b"keep existing contents")
        (self.directory / "usage.jsonl").write_bytes(original)
        self.cli("usage", "export", "--file", str(target), ok=False)
        self.assertEqual(target.read_bytes(), b"keep existing contents")
        self.assertEqual((self.directory / "usage.jsonl").read_bytes(), original)


    def test_provider_discovery_accepts_complete_generation_endpoint(self):
        seen = []
        class Upstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                seen.append(self.path)
                body = b'{"data":[{"id":"discovered"}]}'
                self.send_response(200 if self.path == "/v1/models" else 404)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        try:
            self.cli("providers", "edit", "fixture", "--base-url",
                     f"http://127.0.0.1:{upstream.server_port}/v1/chat/completions")
            result = self.cli("providers", "models", "fixture")
            self.assertEqual(result["catalog"]["data"][0]["id"], "discovered")
            self.assertEqual(seen, ["/v1/models"])
        finally:
            upstream.shutdown()
            upstream.server_close()
            thread.join(timeout=2)

    def test_migration_dry_run_does_not_change_originals(self):
        source = self.home / ".config/magpie"
        source.mkdir()
        (source / "settings.json").write_text("{}", encoding="utf-8", newline="\n")
        (source / "providers.json").write_text('{"providers":[{"id":"local","chat":"http://127.0.0.1:11434/v1","key":""}]}', encoding="utf-8", newline="\n")
        catalog = self.home / "catalog.json"
        catalog.write_text('{"data":[{"id":"local/model","context_window":32768},{"id":"unknown/model"}]}', encoding="utf-8", newline="\n")
        before = {p.name: p.read_bytes() for p in source.iterdir()}
        result = self.cli("migrate", "magpie", "--source", str(source), "--catalog", str(catalog))
        self.assertFalse(result["ready"])
        self.assertEqual(result["summary"]["unresolved_models"], 1)
        self.assertEqual(before, {p.name: p.read_bytes() for p in source.iterdir()})


if __name__ == "__main__":
    unittest.main(verbosity=2)
