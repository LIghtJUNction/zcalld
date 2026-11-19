#!/usr/bin/env python3
"""Isolated service/filesystem fixtures. Never touch real accounts or services."""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("cutover", Path(__file__).with_name("cutover.py"))
cutover = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cutover)


class FakeSystem:
    def __init__(self, root, source, candidate, expected):
        self.root, self.source, self.candidate, self.expected = root, source, candidate, expected
        self.calls = []
        self.old, self.new, self.restarted, self.rotate_after_stop = True, False, False, False

    def run(self, args, label, **_):
        self.calls.append(label)
        if "import-builtin" in args:
            auth = self.candidate / "extensions/plugin-auth.json"
            value = cutover.read_json(auth)
            value["builtin:codex#fixture"] = {"refreshOwner": "external", "access": "fixture-access",
                                             "sourceAuthFile": str(self.root / "native-auth.json")}
            cutover.save_json(auth, value)
        return b'{}'

    def control(self, verb, unit=None, timeout=90):
        self.calls.append((verb, unit))
        if unit == cutover.UNIT and verb == "start":
            self.new = True
        if unit == cutover.UNIT and verb == "stop":
            self.new = False

    def unit(self, name):
        return {"ActiveState": "active" if self.new else "inactive", "MainPID": "202" if self.new else "0",
                "UnitFileState": "disabled", "LoadState": "loaded"}

    def service_processes(self, _):
        return [{"pid": 202, "start_ticks": 20}] if self.new else []

    def get(self, port, path, unix=None):
        self.calls.append(("GET", port, path, unix))
        return {"status": "ok"} if path == "/healthz" else self.expected

    def terminate(self, _):
        self.calls.append("TERM old main")
        self.old = False
        cutover.save_json(self.source / "plugin-auth.json", {"fixture": {"type": "oauth", "refresh": "old-final"}})
        with (self.source / "usage.jsonl").open("ab") as stream:
            stream.write(b'{"t":"2026-01-02T00:00:00Z","in":3,"out":1}\n')

    def drain(self, identities, _):
        self.calls.append("drain")
        if self.rotate_after_stop and any(item["pid"] == 202 for item in identities):
            auth = self.candidate / "extensions/plugin-auth.json"
            value = cutover.read_json(auth)
            value["fixture"]["refresh"] = "new-final"
            cutover.save_json(auth, value)

    def restart_old(self, _):
        self.calls.append("restart old")
        self.old, self.restarted = True, True

    def restored_old(self, _):
        return self.restarted and self.old


