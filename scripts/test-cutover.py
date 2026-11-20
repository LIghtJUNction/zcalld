#!/usr/bin/env python3
"""Isolated service/filesystem fixtures. Never touch real accounts or services."""
import argparse
import contextlib
import copy
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import select
import subprocess
import sys
import socket
import socketserver
import tempfile
import threading
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("cutover", Path(__file__).with_name("cutover.py"))
cutover = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cutover)


def audit_row(key, length=None):
    value = {"key": key, "t": "2026-01-01T00:00:00Z", "what": "chat", "n": 1,
             "values": [{"p": "fixture-placeholder", "k": "email", "b": "rule",
                         "h": "j", "l": 16, "n": 1}]}
    if length is not None:
        value["extension"] = ""
        size = len(cutover.encoded(value)) + 1
        assert size <= length
        value["extension"] = "x" * (length - size)
    return cutover.encoded(value) + b"\n"


class PowerLoss(BaseException):
    """Simulate process termination outside normal exception rollback."""


class FakeSystem:
    def __init__(self, root, source, candidate, expected):
        self.root, self.source, self.candidate, self.expected = root, source, candidate, expected
        self.calls = []
        self.old, self.new, self.restarted, self.rotate_after_stop = True, False, False, False
        self.mcp_backend = None

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

    def mcp(self, port, path=cutover.MCP_PATH, unix=None, protocol=cutover.MCP_PROTOCOL):
        self.calls.append(("MCP", port, path, unix, self.new))
        assert self.mcp_backend is not None, "MCP was probed without an active library fixture"
        return self.mcp_backend(port, path, unix, protocol)

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


class MCPFixture:
    """Real loopback/Unix HTTP framing with fake, metadata-only MCP responses."""

    def __init__(self, case, tools):
        self.case, self.tools = case, copy.deepcopy(tools)
        self.records = []
        self.failure = None
        self.paginate = False
        self.duplicate = False
        self.bad_id = False
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                method, transport = body.get("method"), self.server.transport
                fixture.records.append((transport, body, dict(self.headers)))
                protocol = cutover.MCP_PROTOCOL
                failing = fixture.case.ops.new and fixture.failure
                kind = fixture.failure[1] if failing and fixture.failure[0] == transport else None
                if kind == "protocol":
                    protocol = "2025-11-25"
                result = None
                error = None
                status = 200
                if method == "initialize":
                    result = {"protocolVersion": protocol, "capabilities": {"tools": {}},
                              "serverInfo": {"name": "fixture", "version": "1"}}
                elif self.headers.get("Mcp-Session-Id") != "fixture-" + transport or self.headers.get("MCP-Protocol-Version") != protocol:
                    error = {"code": -32600, "message": "fixture session/protocol header mismatch"}
                elif method == "notifications/initialized":
                    self.send_response(202)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                elif method == "ping":
                    result = {}
                elif method == "tools/list":
                    if kind == "rejection":
                        error = {"code": -32000, "message": "fixture metadata rejected"}
                    elif kind == "http":
                        status = 403
                        error = {"code": -32000, "message": "fixture forbidden"}
                    else:
                        tools = copy.deepcopy(fixture.tools)
                        if kind == "catalog":
                            if tools:
                                tools[0]["description"] = "changed metadata with the same tool name"
                            else:
                                tools = [{"name": "unexpected", "inputSchema": {"type": "object"}}]
                        if fixture.duplicate and tools:
                            tools.append(copy.deepcopy(tools[0]))
                        result = {"tools": tools}
                        if fixture.paginate and len(tools) > 1:
                            if body.get("params", {}).get("cursor") == "page-2":
                                result = {"tools": tools[1:]}
                            else:
                                result = {"tools": tools[:1], "nextCursor": "page-2"}
                else:
                    error = {"code": -32601, "message": "fixture forbids executable methods"}
                reply = {"jsonrpc": "2.0", "id": "wrong-id" if fixture.bad_id else body.get("id")}
                reply["error" if error else "result"] = error if error else result
                payload = cutover.encoded(reply)
                self.send_response(status)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(payload)))
                if method == "initialize":
                    self.send_header("Mcp-Session-Id", "fixture-" + transport)
                self.end_headers()
                self.wfile.write(payload)

        self.tcp = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.tcp.transport = "tcp"
        case.unix.close()
        case.unix_path.unlink()
        self.unix = socketserver.ThreadingUnixStreamServer(str(case.unix_path), Handler)
        self.unix.daemon_threads = True
        self.unix.transport = "unix"
        case.unix_path.chmod(0o600)
        for server in (self.tcp, self.unix):
            thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
            thread.start()
            def close(server=server, thread=thread):
                server.shutdown()
                server.server_close()
                thread.join(2)
            case.addCleanup(close)
        system = cutover.System(case.state)
        case.ops.mcp_backend = lambda _port, path, unix, protocol: system.mcp(
            self.tcp.server_port, path, unix, protocol)

    def assert_read_only(self):
        allowed = {"initialize", "notifications/initialized", "ping", "tools/list"}
        self.case.assertTrue(self.records)
        self.case.assertTrue(all(body["method"] in allowed for _, body, _ in self.records))
        self.case.assertTrue(all(not any(k.lower() in ("authorization", "cookie") for k in headers)
                                 for _, _, headers in self.records))


