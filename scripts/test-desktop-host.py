#!/usr/bin/env python3
"""Real native HTTP -> privacy/routing -> desktop lease acceptance; loopback fixtures only."""
import argparse
import contextlib
import http.client
import http.server
import importlib.util
import json
import os
from pathlib import Path
import socket
import signal
import struct
import subprocess
import threading
import time
import unittest

_spec = importlib.util.spec_from_file_location("desktop_routing_fixture", Path(__file__).with_name("test-routing.py"))
base = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(base)

class Mock(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.records.append((self.path, request))
        refused = self.server.refuse.get(request.get("model"))
        if refused:
            data = b'{"error":{"message":"fixture refused","type":"overloaded"}}'
            self.send_response(refused)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        mode = self.server.mode
        text = request["messages"][0]["content"]
        # The real Responses -> Chat adapter can preserve text as Chat content
        # parts. A provider accepts both that shape and the plain string form.
        if isinstance(text, list):
            assert all(part.get("type") == "text" and isinstance(part.get("text"), str) for part in text)
            text = "".join(part["text"] for part in text)
        assert isinstance(text, str)
        tool = mode == "tool"
        if request.get("stream"):
            parts = [text[i:i + 3] for i in range(0, len(text), 3)]
            events = [{"id": "fixture-chat", "model": request["model"], "choices": [
                {"index": 0, "delta": {"role": "assistant", "content": part}}]} for part in parts]
            if tool:
                events.append({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "fixture-tool", "type": "function",
                     "function": {"name": "fixture", "arguments": "{}"}}]}}]})
            if mode != "incomplete":
                events.append({"choices": [{"index": 0, "delta": {},
                                            "finish_reason": "tool_calls" if tool else "stop"}]})
            data = "".join("data: " + json.dumps(v) + "\n\n" for v in events).encode()
            data += b"data: [DONE]\n\n"
            content_type = "text/event-stream"
        else:
            message = {"role": "assistant", "content": text}
            if tool:
                message["tool_calls"] = [{"id": "fixture-tool", "type": "function",
                                         "function": {"name": "fixture", "arguments": "{}"}}]
            data = json.dumps({"id": "fixture-chat", "object": "chat.completion",
                "model": request["model"], "choices": [
                    {"index": 0, "message": message, "finish_reason": "tool_calls" if tool else "stop"}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 8}}).encode()
            content_type = "application/json"
        if mode == "slow":
            time.sleep(1.15)
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data) + (40 if mode == "truncated" else 0)))
        self.end_headers()
        try:
            if mode == "cancel":
                first = data.find(b"\n\n") + 2
                self.wfile.write(data[:first])
                self.wfile.flush()
                self.server.started.set()
                self.server.release.wait(4)
                self.wfile.write(data[first:])
            else:
                self.wfile.write(data)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass
        if mode == "truncated":
            self.close_connection = True

base.Mock = Mock