class Cutover(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-cutover-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source, self.target, self.state = [self.root / name for name in ("old", "new", "state")]
        for p in (self.source, self.target / "extensions", self.state):
            p.mkdir(parents=True, mode=0o700)
        self.expected = {"object": "list", "data": [{"id": "fixture/model", "context": 100000,
                         "reasoning": True, "tool_call": True, "extra": {"preserved": [1, 2]}}]}
        cutover.save_json(self.source / "settings.json", {"lan": False, "redact": True})
        cutover.save_json(self.source / "providers.json", {"fixture": True})
        cutover.save_json(self.source / "plugin-auth.json", {"fixture": {"type": "oauth", "refresh": "old-staged"}})
        cutover.save_json(self.target / "extensions/plugin-auth.json", cutover.read_json(self.source / "plugin-auth.json"))
        cutover.save_json(self.target / "extensions/builtin-auth-map.json", {"accounts": [
            {"key": "builtin:codex#fixture", "sources": [{"kind": "codex", "path": str(self.root / "native-auth.json")}]}]})
        cutover.save_json(self.root / "native-auth.json", {"refresh": "native-stays-owned"})
        cutover.save_json(self.target / "model-catalog.json", self.expected)
        cutover.save_json(self.state / "expected-models.json", self.expected)
        cutover.save_json(self.target / "migration.json", {"cutover_complete": False})
        usage = b'{"t":"2026-01-01T00:00:00Z","in":2,"out":1,"extra":"kept"}\n'
        cutover.atomic(self.source / "usage.jsonl", usage)
        cutover.atomic(self.target / "usage.jsonl", usage)
        self.config = self.target / "config.conf"
        self.config.write_text('[server]\nlisten=127.0.0.1:3425\nunix_socket=auto\ntrust_loopback=true\n'
                               '[route fixture/model]\nprovider=fixture\nmodel=wire-model\n')
        for name in ("zcalld", "pluginctl", "magpie"):
            cutover.atomic(self.root / name, b"fixture executable\n", 0o755)
        self.xdg = self.root / "config-home"
        self.autostart = self.xdg / "autostart/magpie.desktop"
        cutover.atomic(self.autostart, b"[Desktop Entry]\nExec=/fixture/magpie\nType=Application\n", 0o644)
        self.original_autostart = self.autostart.read_bytes()
        self.unix = socket.socket(socket.AF_UNIX)
        self.addCleanup(self.unix.close)
        self.unix_path = self.root / "gateway.sock"
        self.unix.bind(str(self.unix_path))
        self.unix_path.chmod(0o600)
        self.ops = FakeSystem(self.root, self.source, self.target, self.expected)
        self.tx = cutover.Transaction(self.state, self.ops)
        self.tx.data = {"version": 1, "id": "fixture", "phase": "prepared", "config": str(self.config),
                        "source": str(self.source), "binary": str(self.root / "zcalld"),
                        "pluginctl": str(self.root / "pluginctl"), "timeout": 90,
                        "unix_socket": str(self.unix_path), "old_process": {"pid": 101, "start_ticks": 10},
                        "exclusions": [], "old_unit": None, "old_unit_state": None,
                        "new_unit_state": {"UnitFileState": "disabled"}, "new_start_attempted": False,
                        "new_binary_hash": cutover.file_hash(self.root / "zcalld"),
                        "helper_hash": cutover.file_hash(self.root / "pluginctl"),
                        "config_hash": cutover.file_hash(self.config),
                        "catalog_hash": cutover.digest(cutover.catalog(self.expected)),
                        "topology": {"providers.json": cutover.file_hash(self.source / "providers.json")},
                        "settings_hash": cutover.settings_fingerprint(self.source), "changes": {},
                        "community_keys": ["fixture"], "community_staged": {
                            "fixture": cutover.digest({"type": "oauth", "refresh": "old-staged"})}}
        self.tx.save()
        patches = [mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.xdg)}),
                   mock.patch.object(cutover, "same_process", side_effect=lambda v: v["pid"] == 101 and self.ops.old and not self.ops.restarted),
                   mock.patch.object(cutover, "process_tree", side_effect=lambda main, exclusions=(): [main]),
                   mock.patch.object(cutover, "owns_listener", side_effect=lambda pid, port: (pid == 101 and self.ops.old) or (pid == 202 and self.ops.new)),
                   mock.patch.object(cutover, "listener_inodes", side_effect=lambda port: {"owned"} if self.ops.old or self.ops.new else set())]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def test_switch_and_rollback_preserve_rotations_usage_and_launch(self):
        native_before = (self.root / "native-auth.json").read_bytes()
        self.tx.apply()
        self.assertEqual(self.tx.data["phase"], "active")
        self.assertTrue(self.ops.new)
        self.assertEqual((self.target / "usage.jsonl").read_bytes(), (self.source / "usage.jsonl").read_bytes())
        self.assertEqual(cutover.read_json(self.target / "extensions/plugin-auth.json")["fixture"]["refresh"], "old-final")
        self.assertIn(b"Hidden=true", self.autostart.read_bytes())
        old_history = (self.source / "usage.jsonl").read_bytes()
        row = {"time": 1767312100, "model": "group/fixture", "upstream": "fixture/model", "input_tokens": 12,
               "output_tokens": 7, "elapsed_ms": 20, "status": 200}
        with (self.target / "usage.jsonl").open("ab") as stream:
            stream.write(cutover.encoded(row) + b"\n")
        self.ops.rotate_after_stop = True
        self.tx.rollback()
        self.assertTrue(self.ops.old)
        self.assertFalse(self.ops.new)
        self.assertEqual(self.autostart.read_bytes(), self.original_autostart)
        self.assertEqual(cutover.read_json(self.source / "plugin-auth.json")["fixture"]["refresh"], "new-final")
        data = (self.source / "usage.jsonl").read_bytes()
        self.assertTrue(data.startswith(old_history))
        restored = json.loads(data.splitlines()[-1])
        self.assertEqual((restored["in"], restored["out"], restored["model"]), (12, 7, "wire-model"))
        self.assertEqual(restored["zcalld_record"], row)
        self.assertEqual((self.root / "native-auth.json").read_bytes(), native_before)
        self.tx.rollback()
        self.assertEqual((self.source / "usage.jsonl").read_bytes(), data)
        stop, sync = self.ops.calls.index(("stop", cutover.UNIT)), self.ops.calls.index("credential sync-auth")
        self.assertIn("drain", self.ops.calls[stop:sync])
        self.assertLess(sync, self.ops.calls.index("restart old"))

    def test_failed_new_service_rolls_back_after_draining_refresh(self):
        self.ops.rotate_after_stop = True
        with mock.patch.object(self.tx, "verify_new", side_effect=cutover.CutoverError("fixture verification failed")):
            with self.assertRaises(cutover.CutoverError):
                self.tx.apply()
        self.assertEqual(self.tx.data["phase"], "rolled_back")
        self.assertTrue(self.ops.old)
        self.assertEqual(cutover.read_json(self.source / "plugin-auth.json")["fixture"]["refresh"], "new-final")
        self.assertEqual(self.autostart.read_bytes(), self.original_autostart)

    def test_conflicting_original_token_is_never_overwritten_or_restarted(self):
        self.tx.apply()
        cutover.save_json(self.source / "plugin-auth.json", {"fixture": {"type": "oauth", "refresh": "another-owner"}})
        with self.assertRaisesRegex(cutover.CutoverError, "another owner"):
            self.tx.rollback()
        self.assertEqual(self.tx.data["phase"], "recovery_required")
        self.assertNotIn("restart old", self.ops.calls)
        self.assertNotIn("credential sync-auth", self.ops.calls)
        self.assertEqual(cutover.read_json(self.source / "plugin-auth.json")["fixture"]["refresh"], "another-owner")

    def test_resume_after_old_restart_does_not_duplicate_usage(self):
        self.tx.apply()
        self.tx.rollback()
        self.tx.data["phase"] = "recovery_required"
        before = (self.source / "usage.jsonl").read_bytes()
        count = self.ops.calls.count("credential sync-auth")
        self.tx.rollback()
        self.assertEqual(self.ops.calls.count("credential sync-auth"), count)
        self.assertEqual((self.source / "usage.jsonl").read_bytes(), before)
        self.assertEqual(self.ops.calls.count("restart old"), 1)

    def test_manual_launch_edit_is_not_clobbered(self):
        self.tx.change(self.autostart, b"disabled\n")
        self.autostart.write_bytes(b"user changed this\n")
        with self.assertRaisesRegex(cutover.CutoverError, "outside this transaction"):
            self.tx.restore_changes()
        self.assertEqual(self.autostart.read_bytes(), b"user changed this\n")

    def test_private_recovery_copy_survives_outside_checkout(self):
        self.tx.save_recovery()
        recovery = self.state / "recovery.py"
        self.assertEqual(recovery.read_bytes(), Path(cutover.__file__).read_bytes())
        self.assertEqual(recovery.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.tx.data["recovery_sha256"], cutover.file_hash(recovery))

    def test_sync_reads_latest_builtin_fields_under_target_lock(self):
        auth = self.target / "extensions/plugin-auth.json"
        real_lock = cutover.auth_lock

        @contextlib.contextmanager
        def racing_lock(path):
            with real_lock(path):
                if Path(path) == auth:
                    value = cutover.read_json(auth)
                    value["builtin:other"] = {"refresh": "new-concurrent-value"}
                    cutover.save_json(auth, value)
                yield

        with mock.patch.object(cutover, "auth_lock", racing_lock):
            self.tx.sync_in()
        self.assertEqual(cutover.read_json(auth)["builtin:other"]["refresh"], "new-concurrent-value")

    def test_sync_revalidates_community_after_acquiring_lock(self):
        auth = self.target / "extensions/plugin-auth.json"
        real_lock = cutover.auth_lock

        @contextlib.contextmanager
        def racing_lock(path):
            with real_lock(path):
                if Path(path) == auth:
                    value = cutover.read_json(auth)
                    value["fixture"]["refresh"] = "candidate-concurrent-value"
                    cutover.save_json(auth, value)
                yield

        with mock.patch.object(cutover, "auth_lock", racing_lock):
            with self.assertRaisesRegex(cutover.CutoverError, "another owner"):
                self.tx.sync_in()
        self.assertEqual(cutover.read_json(auth)["fixture"]["refresh"], "candidate-concurrent-value")
        self.assertNotIn("credential import-builtin", self.ops.calls)

    def test_metadata_comparison_covers_more_than_ids(self):
        changed = json.loads(json.dumps(self.expected))
        changed["data"][0]["context"] = 1
        self.assertNotEqual(cutover.digest(cutover.catalog(changed)), cutover.digest(cutover.catalog(self.expected)))
        with self.assertRaises(cutover.CutoverError):
            cutover.catalog({"data": [self.expected["data"][0]] * 2})


if __name__ == "__main__":
    unittest.main()