class Cutover(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-cutover-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
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
        self.old_history = self.source / "masking"
        self.new_history = self.target / "privacy/history"
        self.history_day = "2026-01-01.jsonl"
        self.privacy_snapshot = self.root / "privacy-snapshot"
        self.privacy_snapshot.mkdir(mode=0o700)
        cutover.atomic(self.source / "redact.key", b"x" * 32)
        cutover.atomic(self.target / "privacy/redact.key", b"x" * 32)
        cutover.atomic(self.old_history / self.history_day, audit_row(10))
        cutover.save_json(self.old_history / "checked-2026-01-01.json",
                          {"fixture": {"looked": 2, "masked": 1}})
        shutil.copytree(self.old_history, self.new_history)
        shutil.copytree(self.old_history, self.privacy_snapshot / "masking")
        shutil.copy2(self.source / "redact.key", self.privacy_snapshot / "redact.key")
        cutover.save_json(self.target / "privacy/settings.json", {"redact": True})
        cutover.save_json(self.target / "privacy/plugin.json", {
            "worker": str(self.root / "pluginctl"),
            "settings": str(self.target / "privacy/settings.json"),
            "key": str(self.target / "privacy/redact.key")})
        usage = b'{"t":"2026-01-01T00:00:00Z","in":2,"out":1,"extra":"kept"}\n'
        cutover.atomic(self.source / "usage.jsonl", usage)
        cutover.atomic(self.target / "usage.jsonl", usage)
        self.config = self.target / "config.conf"
        self.config.write_text('[server]\nlisten=127.0.0.1:3425\nunix_socket=auto\ntrust_loopback=true\n'
                               '[route fixture/model]\nprovider=fixture\nmodel=wire-model\n', encoding="utf-8", newline="\n")
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
        self.tx.data["privacy"] = self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.tx.save()
        patches = [mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.xdg)}),
                   mock.patch.object(cutover, "same_process", side_effect=lambda v: v["pid"] == 101 and self.ops.old and not self.ops.restarted),
                   mock.patch.object(cutover, "process_tree", side_effect=lambda main, exclusions=(): [main]),
                   mock.patch.object(cutover, "owns_listener", side_effect=lambda pid, port: (pid == 101 and self.ops.old) or (pid == 202 and self.ops.new)),
                   mock.patch.object(cutover, "listener_inodes", side_effect=lambda port: {"owned"} if self.ops.old or self.ops.new else set())]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def enable_mcp(self, tools=None):
        cutover.save_json(self.source / "library.json", {"mcp": [{
            "name": "library1", "transport": "http", "url": "http://127.0.0.1:3425/mcp/magpie/tools",
            "agents": ["fixture-agent"]}]})
        self.tx.data["topology"]["library.json"] = cutover.file_hash(self.source / "library.json")
        self.mcp = MCPFixture(self, [] if tools is None else tools)
        expected = self.ops.mcp(3425)
        cutover.save_json(self.state / "expected-mcp.json", expected)
        self.tx.data.update(mcp_required=True, mcp_hash=cutover.digest(expected),
                            mcp_tool_count=len(expected["tools"]))
        self.tx.save()
        return expected

    def assert_mcp_rollback(self):
        self.assertEqual(self.tx.data["phase"], "rolled_back")
        self.assertTrue(self.ops.old)
        self.assertFalse(self.ops.new)
        self.assertEqual(self.autostart.read_bytes(), self.original_autostart)
        self.assertLess(self.ops.calls.index(("stop", cutover.UNIT)),
                        self.ops.calls.index("credential sync-auth"))
        self.assertLess(self.ops.calls.index("credential sync-auth"),
                        self.ops.calls.index("restart old"))
        self.mcp.assert_read_only()

    def test_prepare_captures_private_mcp_metadata_without_starting_candidate(self):
        self.enable_mcp()
        snapshot = self.root / "snapshot"
        snapshot.mkdir(mode=0o700)
        for name in ("providers.json", "settings.json", "plugin-auth.json", "library.json", "redact.key"):
            shutil.copy2(self.source / name, snapshot / name)
        shutil.copytree(self.old_history, snapshot / "masking")
        cutover.save_json(self.target / "migration.json", {
            "source": str(self.source), "snapshot": str(snapshot), "cutover_complete": False})
        cutover.save_json(self.target / "extensions/host.json", {
            key: str(self.root / "pluginctl") for key in ("bun", "host", "pi")})
        fresh = self.root / "fresh-state"
        fresh.mkdir(mode=0o700)
        tx = cutover.Transaction(fresh, self.ops)
        args = argparse.Namespace(config=str(self.config), source=str(self.source),
                                  binary=str(self.root / "zcalld"), pluginctl=str(self.root / "pluginctl"),
                                  magpie_pid=101, old_unit=None, external_owners=None, timeout=90)
        old = {"pid": 101, "start_ticks": 10, "uid": os.getuid(), "exe": str(self.root / "magpie")}
        self.ops.calls.clear()
        self.mcp.records.clear()
        with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(self.root)}), \
             mock.patch.object(cutover, "process_identity", return_value=old):
            tx.prepare(args)
        self.assertEqual(tx.data["phase"], "prepared")
        self.assertTrue(tx.data["mcp_required"])
        self.assertEqual(cutover.read_json(fresh / "expected-mcp.json"),
                         {"protocolVersion": cutover.MCP_PROTOCOL, "tools": []})
        self.assertEqual((fresh / "expected-mcp.json").stat().st_mode & 0o777, 0o600)
        self.assertEqual(tx.data["mcp_hash"], cutover.digest(cutover.read_json(fresh / "expected-mcp.json")))
        self.assertTrue(self.ops.old)
        self.assertFalse(self.ops.new)
        self.assertNotIn(("start", cutover.UNIT), self.ops.calls)
        self.assertNotIn("credential import-builtin", self.ops.calls)
        self.assertTrue(all(transport == "tcp" for transport, _, _ in self.mcp.records))
        self.mcp.assert_read_only()

    def test_mcp_empty_catalog_is_verified_over_tcp_and_unix(self):
        self.enable_mcp()
        self.ops.calls.clear()
        self.tx.apply()
        self.assertEqual(self.tx.data["phase"], "active")
        probes = [row for row in self.ops.calls if isinstance(row, tuple) and row[0] == "MCP"]
        self.assertEqual([(row[3], row[4]) for row in probes], [(None, True), (str(self.unix_path), True)])
        self.tx.rollback()
        self.assert_mcp_rollback()

    def test_mcp_unix_catalog_metadata_mismatch_rolls_back(self):
        self.enable_mcp([{"name": "fixture-tool", "description": "original",
                          "inputSchema": {"type": "object", "properties": {}}}])
        self.mcp.failure = ("unix", "catalog")
        self.ops.rotate_after_stop = True
        with self.assertRaisesRegex(cutover.CutoverError, "tool metadata differs"):
            self.tx.apply()
        self.assert_mcp_rollback()
        self.assertEqual(cutover.read_json(self.source / "plugin-auth.json")["fixture"]["refresh"], "new-final")

    def test_mcp_rpc_rejection_rolls_back(self):
        self.enable_mcp()
        self.mcp.failure = ("tcp", "rejection")
        with self.assertRaisesRegex(cutover.CutoverError, "JSON-RPC request was rejected"):
            self.tx.apply()
        self.assert_mcp_rollback()

    def test_mcp_http_rejection_rolls_back(self):
        self.enable_mcp()
        self.mcp.failure = ("unix", "http")
        with self.assertRaisesRegex(cutover.CutoverError, "HTTP request was rejected"):
            self.tx.apply()
        self.assert_mcp_rollback()

    def test_mcp_protocol_change_rolls_back(self):
        self.enable_mcp()
        self.mcp.failure = ("tcp", "protocol")
        with self.assertRaisesRegex(cutover.CutoverError, "protocol differs"):
            self.tx.apply()
        self.assert_mcp_rollback()

    def test_mcp_private_baseline_drift_fails_before_stopping_old(self):
        self.enable_mcp()
        cutover.save_json(self.state / "expected-mcp.json", {"protocolVersion": "2025-11-25", "tools": []})
        with self.assertRaisesRegex(cutover.CutoverError, "private MCP baseline changed"):
            self.tx.apply()
        self.assertTrue(self.ops.old)
        self.assertFalse(self.ops.new)
        self.assertNotIn("TERM old main", self.ops.calls)

    def test_mcp_reader_checks_session_pagination_reply_id_and_duplicate_names(self):
        tools = [{"name": "z", "description": "last", "inputSchema": {"type": "object"}},
                 {"name": "a", "description": "first", "inputSchema": {"type": "object"}}]
        self.enable_mcp(tools)
        self.mcp.paginate = True
        for unix in (None, str(self.unix_path)):
            with self.subTest(unix=bool(unix)):
                result = self.ops.mcp(3425, unix=unix)
                self.assertEqual([x["name"] for x in result["tools"]], ["a", "z"])
        self.mcp.bad_id = True
        with self.assertRaisesRegex(cutover.CutoverError, "JSON-RPC request was rejected"):
            self.ops.mcp(3425)
        self.mcp.bad_id = False
        self.mcp.duplicate = True
        with self.assertRaisesRegex(cutover.CutoverError, "duplicate names"):
            self.ops.mcp(3425)
        self.mcp.assert_read_only()

    def test_mcp_selection_only_covers_assigned_magpie_loopback_endpoint(self):
        entry = {"name": "library1", "transport": "http", "agents": ["fixture-agent"],
                 "url": "http://127.0.0.1:3425/mcp/magpie/tools"}
        def selected(change):
            cutover.save_json(self.source / "library.json", {"mcp": [dict(entry, **change)]})
            return cutover.required_mcp(self.source)
        self.assertTrue(selected({}))
        for change in (
            {"agents": []}, {"disabled": True}, {"hidden": True}, {"offHere": True},
            {"transport": "stdio"}, {"url": "http://127.0.0.1:9000/mcp/magpie/tools"},
            {"url": "http://127.0.0.1:3425/mcp/other"}, {"url": "http://other.example:3425/mcp/magpie/tools"},
            {"url": "http://127.0.0.1:3425/mcp/magpie/tools?token=fixture"},
        ):
            with self.subTest(change=change):
                self.assertFalse(selected(change))

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

    def test_privacy_prepare_accepts_only_untouched_candidate_and_bound_paths(self):
        with (self.old_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(2, 947))
        state = self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.assertEqual(state["staged"], cutover.history_manifest(self.new_history))
        self.assertNotEqual(state["staged"], cutover.history_manifest(self.old_history))
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(3))
        with self.assertRaisesRegex(cutover.CutoverError, "candidate privacy history"):
            self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.assertTrue(self.ops.old)
        shutil.copy2(self.privacy_snapshot / "masking" / self.history_day, self.new_history / self.history_day)
        binding = cutover.read_json(self.target / "privacy/plugin.json")
        binding["history_dir"] = str(self.root / "unrelated-history")
        cutover.save_json(self.target / "privacy/plugin.json", binding)
        with self.assertRaisesRegex(cutover.CutoverError, "binding"):
            self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)

    def test_privacy_keys_and_candidate_edits_fail_before_stopping_old(self):
        for key in (self.source / "redact.key", self.target / "privacy/redact.key"):
            with self.subTest(key=key.name):
                cutover.atomic(key, b"y" * 32)
                with self.assertRaisesRegex(cutover.CutoverError, "privacy key"):
                    self.tx.apply()
                self.assertTrue(self.ops.old)
                self.assertNotIn("TERM old main", self.ops.calls)
                cutover.atomic(key, b"x" * 32)
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(4))
        before = (self.new_history / self.history_day).read_bytes()
        with self.assertRaisesRegex(cutover.CutoverError, "another writer"):
            self.tx.apply()
        self.assertEqual((self.new_history / self.history_day).read_bytes(), before)
        self.assertNotIn("TERM old main", self.ops.calls)

    def test_privacy_unsafe_nodes_are_rejected_without_following_them(self):
        outside = self.root / "outside.jsonl"
        cutover.atomic(outside, audit_row(20))
        node = self.new_history / "2026-01-02.jsonl"
        for kind in ("symlink", "directory", "fifo", "hardlink", "public"):
            with self.subTest(kind=kind):
                if kind == "symlink":
                    node.symlink_to(outside)
                elif kind == "directory":
                    node.mkdir(mode=0o700)
                elif kind == "fifo":
                    os.mkfifo(node, 0o600)
                elif kind == "hardlink":
                    os.link(outside, node)
                else:
                    cutover.atomic(node, audit_row(20), 0o644)
                with self.assertRaises(cutover.CutoverError):
                    cutover.history_manifest(self.new_history)
                if node.is_dir() and not node.is_symlink():
                    node.rmdir()
                else:
                    node.unlink()
        self.assertEqual(outside.read_bytes(), audit_row(20))

    def test_privacy_active_writer_lock_is_refused_and_never_migrated(self):
        lock = cutover.history_lock_path(self.new_history)
        cutover.atomic(lock, (99).to_bytes(8, "little"))
        with lock.open("r+b") as held:
            cutover.fcntl.flock(held, cutover.fcntl.LOCK_EX | cutover.fcntl.LOCK_NB)
            with self.assertRaisesRegex(cutover.CutoverError, "active writer"):
                self.tx.apply()
            self.assertTrue(self.ops.old)
            self.assertNotIn("TERM old main", self.ops.calls)
        self.assertNotIn(lock.name, cutover.history_manifest(self.new_history))
        with (self.old_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(4))
        self.tx.apply()
        self.assertEqual(lock.read_bytes(), (99).to_bytes(8, "little"))
        cutover.atomic(lock, (100).to_bytes(8, "little"))
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(5))
        self.tx.rollback()
        self.assertFalse((self.old_history / lock.name).exists())
        self.assertEqual(cutover.history_lock_path(self.old_history).read_bytes(), b"")
        self.assertTrue(lock.exists())

    def test_privacy_final_tail_and_checked_counters_are_copied_after_drain(self):
        initial = (self.old_history / self.history_day).read_bytes()
        tail = audit_row(2, 947)  # exact size of the observed live migration drift
        with (self.old_history / self.history_day).open("ab") as stream:
            stream.write(tail)
        real_drain = self.ops.drain
        final = audit_row(1)  # completion order need not be monotonic by record key
        def drain(identities, timeout):
            real_drain(identities, timeout)
            if any(item["pid"] == 101 for item in identities):
                with (self.old_history / self.history_day).open("ab") as stream:
                    stream.write(final)
                cutover.save_json(self.old_history / "checked-2026-01-01.json",
                                  {"fixture": {"looked": 4, "masked": 3, "whole": 1}})
                cutover.atomic(self.old_history / "2026-01-02.jsonl", audit_row(0))
        with mock.patch.object(self.ops, "drain", side_effect=drain):
            self.tx.apply()
        self.assertEqual((self.new_history / self.history_day).read_bytes(), initial + tail + final)
        self.assertEqual(cutover.history_manifest(self.new_history), cutover.history_manifest(self.old_history))
        self.assertEqual(self.tx.data["privacy"]["base"], cutover.history_manifest(self.old_history))
        self.assertTrue(self.tx.data["privacy"]["forward"]["committed"])
        stage = Path(self.tx.data["privacy"]["forward"]["stage"])
        self.assertTrue(stage.is_relative_to(self.state))
        self.assertEqual((stage / self.history_day).read_bytes(), initial)
        self.assertEqual(stage.stat().st_mode & 0o777, 0o700)

    def test_privacy_atomic_exchange_retains_a_racing_candidate_writer(self):
        with (self.old_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(2))
        real_move = cutover.history_move
        touched = False
        expected = (self.new_history / self.history_day).read_bytes() + audit_row(77)
        def race(source, destination, exchange):
            nonlocal touched
            if Path(destination) == self.new_history and not touched:
                touched = True
                with (self.new_history / self.history_day).open("ab") as stream:
                    stream.write(audit_row(77))
            return real_move(source, destination, exchange)
        with mock.patch.object(cutover, "history_move", side_effect=race):
            with self.assertRaisesRegex(cutover.CutoverError, "atomic exchange"):
                self.tx.apply()
        self.assertEqual((self.new_history / self.history_day).read_bytes(), expected)
        self.assertTrue(self.ops.old)
        self.assertFalse(self.ops.new)

    def test_privacy_fallback_key_is_immutable_and_distinct_from_active_key(self):
        for directory in (self.old_history, self.new_history, self.privacy_snapshot / "masking"):
            cutover.atomic(directory / "redact.key", b"historical-fallback-key".ljust(32, b"."))
        self.tx.data["privacy"] = self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.tx.save()
        self.tx.apply()
        self.assertEqual((self.target / "privacy/redact.key").read_bytes(), b"x" * 32)
        fallback = (self.old_history / "redact.key").read_bytes()
        cutover.atomic(self.new_history / "redact.key", b"z" * 32)
        with self.assertRaisesRegex(cutover.CutoverError, "fallback privacy key"):
            self.tx.rollback()
        self.assertEqual((self.old_history / "redact.key").read_bytes(), fallback)
        self.assertFalse(self.ops.old)

    def test_privacy_source_key_fallback_survives_history_exchange(self):
        (self.source / "redact.key").rename(self.old_history / "redact.key")
        (self.privacy_snapshot / "redact.key").rename(self.privacy_snapshot / "masking/redact.key")
        cutover.atomic(self.new_history / "redact.key", b"x" * 32)
        self.tx.data["privacy"] = self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.tx.save()
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(3))
        self.tx.rollback()
        self.assertFalse((self.source / "redact.key").exists())
        self.assertEqual((self.old_history / "redact.key").read_bytes(), b"x" * 32)
        self.assertEqual((self.old_history / self.history_day).read_bytes(),
                         (self.new_history / self.history_day).read_bytes())

    def test_privacy_day_clear_prune_and_checked_replacement_keep_recovery_copies(self):
        (self.old_history / "checked-2026-01-01.json").unlink()
        cutover.atomic(self.old_history / self.history_day, audit_row(1))
        cutover.atomic(self.old_history / "2026-01-02.jsonl", audit_row(2))
        self.tx.apply()
        self.assertFalse((self.new_history / "checked-2026-01-01.json").exists())
        (self.new_history / self.history_day).unlink()
        cutover.save_json(self.new_history / "checked-2026-01-02.json",
                          {"fixture": {"looked": 8, "masked": 5}})
        self.tx.rollback()
        self.assertEqual(cutover.history_manifest(self.old_history), cutover.history_manifest(self.new_history))
        self.assertFalse((self.old_history / self.history_day).exists())
        backup = Path(self.tx.data["privacy"]["rollback"]["stage"])
        self.assertEqual((backup / self.history_day).read_bytes(), audit_row(1))

    def test_privacy_rollback_waits_for_final_audit_write_and_is_repeatable(self):
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(2))
        real_drain = self.ops.drain
        def drain(identities, timeout):
            real_drain(identities, timeout)
            if any(item["pid"] == 202 for item in identities):
                with (self.new_history / self.history_day).open("ab") as stream:
                    stream.write(audit_row(3))
                cutover.save_json(self.new_history / "checked-2026-01-01.json",
                                  {"fixture": {"looked": 4, "masked": 3}})
        with mock.patch.object(self.ops, "drain", side_effect=drain):
            self.tx.rollback()
        expected = audit_row(10) + audit_row(2) + audit_row(3)
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
        self.assertEqual(cutover.history_manifest(self.old_history), cutover.history_manifest(self.new_history))
        self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)

    def test_privacy_rollback_refuses_a_different_original_writer(self):
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(2))
        with (self.old_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(99))
        expected = (self.old_history / self.history_day).read_bytes()
        with self.assertRaisesRegex(cutover.CutoverError, "original privacy history.*another writer"):
            self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
        self.assertEqual(self.tx.data["phase"], "recovery_required")
        self.assertNotIn("restart old", self.ops.calls)
        self.assertNotIn("credential sync-auth", self.ops.calls)

    def test_privacy_rollback_rejects_unfinished_records_without_printing_them(self):
        self.tx.apply()
        marker = b'{"private_fixture_value":"must-not-appear"'
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(marker)
        before = (self.old_history / self.history_day).read_bytes()
        with self.assertRaises(cutover.CutoverError) as raised:
            self.tx.rollback()
        self.assertNotIn("must-not-appear", str(raised.exception))
        self.assertEqual((self.old_history / self.history_day).read_bytes(), before)
        self.assertEqual(self.tx.data["phase"], "recovery_required")

    def test_privacy_failed_start_preserves_new_audit_rows_in_automatic_rollback(self):
        def fail():
            with (self.new_history / self.history_day).open("ab") as stream:
                stream.write(audit_row(4))
            raise cutover.CutoverError("fixture start verification failed")
        with mock.patch.object(self.tx, "verify_new", side_effect=fail):
            with self.assertRaisesRegex(cutover.CutoverError, "fixture start"):
                self.tx.apply()
        expected = audit_row(10) + audit_row(4)
        self.assertEqual(self.tx.data["phase"], "rolled_back")
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
        self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)

    def test_privacy_crash_after_atomic_exchange_resumes_without_duplicate_rows(self):
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(5))
        real_move = cutover.history_move
        def crash(source, destination, exchange):
            real_move(source, destination, exchange)
            if Path(destination) == self.old_history:
                raise PowerLoss()
        with mock.patch.object(cutover, "history_move", side_effect=crash):
            with self.assertRaises(PowerLoss):
                self.tx.rollback()
        expected = audit_row(10) + audit_row(5)
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
        resumed = cutover.Transaction(self.state, self.ops)
        self.assertFalse(resumed.data["privacy"]["rollback"]["committed"])
        resumed.rollback()
        self.assertEqual(resumed.data["phase"], "rolled_back")
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
        resumed.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)

    def test_privacy_external_lock_serializes_old_waiter_and_post_exchange_writer(self):
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(5))
        expected = (self.new_history / self.history_day).read_bytes()
        lock = cutover.history_lock_path(self.old_history)
        identity = (lock.stat().st_dev, lock.stat().st_ino)
        writers = []
        child = r"""
import fcntl, json, os, sys
lock, path, row = sys.argv[1:]
fd = os.open(lock, os.O_RDWR | os.O_NOFOLLOW)
identity = os.fstat(fd)
print(json.dumps([identity.st_dev, identity.st_ino]), flush=True)
fcntl.flock(fd, fcntl.LOCK_EX)
current = os.stat(lock, follow_symlinks=False)
assert (current.st_dev, current.st_ino) == (identity.st_dev, identity.st_ino)
with open(path, "ab", buffering=0) as output:
    output.write(row.encode() + b"\n")
    os.fsync(output.fileno())
fcntl.flock(fd, fcntl.LOCK_UN)
os.close(fd)
print("committed", flush=True)
"""
        def spawn(key):
            process = subprocess.Popen(
                [sys.executable, "-c", child, str(lock), str(self.old_history / self.history_day),
                 audit_row(key).decode().rstrip("\n")],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
            writers.append(process)
            def cleanup():
                if process.poll() is None:
                    process.terminate()
                process.communicate(timeout=3)
            self.addCleanup(cleanup)
            self.assertTrue(select.select([process.stdout], [], [], 3)[0], "fixture writer did not open its lock")
            self.assertEqual(tuple(json.loads(process.stdout.readline())), identity)
            return process
        real_move = cutover.history_move
        def exchange(source, destination, exchange):
            if Path(destination) != self.old_history:
                return real_move(source, destination, exchange)
            first = spawn(200)  # descriptor opened before the directory moves
            real_move(source, destination, exchange)
            second = spawn(201)  # descriptor opened through the new live path
            self.assertEqual((lock.stat().st_dev, lock.stat().st_ino), identity)
            self.assertEqual(select.select([first.stdout, second.stdout], [], [], 0.03)[0], [],
                             "a writer bypassed the transaction's stable lock")
            self.assertEqual((self.old_history / self.history_day).read_bytes(), expected)
            self.assertFalse(self.tx.data["privacy"]["rollback"]["committed"])
        with mock.patch.object(cutover, "history_move", side_effect=exchange):
            self.tx.rollback()
        for process in writers:
            stdout, stderr = process.communicate(timeout=3)
            self.assertEqual(process.returncode, 0, stderr.decode())
            self.assertEqual(stdout.strip(), b"committed")
        restored = (self.old_history / self.history_day).read_bytes()
        self.assertTrue(restored.startswith(expected))
        tail = [json.loads(line) for line in restored[len(expected):].splitlines()]
        self.assertEqual({row["key"] for row in tail}, {200, 201})
        self.assertEqual(len(tail), 2)
        self.assertTrue(self.tx.data["privacy"]["rollback"]["committed"])
        self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), restored)

    def test_privacy_rollback_publishes_new_history_without_replacing_another_directory(self):
        for directory in (self.old_history, self.new_history, self.privacy_snapshot / "masking"):
            shutil.rmtree(directory)
        self.tx.data["privacy"] = self.tx.prepare_privacy(self.source, self.target, self.privacy_snapshot)
        self.tx.save()
        self.tx.apply()
        cutover.atomic(self.new_history / self.history_day, audit_row(8))
        self.assertFalse(self.old_history.exists())
        self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), audit_row(8))
        self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), audit_row(8))

    def test_privacy_crash_during_private_copy_can_retry_without_touching_original(self):
        self.tx.apply()
        with (self.new_history / self.history_day).open("ab") as stream:
            stream.write(audit_row(5))
        before = (self.old_history / self.history_day).read_bytes()
        def crash(source, destination, expected):
            cutover.atomic(Path(destination) / self.history_day, b'{"unfinished')
            raise PowerLoss()
        with mock.patch.object(cutover, "history_copy", side_effect=crash):
            with self.assertRaises(PowerLoss):
                self.tx.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), before)
        resumed = cutover.Transaction(self.state, self.ops)
        self.assertFalse(resumed.data["privacy"]["rollback"]["ready"])
        resumed.rollback()
        self.assertEqual((self.old_history / self.history_day).read_bytes(), audit_row(10) + audit_row(5))
        self.assertEqual(resumed.data["phase"], "rolled_back")

    def test_metadata_comparison_covers_more_than_ids(self):
        changed = json.loads(json.dumps(self.expected))
        changed["data"][0]["context"] = 1
        self.assertNotEqual(cutover.digest(cutover.catalog(changed)), cutover.digest(cutover.catalog(self.expected)))
        with self.assertRaises(cutover.CutoverError):
            cutover.catalog({"data": [self.expected["data"][0]] * 2})


if __name__ == "__main__":
    unittest.main()