class DesktopHostTests(base.Routing):
    args = None

    def setUp(self):
        super().setUp()
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith(("ZCALLD_", "CODEX_", "XDG_")) or key == "NOTIFY_SOCKET":
                self.env.pop(key)
        self.env.update(HOME=str(self.root), USERPROFILE=str(self.root),
                        APPDATA=str(self.root / "appdata"),
                        XDG_CONFIG_HOME=str(self.root / "config-home"),
                        ZCALLD_DESKTOP_PLUGIN=str(self.args.desktop_plugin),
                        ZCALLD_PLUGINCTL=str(self.args.pluginctl),
                        ZCALLD_REDACT_PLUGIN=str(self.args.redact_plugin),
                        ZCALLD_PRIVACY_WORKER=str(self.args.privacy_worker))
        self.mock.mode = "normal"
        self.mock.started = threading.Event()
        self.mock.release = threading.Event()
        self.state = {"groups": [{"name": "group/test", "mode": "order",
            "original": {"affinity": "none"},
            "members": [{"model": "first"}, {"model": "second"}]}]}
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.desktop_settings = self.root / "desktop-input.json"
        self.settings = {"enabled": True, "keep_awake": True,
            "turn_done_seconds": 1, "mask_alert": True, "backend": "record", "language": "en"}
        self.desktop_settings.write_text(json.dumps(self.settings), encoding="utf-8", newline="\n")
        self.cli("desktop", "configure", "--file", str(self.desktop_settings))
        self.state_dir = self.root / "desktop/state"
        self.inbox = self.root / "desktop/inbox"
        self.recorded = self.state_dir / "recorded.jsonl"
        privacy = self.root / "privacy.json"
        binding = json.loads(privacy.read_bytes())
        binding.update(history_dir=str(self.root / "history"), desktop_inbox=str(self.inbox))
        privacy.write_text(json.dumps(binding), encoding="utf-8", newline="\n")

    def tearDown(self):
        self.mock.release.set()
        self.stop_gateway()
        super().tearDown()

    def stop_gateway(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                self.fail("desktop gateway did not drain")

    def cli(self, *args, ok=True):
        result = subprocess.run([str(self.args.binary), *args, "--config", str(self.config), "--json"],
            env=self.env, capture_output=True, text=True, encoding="utf-8", timeout=20)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def start(self):
        self.process = subprocess.Popen([str(self.args.binary), "serve", "--config", str(self.config)],
            env=self.env, stdout=self.log, stderr=self.log,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
        end = time.monotonic() + 8
        while time.monotonic() < end:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode(errors="replace"))
            try:
                if self.call(None)[0] == 200:
                    return
            except OSError:
                pass
            time.sleep(.02)
        self.fail("gateway did not start")

    def rows(self):
        try:
            return [json.loads(line) for line in self.recorded.read_text(encoding="utf-8").splitlines()]
        except FileNotFoundError:
            return []
        except json.JSONDecodeError:
            # Reader can observe the last append in progress.
            return []

    def wait_for(self, predicate, timeout=8):
        end = time.monotonic() + timeout
        rows = []
        while time.monotonic() < end:
            rows = self.rows()
            if predicate(rows):
                return rows
            time.sleep(.025)
        self.fail("desktop record condition not reached: " + json.dumps(rows))

    def released(self, completed=0, suppressed=0):
        return self.wait_for(lambda rows: any(
            r.get("kind") == "state" and r.get("active_requests") == 0
            and r.get("completed", 0) >= completed and r.get("suppressed", 0) >= suppressed
            for r in rows))

    def turn(self):
        return self.wait_for(lambda rows: any(r.get("kind") == "turn" for r in rows))

    def generate(self, *, text="fixture text", stream=True, model="first", session="fixture-session", responses=False):
        body = {"model": model, "stream": stream}
        if responses:
            body["input"] = [{"role": "user", "content": [{"type": "input_text", "text": text}]}]
        else:
            body["messages"] = [{"role": "user", "content": text}]
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)) as connection:
            connection.request("POST", "/v1/responses" if responses else "/v1/chat/completions",
                json.dumps(body), {"Content-Type": "application/json", "User-Agent": "codex/fixture",
                                  "Session-Id": session})
            response = connection.getresponse()
            return response.status, response.read()

    def test_inspection_and_check_do_not_start_inhibitors_or_consume_inbox(self):
        event = self.inbox / ("1" * 32 + ".json")
        event.write_text(json.dumps({"kind": 2, "event": {"version": 1,
            "event_id": "fixture-event", "agent": "codex",
            "counts": [{"kind": "SECRET", "new_values": 1}]}}), encoding="utf-8", newline="\n")
        event.chmod(0o600)
        self.cli("desktop", "show")
        self.cli("check")
        self.assertTrue(event.exists())
        self.assertFalse(self.recorded.exists())
        self.assertEqual(self.mock.records, [])

    def test_plan_sync_rechecks_source_and_target_and_never_changes_agent_config(self):
        source = self.root / "old-magpie"
        source.mkdir(mode=0o700)
        settings = source / "settings.json"
        settings.write_text(json.dumps({"keepAwake": True, "keepAwakeDisplay": False,
            "usageAlert": 80, "balanceAlert": 3.5, "resetReminder": 24,
            "turnDoneAlert": 30, "maskAlert": True, "language": "zh-CN",
            "privateUnrelated": "never copied"}), encoding="utf-8", newline="\n")
        agent = self.root / ".codex/config.toml"
        agent.parent.mkdir()
        agent.write_bytes(b'# synthetic sentinel\nmodel = "untouched"\n')
        original = agent.read_bytes()
        target = self.root / "desktop.json"
        before = target.read_bytes()
        plan = self.cli("desktop", "plan", "--source", str(source))
        self.assertEqual(target.read_bytes(), before)
        self.assertNotIn("never copied", json.dumps(plan))
        updated = json.loads(settings.read_bytes()); updated["usageAlert"] = 85
        settings.write_text(json.dumps(updated), encoding="utf-8", newline="\n")
        self.cli("desktop", "sync", "--source", str(source), "--expect-plan", plan["plan_hash"], ok=False)
        self.assertEqual(target.read_bytes(), before)
        plan = self.cli("desktop", "plan", "--source", str(source))
        self.cli("desktop", "sync", "--source", str(source), "--expect-plan", plan["plan_hash"])
        saved = json.loads(target.read_bytes())
        self.assertEqual(saved["usage_alert"], 85)
        self.assertEqual(saved["reset_reminder_hours"], 24)
        self.assertEqual(saved["balance_alert"], 3.5)
        self.assertEqual(agent.read_bytes(), original)
        self.assertEqual(self.mock.records, [])
        self.assertFalse(self.recorded.exists())
        self.cli("desktop", "disable")
        self.assertFalse(json.loads(target.read_bytes())["enabled"])

    def test_successful_stream_restores_privacy_before_completion_and_notifies_once(self):
        self.mock.mode = "slow"
        self.start()
        secret = "sk-proj-Wn4rT7yQ2mLx9VbK3cZp"
        status, body = self.generate(text="Use " + secret)
        self.assertEqual(status, 200, body)
        self.assertIn(secret.encode(), body)
        upstream = json.dumps(self.mock.records)
        self.assertNotIn(secret, upstream)
        self.assertIn("{{", upstream)
        rows = self.turn()
        self.released(completed=1)
        self.assertEqual(sum(r["kind"] == "turn" for r in rows), 1)
        self.assertTrue(any(r.get("kind") == "inhibit" and r.get("level") == "system" for r in rows))
        self.assertNotIn(secret, json.dumps(rows))
        self.assertNotIn("fixture-session", json.dumps(rows))
        if self.args.expect_privacy_events:
            self.wait_for(lambda rs: any(r.get("kind") == "mask" for r in rs))

    def test_converted_responses_stream_completes_after_delivery(self):
        self.mock.mode = "slow"
        self.start()
        status, body = self.generate(responses=True)
        self.assertEqual(status, 200, body)
        self.assertIn(b"response.completed", body)
        self.turn()
        self.released(completed=1)

    def test_group_refusal_is_hidden_and_one_inbound_lease_completes(self):
        self.mock.refuse["upstream-first"] = 503
        self.mock.mode = "slow"
        self.start()
        status, body = self.generate(model="group/test", text="Use sk-proj-Wn4rT7yQ2mLx9VbK3cZp")
        self.assertEqual(status, 200, body)
        self.assertNotIn(b"fixture refused", body)
        self.assertEqual([r[1]["model"] for r in self.mock.records], ["upstream-first", "upstream-second"])
        rows = self.turn()
        self.released(completed=1)
        self.assertEqual(sum(r["kind"] == "turn" for r in rows), 1)
        self.assertTrue(all(r.get("suppressed", 0) == 0 for r in rows if r.get("kind") == "state"))

    def test_tools_and_incomplete_successes_release_activity_without_completion(self):
        self.start()
        for index, mode in enumerate(("tool", "incomplete")):
            self.mock.mode = mode
            status, _ = self.generate(session="fixture-" + mode)
            self.assertEqual(status, 200)
            self.released(suppressed=index + 1)
        self.assertFalse(any(r["kind"] == "turn" for r in self.rows()))

    def test_unrouted_and_truncated_responses_release_activity(self):
        self.start()
        self.assertEqual(self.generate(model="missing", session="fixture-missing")[0], 404)
        self.released(suppressed=1)
        self.mock.mode = "truncated"
        try:
            self.generate(session="fixture-truncated")
        except (http.client.IncompleteRead, http.client.RemoteDisconnected, ConnectionResetError):
            pass
        self.released(suppressed=2)
        self.assertFalse(any(r["kind"] == "turn" for r in self.rows()))

    def test_client_disconnect_never_completes_and_drain_releases_inhibitor(self):
        self.mock.mode = "cancel"
        self.start()
        body = json.dumps({"model": "first", "stream": True,
            "messages": [{"role": "user", "content": "fixture cancellation"}]}).encode()
        client = socket.create_connection(("127.0.0.1", self.port), timeout=5)
        try:
            client.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                b"Content-Type: application/json\r\nUser-Agent: codex/fixture\r\nSession-Id: fixture-cancel\r\n"
                + f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
            response = http.client.HTTPResponse(client)
            response.begin()
            self.assertEqual(response.status, 200)
            self.assertTrue(self.mock.started.wait(2))
            self.wait_for(lambda rs: any(r.get("kind") == "state" and r.get("active_requests", 0) >= 1 for r in rs))
            client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("hh" if os.name == "nt" else "ii", 1, 0))
            response.close()
        finally:
            client.close()
            self.mock.release.set()
        self.released(suppressed=1)
        self.assertFalse(any(r["kind"] == "turn" for r in self.rows()))
        self.stop_gateway()
        self.assertTrue(any(r.get("kind") == "inhibit" and r.get("level") == "off" for r in self.rows()))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "desktop-plugin", "pluginctl", "redact-plugin", "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--expect-privacy-events", action="store_true")
    parser.add_argument("tests", nargs="*")
    args = parser.parse_args()
    for name, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, name, value.resolve(strict=True))
    DesktopHostTests.args = args
    names = args.tests or sorted(n for n in DesktopHostTests.__dict__ if n.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite(DesktopHostTests(n) for n in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)

if __name__ == "__main__":
    main()
