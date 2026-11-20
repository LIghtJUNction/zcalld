#!/usr/bin/env python3
"""Portable native gateway <-> Go advanced-rule fixtures; all upstreams are local mocks."""
import argparse
import contextlib
import datetime
import http.client
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest

spec = importlib.util.spec_from_file_location("basic_routing", Path(__file__).with_name("test-routing.py"))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)


class Advanced(unittest.TestCase):
    args = None

    def setUp(self):
        base.Routing.setUp(self)
        old = self.groups
        self.groups = self.root / "routing.json"
        old.rename(self.groups)
        text = self.config.read_text(encoding="utf-8")
        text = text[:text.index("[plugin routing]")]
        text = text.replace("[server]\n", "[server]\ntrust_loopback=true\n")
        self.config.write_text(text, encoding="utf-8", newline="\n")
        self.state["rules_worker"] = str(self.args.pluginctl)
        self.state["groups"][0]["original"] = {"affinity": "none", "rules": []}
        self.workspace = self.root / "trusted-project"
        self.workspace.mkdir()
        keys = self.root / "caller-keys.json"
        keys.write_text(json.dumps([{"id": "project", "secret": "fixture-bound", "lan": False,
                                     "workspace": str(self.workspace)}]), encoding="utf-8", newline="\n")
        if os.name == "posix":
            keys.chmod(0o600)
        self.config.write_text(self.config.read_text(encoding="utf-8").replace(
            "[server]\n", f"[server]\ncaller_keys_file={keys}\n"), encoding="utf-8", newline="\n")
        self.save()

    tearDown = base.Routing.tearDown

    def command_environment(self):
        environment = self.environment.copy()
        environment.pop("NOTIFY_SOCKET", None)
        environment.update({"ZCALLD_PLUGINCTL": str(self.args.pluginctl),
                            "ZCALLD_PLUGIN_ROUTING_PLUGIN": str(self.args.routing_plugin)})
        return environment

    def save(self):
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")

    def start(self):
        self.process = subprocess.Popen([str(self.args.binary), "serve", "--config", str(self.config)],
                                        stdout=self.log, stderr=self.log, env=self.command_environment(),
                                        cwd=self.root)
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode(errors="replace"))
            try:
                if self.call(None)[0] == 200:
                    return
            except OSError:
                pass
            time.sleep(.03)
        self.fail("gateway did not start")

    def call(self, body, session="advanced-fixture", headers=None):
        supplied = {"Content-Type": "application/json", "X-Session-Id": session}
        supplied.update(headers or {})
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=15)) as connection:
            connection.request("GET" if body is None else "POST",
                               "/healthz" if body is None else "/v1/chat/completions",
                               None if body is None else json.dumps(body), supplied)
            response = connection.getresponse()
            return response.status, response.read()

    def cli(self, *arguments, ok=True):
        result = subprocess.run([str(self.args.binary), *arguments, "--config", str(self.config), "--json"],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=self.command_environment(), timeout=30)
        if ok:
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
            return json.loads(result.stdout)
        self.assertNotEqual(result.returncode, 0, result.stdout.decode(errors="replace"))
        return result

    def model(self):
        return [body["model"] for path, body in self.mock.records
                if path.endswith("/chat/completions")][-1]

    def body(self, text="fixture", **extra):
        return {"model": "group/test", "messages": [{"role": "user", "content": text}], **extra}

    def rule(self, *rules, affinity="none"):
        self.state["groups"][0]["original"] = {"affinity": affinity, "rules": list(rules)}
        self.save()

    def test_time_where_and_subagent_use_real_rpc(self):
        utc = datetime.datetime.now(datetime.timezone.utc)
        days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        self.rule({"use": "second",
                   "time": {"from": "00:00", "to": "00:00", "zone": "UTC",
                            "days": [days[utc.weekday()], days[(utc.weekday() + 1) % 7]]},
                   "where": [{"field": "tokens", "op": "gt", "value": "0"},
                             {"field": "effort", "op": "is", "value": "off"},
                             {"field": "agent", "op": "is", "value": "claude"},
                             {"field": "subagent", "op": "is"},
                             {"field": "system", "op": "starts", "value": "routing fixture"},
                             {"field": "last", "op": "matches", "value": r"^do\s+work$"}]})
        self.start()
        body = {"model": "group/test", "messages": [
            {"role": "system", "content": "Routing fixture instructions"},
            {"role": "user", "content": "do WORK"}]}
        headers = {"User-Agent": "claude-code/2.1.296", "X-Claude-Code-Agent-Id": "fixture-child"}
        status, data = self.call(body, headers=headers)
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-second")
        status, data = self.call(body, session="not-subagent", headers={"User-Agent": "claude-code/2"})
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-first")

    def test_only_validated_caller_key_can_supply_bound_workspace(self):
        self.rule({"use": "second", "folders": [str(self.workspace)]})
        self.start()
        forged = {"X-Cwd": str(self.workspace), "X-OpenCode-Directory": str(self.workspace),
                  "X-Zcalld-Workspace": str(self.workspace), "X-Agent-Name": "project",
                  "Authorization": "Bearer made-up-key"}
        status, data = self.call(self.body(f"<cwd>{self.workspace}</cwd>"), headers=forged)
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-first")
        forged["Authorization"] = "Bearer fixture-bound"
        status, data = self.call(self.body(), headers=forged)
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-second")

    def test_pause_rechecks_workspace_even_for_held_turn(self):
        self.rule({"use": "first", "pause": True, "exceptFolders": [str(self.workspace)]},
                  {"use": "first", "where": [{"field": "message", "op": "contains", "value": "fixture"}]},
                  affinity="turn")
        self.start()
        status, data = self.call(self.body(), headers={"Authorization": "Bearer fixture-bound"})
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-first")
        status, data = self.call(self.body(), headers={"X-Cwd": str(self.workspace)})
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-second")

    def test_privacy_precedes_where_and_external_classifier(self):
        self.state["groups"][0]["original"] = {
            "affinity": "none", "classifier": "decider/mock", "effort": "auto",
            "rules": [
                {"use": "first", "where": [{"field": "message", "op": "contains",
                                            "value": "jane.doe@acme.io"}]},
                {"use": "first", "intent": "simple"},
                {"use": "second", "intent": "complex", "useEffort": "high",
                 "where": [{"field": "message", "op": "excludes", "value": "jane.doe@acme.io"}]}]}
        self.save()
        self.start()
        text = "Design this feature for jane.doe@acme.io"
        for stream in (False, True):
            status, data = self.call(self.body(text, stream=stream), session=f"privacy-{stream}")
            self.assertEqual(status, 200, data)
            self.assertEqual(self.model(), "upstream-second")
            self.assertNotIn("jane.doe@acme.io", json.dumps(self.mock.records))
            if stream:
                restored = "".join(value["choices"][0]["delta"].get("content", "")
                    for line in data.decode().splitlines()
                    if line.startswith("data: ") and line != "data: [DONE]"
                    and (value := json.loads(line[6:])).get("choices"))
            else:
                restored = json.loads(data)["choices"][0]["message"]["content"]
            self.assertEqual(restored, text)
        self.assertTrue(any(path.endswith("/systemone") for path, _ in self.mock.records))

    def test_groups_file_validates_before_atomic_change_and_discovers_helper(self):
        self.state.pop("rules_worker")
        self.save()
        definition = self.root / "group-definition.json"
        definition.write_text(json.dumps({"members": ["first", "second"], "mode": "rotate",
            "rules": [{"use": "second", "where": [{"field": "system", "op": "matches",
                                                   "value": "^fixture"}]}]}), encoding="utf-8", newline="\n")
        self.cli("groups", "set", "group/test", "--file", str(definition))
        state = json.loads(self.groups.read_text(encoding="utf-8"))
        self.assertEqual(Path(state["rules_worker"]).resolve(), self.args.pluginctl)
        self.assertEqual(state["groups"][0]["original"]["rules"][0]["where"][0]["op"], "matches")
        partial = {"rules": state["groups"][0]["original"]["rules"]}
        definition.write_text(json.dumps(partial), encoding="utf-8", newline="\n")
        self.cli("groups", "set", "group/test", "--file", str(definition))
        self.assertEqual(json.loads(self.groups.read_text(encoding="utf-8"))["groups"][0]["mode"], "rotate")
        before = self.groups.read_bytes(), self.config.read_bytes()
        invalid = json.loads(definition.read_text(encoding="utf-8"))
        invalid["rules"][0]["where"][0]["value"] = "(?<=unsupported)lookbehind"
        definition.write_text(json.dumps(invalid), encoding="utf-8", newline="\n")
        self.cli("groups", "set", "group/test", "--file", str(definition), ok=False)
        self.assertEqual((self.groups.read_bytes(), self.config.read_bytes()), before)
        for malformed in ({"members": []}, {"members": "first"}, {"mode": False}):
            definition.write_text(json.dumps(malformed), encoding="utf-8", newline="\n")
            self.cli("groups", "set", "group/test", "--file", str(definition), ok=False)
            self.assertEqual((self.groups.read_bytes(), self.config.read_bytes()), before)
        self.start()
        status, data = self.call({"model": "group/test", "messages": [
            {"role": "system", "content": "Fixture system"},
            {"role": "user", "content": "hello"}]})
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-second")

    def test_migration_validates_conditions_without_changing_source(self):
        source = self.root / "source-magpie"
        source.mkdir()
        (source / "settings.json").write_text("{}", encoding="utf-8", newline="\n")
        providers = {"providers": [{"id": "local", "chat": "http://127.0.0.1:11434/v1", "key": ""}],
                     "groups": [{"id": "fixture", "name": "fixture", "members": ["local/one", "local/two"],
                                 "routing": "order", "rules": [{"use": "local/two",
                                     "where": [{"field": "message", "op": "matches", "value": "^fixture"}]}]}]}
        provider_path = source / "providers.json"
        provider_path.write_text(json.dumps(providers), encoding="utf-8", newline="\n")
        catalog = self.root / "source-catalog.json"
        catalog.write_text(json.dumps({"data": [{"id": model} for model in
                                               ["local/one", "local/two", "group/fixture"]]}), encoding="utf-8", newline="\n")
        before = {path.name: path.read_bytes() for path in source.iterdir()}
        result = self.cli("migrate", "magpie", "--source", str(source), "--catalog", str(catalog))
        self.assertTrue(result["ready"], result)
        self.assertEqual(before, {path.name: path.read_bytes() for path in source.iterdir()})
        providers["groups"][0]["rules"][0]["where"][0]["op"] = "unknown"
        provider_path.write_text(json.dumps(providers), encoding="utf-8", newline="\n")
        result = self.cli("migrate", "magpie", "--source", str(source), "--catalog", str(catalog))
        self.assertFalse(result["ready"], result)
        self.assertEqual(result["summary"]["compatibility_gaps"], 1, result)

    def test_simple_groups_do_not_require_or_start_a_rule_helper(self):
        self.state["rules_worker"] = str(self.root / "does-not-exist")
        self.save()
        self.start()
        status, data = self.call(self.body())
        self.assertEqual(status, 200, data)
        self.assertEqual(self.model(), "upstream-first")

    @unittest.skipUnless(sys.platform == "linux", "Linux kernel process/socket evidence")
    def test_tcp_directory_is_the_actual_client_process_directory(self):
        self.rule({"use": "second", "folders": [str(self.workspace)]})
        self.start()
        code = (
            "import http.client,json,sys;"
            "c=http.client.HTTPConnection('127.0.0.1',int(sys.argv[1]),timeout=15);"
            "c.request('POST','/v1/chat/completions',json.dumps({'model':'group/test',"
            "'messages':[{'role':'user','content':'fixture'}]}),"
            "{'Content-Type':'application/json','X-Cwd':'/forged'});"
            "r=c.getresponse();print(r.status);print(r.read().decode());c.close()"
        )
        child = subprocess.run([sys.executable, "-c", code, str(self.port)], cwd=self.workspace,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25)
        self.assertEqual(child.returncode, 0, child.stderr.decode())
        self.assertTrue(child.stdout.startswith(b"200\n"), child.stdout)
        self.assertEqual(self.model(), "upstream-second")

    @unittest.skipUnless(sys.platform == "linux", "Linux Unix peer credentials")
    def test_unix_socket_peer_cwd_ignores_client_directory_hint(self):
        self.rule({"use": "second", "folders": [str(self.workspace)]})
        unix = self.root / "gateway.sock"
        self.config.write_text(self.config.read_text(encoding="utf-8").replace("unix_socket=none", f"unix_socket={unix}"), encoding="utf-8", newline="\n")
        self.start()
        code = (
            "import socket,sys,json;"
            "s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);"
            "b=json.dumps({'model':'group/test','messages':[{'role':'user','content':'fixture'}]}).encode();"
            "s.sendall(b'POST /v1/chat/completions HTTP/1.1\\r\\nHost: localhost\\r\\nX-Cwd: /forged\\r\\n"
            "Content-Type: application/json\\r\\nContent-Length: '+str(len(b)).encode()+b'\\r\\n\\r\\n'+b);"
            "data=b''\n"
            "while True:\n"
            " p=s.recv(65536)\n"
            " if not p: break\n"
            " data+=p\n"
            "print(data.decode());s.close()\n"
        )
        child = subprocess.run([sys.executable, "-c", code, str(unix)], cwd=self.workspace,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25)
        self.assertEqual(child.returncode, 0, child.stderr.decode())
        self.assertTrue(child.stdout.startswith(b"HTTP/1.1 200"), child.stdout)
        self.assertEqual(self.model(), "upstream-second")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name, variable in [("binary", "ZCALLD_TEST_BINARY"), ("pluginctl", "ZCALLD_TEST_PLUGINCTL")]:
        parser.add_argument("--" + name, default=os.environ.get(variable),
                            type=lambda value: Path(value).resolve())
    for name in ("routing-plugin", "redact-plugin", "privacy-worker"):
        parser.add_argument("--" + name, required=True, type=lambda value: Path(value).resolve())
    Advanced.args, remaining = parser.parse_known_args()
    if not Advanced.args.binary or not Advanced.args.pluginctl:
        parser.error("--binary and --pluginctl (or their ZCALLD_TEST_* variables) are required")
    unittest.main(argv=[__file__, *remaining], verbosity=2)
