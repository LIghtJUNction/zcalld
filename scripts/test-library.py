#!/usr/bin/env python3
"""Exercise library CRUD and native-file application in disposable agent homes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

BINARY = Path(os.environ.get("ZCALLD_TEST_BINARY", "target/debug/zcalld")).resolve()


class LibraryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-library-")
        self.home = Path(self.temp.name)
        self.directory = self.home / ".config/zcalld"
        self.directory.mkdir(parents=True)
        self.config = self.directory / "config.conf"
        self.config.write_text("[server]\nlisten = 127.0.0.1:39415\n")
        self.env = {k: v for k, v in os.environ.items() if not k.startswith((
            "ZCALLD_", "CODEX_", "XDG_", "CLAUDE_", "COPILOT_", "OPENCODE_", "PI_", "OMP_", "OMO_", "SENPI_", "GROK_", "DSH_", "QWEN_"))}
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home),
                        XDG_CONFIG_HOME=str(self.home / ".config"),
                        APPDATA=str(self.home / ".config"),
                        LOCALAPPDATA=str(self.home / "AppData/Local"))

    def tearDown(self):
        self.temp.cleanup()

    def cli(self, *args, ok=True):
        result = subprocess.run([str(BINARY), "library", *map(str, args),
                                 "--config", str(self.config), "--json"],
                                env=self.env, capture_output=True, text=True, timeout=20)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return json.loads(result.stdout) if result.stdout.strip() else {"error": result.stderr}

    def write(self, path, text):
        path = self.home / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def state(self):
        return json.loads((self.directory / "library.json").read_text())

    def mcp(self, name="fixture", **kwargs):
        entry = {"name": name, "transport": "stdio", "command": "echo", "args": ["hi"], **kwargs}
        file = self.write("input.json", json.dumps(entry))
        self.cli("mcp", "set", "--file", file)
        return entry

    def seed_instructions(self, agent="claude"):
        file = self.write("prompt.md", "shared text\n")
        self.cli("instructions", "set", "default", "--file", file, "--agents", agent)

    def seed_skill(self, agent="claude"):
        self.write("source/SKILL.md", "---\nname: fixture\ndescription: local test\n---\nSkill body\n")
        helper = self.write("source/scripts/tool.sh", "#!/bin/sh\nprintf fixture\n")
        if os.name != "nt":
            helper.chmod(0o700)
        (self.home / "source/data.bin").write_bytes(bytes(range(256)))
        self.cli("skills", "install", "fixture", "--dir", self.home / "source", "--agents", agent)

    def test_mcp_crud_assignment_and_private_listing(self):
        secret = "library-fixture-private-value"
        self.mcp(env={"TOKEN": secret}, agents=["gemini"], note="retain metadata")
        self.assertNotIn(secret, json.dumps(self.cli("list")))
        self.assertNotIn(secret, json.dumps(self.cli("mcp", "show", "fixture")))
        exported = self.home / "private-entry.json"
        self.cli("mcp", "show", "fixture", "--file", exported)
        self.assertEqual(json.loads(exported.read_text())["env"]["TOKEN"], secret)
        self.cli("apply", "--agent", "claude")
        self.assertFalse((self.home / ".claude.json").exists())
        self.cli("mcp", "assign", "fixture", "--agents", "claude,gemini")
        self.cli("apply", "--agent", "claude")
        self.assertEqual(json.loads((self.home / ".claude.json").read_text())["mcpServers"]["fixture"]["type"], "stdio")
        self.cli("mcp", "disable", "fixture")
        self.cli("mcp", "apply", "--agent", "claude")
        self.assertEqual(json.loads((self.home / ".claude.json").read_text())["mcpServers"], {})
        self.cli("mcp", "enable", "fixture")
        self.assertEqual(self.state()["mcp"][0]["note"], "retain metadata")
        self.cli("mcp", "remove", "fixture")
        self.assertEqual(self.cli("mcp", "list")["mcp"], [])

    def test_jsonc_apply_remove_preserves_unrelated_bytes(self):
        path = self.write(".gemini/settings.json", '{\n // keep this comment\n "mcpServers": {"existing":{"command":"true"}},\n "large":9007199254740993, "other":[1,2,3]\n}\n')
        self.mcp(agents=["gemini"])
        self.cli("apply", "--agent", "gemini")
        self.cli("mcp", "remove", "fixture")
        self.cli("apply", "--agent", "gemini")
        text = path.read_text()
        for marker in ['// keep this comment', '"existing":{"command":"true"}', '9007199254740993', '"other":[1,2,3]']:
            self.assertIn(marker, text)
        self.assertNotIn('"fixture"', text)

    def test_remote_transport_matrix(self):
        self.mcp(transport="http", url="https://example.invalid/mcp", headers={"X-Fixture": "literal"})
        matrix = {
            "gemini": (".gemini/settings.json", "mcpServers", "httpUrl"),
            "qwen": (".qwen/settings.json", "mcpServers", "httpUrl"),
            "antigravity": (".gemini/config/mcp_config.json", "mcpServers", "serverUrl"),
            "claude": (".claude.json", "mcpServers", "url"),
            "copilot": (".copilot/mcp-config.json", "mcpServers", "url"),
            "cursor": (".cursor/mcp.json", "mcpServers", "url"),
            "pi": (".pi/agent/mcp.json", "mcpServers", "url"),
            "omp": (".omp/agent/mcp.json", "mcpServers", "url"),
            "omo": (".omo/agent/mcp.json", "mcpServers", "url"),
            "opencode": (".config/opencode/opencode.json", "mcp", "url"),
            "crush": ("AppData/Local/crush/crush.json" if os.name == "nt" else ".config/crush/crush.json", "mcp", "url"),
            "zed": (".config/Zed/settings.json" if os.name == "nt" else ".config/zed/settings.json", "context_servers", "url"),
            "vscode": ("Library/Application Support/Code/User/mcp.json" if sys.platform == "darwin" else ".config/Code/User/mcp.json", "servers", "url"),
        }
        for agent, (file, key, url_key) in matrix.items():
            with self.subTest(agent=agent):
                self.cli("apply", "--agent", agent)
                item = json.loads((self.home / file).read_text())[key]["fixture"]
                self.assertEqual(item[url_key], "https://example.invalid/mcp")
                self.assertEqual(item["headers"], {"X-Fixture": "literal"})
                self.assertNotIn("transport", item)
        self.cli("apply", "--agent", "claude-desktop", ok=False)

    def test_codex_and_grok_toml_preserve_other_tables(self):
        source = '# manual comment\nmodel="native-model"\n[projects."/tmp/project"]\ntrust_level="trusted"\n'
        codex = self.write(".codex/config.toml", source)
        grok = self.write(".grok/config.toml", source)
        self.mcp(transport="http", url="https://example.invalid/mcp", startupTimeout=45,
                 headers={"Authorization": "Bearer ${API_TOKEN}", "X-Other": "${HEADER_TOKEN}"})
        self.cli("apply", "--agent", "codex")
        current = tomllib.loads(codex.read_text())
        item = current["mcp_servers"]["fixture"]
        self.assertEqual(item["bearer_token_env_var"], "API_TOKEN")
        self.assertEqual(item["env_http_headers"]["X-Other"], "HEADER_TOKEN")
        self.assertEqual(item["startup_timeout_sec"], 45)
        self.assertEqual(current["model"], "native-model")
        self.cli("apply", "--agent", "grok")
        self.assertEqual(tomllib.loads(grok.read_text())["mcp_servers"]["fixture"]["headers"]["Authorization"], "Bearer ${API_TOKEN}")
        self.cli("mcp", "disable", "fixture")
        self.cli("apply", "--agent", "codex")
        self.assertNotIn("mcp_servers", tomllib.loads(codex.read_text()))
        self.assertTrue(codex.read_text().startswith(source))

    def test_unowned_collisions_and_explicit_equal_adoption(self):
        self.mcp()
        path = self.write(".gemini/settings.json", '{"mcpServers":{"fixture":{"command":"different"}},"keep":1}')
        original = path.read_bytes()
        self.cli("apply", "--agent", "gemini", ok=False)
        self.assertEqual(path.read_bytes(), original)
        path.write_text('{"mcpServers":{"fixture":{"command":"echo","args":["hi"]}},"keep":1}')
        result = self.cli("apply", "--agent", "gemini")
        self.assertEqual(result["unowned_equal_servers"], 1)
        self.cli("apply", "--agent", "gemini", "--adopt-existing")
        self.cli("revert", "--agent", "gemini")
        self.assertEqual(json.loads(path.read_text()), {"mcpServers": {}, "keep": 1})

    def test_external_mcp_edits_stop_whole_apply(self):
        self.mcp(agents=["claude"])
        self.seed_instructions()
        self.cli("apply", "--agent", "claude")
        mcp = self.home / ".claude.json"
        data = json.loads(mcp.read_text())
        data["mcpServers"]["fixture"]["command"] = "manual"
        mcp.write_text(json.dumps(data))
        before = (self.home / ".claude/CLAUDE.md").read_bytes()
        self.write("prompt.md", "replacement shared prompt")
        self.cli("instructions", "set", "default", "--file", self.home / "prompt.md")
        self.cli("apply", "--agent", "claude", ok=False)
        self.assertEqual((self.home / ".claude/CLAUDE.md").read_bytes(), before)
        self.assertEqual(json.loads(mcp.read_text())["mcpServers"]["fixture"]["command"], "manual")

    def test_opencode_v2_layout_and_env_reference_conversion(self):
        path = self.write(".config/opencode/opencode.jsonc", '{// keep\n"mcp":{"servers":{"other":{"type":"remote","url":"https://other.invalid"}}},"theme":"system"}')
        self.mcp(env={"TOKEN": "${TOKEN}"})
        self.cli("apply", "--agent", "opencode")
        data = json.loads(path.read_text().replace("// keep", ""))
        item = data["mcp"]["servers"]["fixture"]
        self.assertEqual(item["command"], ["echo", "hi"])
        self.assertEqual(item["environment"], {"TOKEN": "{env:TOKEN}"})
        self.assertNotIn("enabled", item)
        self.cli("revert", "--agent", "opencode")
        self.assertIn('"other":{"type":"remote","url":"https://other.invalid"}', path.read_text())

    def test_opencode_server_named_servers_is_not_a_v2_container(self):
        path = self.write(".config/opencode/opencode.json", '{"mcp":{"servers":{"command":["manual-tool"]}}}')
        self.mcp()
        self.cli("apply", "--agent", "opencode")
        data = json.loads(path.read_text())["mcp"]
        self.assertEqual(data["servers"], {"command": ["manual-tool"]})
        self.assertEqual(data["fixture"]["command"], ["echo", "hi"])

    def test_instruction_crud_sets_extras_apply_and_revert(self):
        self.seed_instructions("claude,codex")
        original = self.write(".claude/CLAUDE.md", "User before\n")
        extra = self.write("extra.md", "Claude-only additions")
        self.cli("instructions", "extra", "--agent", "claude", "--file", extra)
        preview = self.cli("apply", "--agent", "claude", "--dry-run")
        self.assertTrue(preview["dry_run"])
        self.assertEqual(original.read_text(), "User before\n")
        self.assertFalse((self.directory / "library-managed.json").exists())
        self.cli("apply", "--agent", "claude")
        self.assertIn("shared text\n\nClaude-only additions", original.read_text())
        self.cli("apply", "--agent", "codex")
        self.assertNotIn("Claude-only", (self.home / ".codex/AGENTS.md").read_text())
        second = self.write("second.md", "second set")
        self.cli("instructions", "create", "second", "--file", second, "--name", "Second")
        self.cli("instructions", "rename", "second", "--name", "Renamed")
        self.cli("instructions", "use", "second")
        self.cli("instructions", "apply", "--agent", "claude")
        self.assertIn("second set", original.read_text())
        self.assertNotIn("shared text", original.read_text())
        original.write_text(original.read_text() + "User after\n")
        self.cli("instructions", "disable", "--agent", "claude")
        self.cli("instructions", "apply", "--agent", "claude")
        self.assertIn("User before", original.read_text())
        self.assertIn("User after", original.read_text())
        self.assertNotIn("zcalld:library", original.read_text())
        self.cli("instructions", "remove", "second")
        self.assertEqual(self.cli("instructions", "list")["active"], "default")
        self.cli("instructions", "remove", "default", ok=False)

    def test_instruction_override_external_edit_and_missing_link_fail(self):
        self.seed_instructions("codex,claude")
        self.write(".codex/AGENTS.override.md", "override")
        self.cli("apply", "--agent", "codex", ok=False)
        self.assertFalse((self.home / ".codex/AGENTS.md").exists())
        self.cli("apply", "--agent", "claude")
        target = self.home / ".claude/CLAUDE.md"
        target.write_text(target.read_text().replace("shared text", "manual edited block"))
        self.cli("revert", "--agent", "claude", ok=False)
        self.assertIn("manual edited block", target.read_text())
        source = self.write("linked.md", "linked prompt")
        self.cli("instructions", "link", "linked", "--file", source)
        self.cli("instructions", "use", "linked")
        source.unlink()
        self.cli("apply", "--agent", "claude", ok=False)
        self.assertIn("manual edited block", target.read_text())

    def test_skill_assets_execution_bits_update_and_disable(self):
        self.seed_skill()
        self.cli("apply", "--agent", "claude")
        target = self.home / ".claude/skills/fixture"
        self.assertEqual((target / "data.bin").read_bytes(), bytes(range(256)))
        if os.name != "nt":
            self.assertEqual((target / "scripts/tool.sh").stat().st_mode & 0o777, 0o700)
        self.write("source/references/detail.md", "reference material")
        self.cli("skills", "update", "fixture")
        self.cli("skills", "apply", "--agent", "claude")
        self.assertEqual((target / "references/detail.md").read_text(), "reference material")
        self.write(".claude/skills/manual/SKILL.md", "user skill")
        self.cli("skills", "disable", "fixture")
        self.cli("skills", "apply", "--agent", "claude")
        self.assertFalse((target / "SKILL.md").exists())
        self.assertEqual((self.home / ".claude/skills/manual/SKILL.md").read_text(), "user skill")
        self.cli("skills", "enable", "fixture")
        self.cli("skills", "apply", "--agent", "claude")
        self.cli("skills", "remove", "fixture")
        self.cli("skills", "apply", "--agent", "claude")
        self.assertFalse((target / "SKILL.md").exists())

    def test_skill_create_read_write_and_assignment(self):
        source = self.write("new.md", "# New skill\n")
        self.cli("skills", "create", "new", "--file", source)
        self.assertEqual(self.cli("skills", "read", "new")["text"], "# New skill\n")
        self.cli("skills", "assign", "new", "--agents", "codex")
        self.cli("skills", "write", "new", "--file", "references/new.md", "--input", source)
        self.assertIn("references/new.md", self.cli("skills", "show", "new")["files"])
        self.cli("apply", "--agent", "claude")
        self.assertFalse((self.home / ".claude/skills/new").exists())
        self.cli("apply", "--agent", "codex")
        self.assertTrue((self.home / ".codex/skills/new/references/new.md").exists())
        self.cli("skills", "write", "new", "--file", "../escape", "--input", source, ok=False)
        self.assertFalse((self.directory / "library/skills/escape").exists())

    def test_skill_external_changes_never_get_overwritten_or_removed(self):
        self.seed_skill()
        self.cli("apply", "--agent", "claude")
        target = self.write(".claude/skills/fixture/user-note.md", "added by user")
        self.cli("skills", "disable", "fixture")
        self.cli("apply", "--agent", "claude", ok=False)
        self.assertEqual(target.read_text(), "added by user")
        self.assertTrue((target.parent / "SKILL.md").exists())

    @unittest.skipIf(os.name == "nt", "symlinks need Windows developer privileges")
    def test_skill_symlinks_internal_copy_escape_and_cycle(self):
        self.seed_skill()
        (self.home / "source/link.md").symlink_to("SKILL.md")
        self.cli("skills", "update", "fixture")
        stored = self.directory / "library/skills/fixture/link.md"
        self.assertFalse(stored.is_symlink())
        self.assertEqual(stored.read_bytes(), (stored.parent / "SKILL.md").read_bytes())
        outside = self.write("outside.md", "must not enter skill")
        (self.home / "source/escape.md").symlink_to(outside)
        before = (self.directory / "library.json").read_bytes()
        self.cli("skills", "update", "fixture", ok=False)
        self.assertEqual((self.directory / "library.json").read_bytes(), before)
        (self.home / "source/escape.md").unlink()
        (self.home / "source/cycle").symlink_to(self.home / "source", target_is_directory=True)
        self.cli("skills", "update", "fixture", ok=False)

    @unittest.skipIf(os.name == "nt", "symlinks need Windows developer privileges")
    def test_symlinked_agent_path_is_rejected_before_any_apply(self):
        self.seed_instructions()
        outside = self.home / "elsewhere"
        outside.mkdir()
        (self.home / ".claude").symlink_to(outside, target_is_directory=True)
        self.cli("apply", "--agent", "claude", ok=False)
        self.assertEqual(list(outside.iterdir()), [])

    def test_import_export_assets_and_unknown_metadata(self):
        self.seed_instructions()
        self.seed_skill()
        current = self.state()
        current["future-setting"] = {"preserve": True}
        source = self.write("incoming.json", json.dumps(current))
        self.cli("import", "--file", source)
        metadata = self.home / "export.json"
        assets = self.home / "export-assets"
        self.cli("export", "--file", metadata, "--assets", assets)
        self.assertTrue(json.loads(metadata.read_text())["future-setting"]["preserve"])
        self.assertEqual((assets / "instructions.md").read_text(), "shared text\n")
        self.assertEqual((assets / "skills/fixture/data.bin").read_bytes(), bytes(range(256)))
        self.assertEqual(self.cli("list")["skills"], 1)
        if os.name != "nt":
            self.assertEqual(metadata.stat().st_mode & 0o777, 0o600)

    def test_invalid_state_and_duplicate_entries_are_not_reset(self):
        path = self.directory / "library.json"
        path.write_text("{ invalid")
        self.cli("list", ok=False)
        self.assertEqual(path.read_text(), "{ invalid")
        path.unlink()
        bad = self.write("duplicate.json", json.dumps({"mcp": [
            {"name": "same", "command": "echo"}, {"name": "same", "command": "true"}]}))
        self.cli("import", "--file", bad, ok=False)
        self.assertFalse(path.exists())

    def test_pi_extension_layout_and_conflicts(self):
        self.write(".pi/agent/settings.json", '{"packages":["npm:pi-mcp-adapter@5.1.0"]}')
        self.mcp(transport="http", url="https://example.invalid/mcp")
        self.cli("apply", "--agent", "pi")
        self.assertFalse((self.home / ".pi/agent/mcp.json").exists())
        item = json.loads((self.home / ".pi/agent/mcp-adapter.json").read_text())["mcpServers"]["fixture"]
        self.assertEqual(item["httpTransport"], "streamable-http")
        self.write(".pi/agent/settings.json", '{"packages":["npm:pi-mcp-adapter@5.1.0","npm:pi-mcp-extension"]}')
        self.cli("apply", "--agent", "pi", ok=False)

    def test_omp_profile_and_omo_custom_home(self):
        self.env["OMP_PROFILE"] = "fixture"
        self.env["PI_CONFIG_DIR"] = ".omp-custom"
        self.env["OMO_CODING_AGENT_DIR"] = str(self.home / "omo-custom")
        self.mcp()
        self.cli("apply", "--agent", "omp")
        self.assertTrue((self.home / ".omp-custom/profiles/fixture/agent/mcp.json").exists())
        self.assertFalse((self.home / ".omp/agent/mcp.json").exists())
        self.cli("apply", "--agent", "omo")
        self.assertTrue((self.home / "omo-custom/mcp.json").exists())
        self.env["OMP_PROFILE"] = "../../escape"
        self.cli("apply", "--agent", "omp", ok=False)

    def test_unknown_adapter_and_unsupported_transport_report_failure(self):
        self.mcp(transport="sse", url="https://example.invalid/events")
        self.cli("apply", "--agent", "codex", ok=False)
        self.cli("apply", "--agent", "zed", ok=False)
        self.cli("apply", "--agent", "unknown", ok=False)
        self.cli("mcp", "apply", "--agent", "dsh", ok=False)
        self.assertFalse((self.directory / "library-managed.json").exists())
        targets = {v["agent"]: v for v in self.cli("targets")["targets"]}
        self.assertFalse(targets["dsh"]["mcp"])
        self.assertTrue(targets["codex"]["skills"])

    @unittest.skipUnless(sys.platform == "linux", "flock contention fixture")
    def test_concurrent_library_writer_is_rejected(self):
        import fcntl
        with (self.directory / ".library.lock").open("a+") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.cli("list", ok=False)
        self.cli("list")


if __name__ == "__main__":
    unittest.main()
