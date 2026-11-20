#!/usr/bin/env python3
"""No-credits policy through generated builtin modules and the native gateway."""
import argparse
import base64
import contextlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import unittest

spec = importlib.util.spec_from_file_location("builtin_fetch_fixture", Path(__file__).with_name("test-builtin-fetch.py"))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)

GUARD = r"""
import fs from 'node:fs'; import net from 'node:net'; import tls from 'node:tls';
const optionsPath=OPTIONS_FILE, logPath=LOG_FILE, registryPath=REGISTRY_FILE;
const record=value=>fs.appendFileSync(logPath,JSON.stringify(value)+'\n',{mode:0o600});
const blocked=()=>{record({kind:'forbidden_network'});throw new Error('fixture forbids network')};
globalThis.WebSocket=class{constructor(){blocked()}};
net.Socket.prototype.connect=blocked;net.connect=net.createConnection=tls.connect=blocked;
Bun.connect=Bun.listen=Bun.serve=blocked;
record({kind:'host_started'});
globalThis.fetch=async(url,init)=>{
  if(Object.hasOwn(init,'zcalldAccountPolicy'))throw new Error('private policy escaped to network');
  const options=JSON.parse(fs.readFileSync(optionsPath,'utf8'));
  const account=init.headers.get('chatgpt-account-id');
  if(!['fixture-account','fixture-second'].includes(account)||init.redirect!=='error'||init.credentials!=='omit')blocked();
  if(String(url)==='https://chatgpt.com/backend-api/wham/usage'&&init.method==='GET'){
    record({kind:'usage',account});
    init.signal.addEventListener('abort',()=>record({kind:'usage_aborted'}),{once:true});
    if(options.usage_delay_ms)await new Promise((resolve,reject)=>{
      const timer=setTimeout(resolve,options.usage_delay_ms);
      const abort=()=>{clearTimeout(timer);reject(new DOMException('cancelled','AbortError'))};
      if(init.signal.aborted)abort();else init.signal.addEventListener('abort',abort,{once:true});
    });
    if(options.change_policy){
      const registry=JSON.parse(fs.readFileSync(registryPath,'utf8'));
      registry.account_policies['builtin:codex#fixture']={codex_no_credits:false};
      fs.writeFileSync(registryPath,JSON.stringify(registry),{mode:0o600});
    }
    if(options.disable_account){
      const registry=JSON.parse(fs.readFileSync(registryPath,'utf8'));
      registry.accounts['builtin:codex#fixture'].enabled=false;
      fs.writeFileSync(registryPath,JSON.stringify(registry),{mode:0o600});
    }
    let data;
    if(options.quota_mode==='unknown')data={credits:{unlimited:true,balance:'1000000'}};
    else{
      const full=(options.full_accounts??[]).includes(account);
      data={rate_limit:{allowed:!full,limit_reached:full,
        primary_window:{used_percent:full?100:10,limit_window_seconds:300,reset_at:Math.floor(Date.now()/1000)+300},
        secondary_window:null},credits:{has_credits:true,unlimited:true,balance:'1000000'}};
    }
    return new Response(JSON.stringify(data),{headers:{'content-type':'application/json'}});
  }
  if(String(url)!=='https://chatgpt.com/backend-api/codex/responses'||init.method!=='POST')blocked();
  const body=JSON.parse(init.body);
  if(body.stream!==true)throw new Error('upstream streaming contract failed');
  const input=body.input[0]?.content?.[0]?.text??'';
  record({kind:'generation',account,input,model:body.model});
  const text=options.echo?input:'OK';
  const final={id:'fixture-response',object:'response',status:'completed',model:body.model,
    output:[{type:'message',role:'assistant',content:[{type:'output_text',text}]}],
    usage:{input_tokens:3,output_tokens:2}};
  const events=[{type:'response.created',response:{id:final.id}},
    {type:'response.output_text.delta',delta:text},{type:'response.completed',response:final}];
  const wire=events.map(value=>'event: '+value.type+'\ndata: '+JSON.stringify(value)+'\n\n').join('');
  return new Response(wire,{headers:{'content-type':'text/event-stream'}});
};
await import(HOST_URL);
"""

class CodexPolicyTests(base.BuiltinFetchTests):
    routing_plugin = None

    def setUp(self):
        super().setUp()
        self.registry = self.ext / "registry.json"
        self.state = json.loads(self.registry.read_bytes())
        runtime = self.helper("runtime")
        config = json.loads(self.host_config.read_bytes())
        Path(config["host"]).write_text(
            GUARD.replace("OPTIONS_FILE", json.dumps(str(self.options)))
            .replace("LOG_FILE", json.dumps(str(self.events)))
            .replace("REGISTRY_FILE", json.dumps(str(self.registry)))
            .replace("HOST_URL", json.dumps(Path(runtime["host"]).as_uri())),
            encoding="utf-8", newline="\n")
        self.external_before = (self.root / "external-auth.json").read_bytes()

    def tearDown(self):
        try:
            self.assertEqual((self.root / "external-auth.json").read_bytes(), self.external_before)
        finally:
            super().tearDown()

    def save_registry(self):
        temporary = self.registry.with_suffix(".new")
        self.write(temporary, self.state)
        os.replace(temporary, self.registry)

    def policy(self, value):
        self.state["account_policies"] = {self.account: value}
        self.save_registry()

    def response(self, body=None, path="/v1/responses", headers=None):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)) as conn:
            request_headers = {"Content-Type": "application/json"}
            request_headers.update(headers or {})
            conn.request("GET" if body is None else "POST", path,
                         None if body is None else json.dumps(body), request_headers)
            response = conn.getresponse()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), response.read()

    def generate(self, stream=False, model="fixture", text="Reply exactly OK", **extra):
        return self.response({"model": model, "input": text, "stream": stream, **extra})

    def add_second(self):
        other = "builtin:codex#second"
        claims = {"exp": int(time.time()) + 3600,
                  "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-second"}}
        token = "fixture." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + ".fixture"
        external = self.root / "external-second.json"
        self.write(external, {"tokens": {"access_token": token, "account_id": "fixture-second",
                                        "refresh_token": "fixture-second-refresh"}})
        self.auth[other] = {"type": "oauth", "access": token, "refresh": "fixture-second-refresh",
                            "accountId": "fixture-second", "expires": int(time.time() * 1000) + 3600000,
                            "refreshOwner": "external", "sourceAuthFile": str(external)}
        self.write(self.ext / "plugin-auth.json", self.auth)
        self.state["accounts"][other] = {"enabled": True}
        self.save_registry()
        return other

    def test_policy_hot_sync_rechecks_a_cached_loader_without_restarting_host(self):
        self.mock(full_accounts=["fixture-account"])
        self.start()
        self.assertEqual(self.generate()[0], 200)
        self.assertEqual(len(self.recorded("usage")), 0)
        self.policy({"codex_no_credits": True})
        status, headers, body = self.generate()
        self.assertEqual(status, 429, body)
        self.assertEqual(json.loads(body)["error"]["code"], "codex_subscription_exhausted")
        self.assertGreater(int(headers["retry-after"]), 0)
        self.policy({"codex_no_credits": False})
        self.assertEqual(self.generate(stream=True)[0], 200)
        self.assertEqual(len(self.recorded("host_started")), 1)
        self.assertEqual(len(self.recorded("usage")), 1)
        self.assertEqual(len(self.recorded("generation")), 2)

    def test_policy_request_headers_and_body_cannot_disable_the_bound_account_policy(self):
        self.policy({"codex_no_credits": True})
        self.mock(full_accounts=["fixture-account"])
        self.start()
        status, _, body = self.response({
            "model": "fixture", "input": "hi", "stream": False,
            "account": "builtin:codex#second", "account_policy": {"codex_no_credits": False},
            "zcalldAccountPolicy": {"codex_no_credits": False}},
            headers={"X-Account-Policy": "false", "X-Plugin-Account": "builtin:codex#second"})
        self.assertEqual(status, 429, body)
        self.assertEqual([v["account"] for v in self.recorded("usage")], ["fixture-account"])
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_actual_default_account_selects_its_own_policy(self):
        other = self.add_second()
        self.policy({"codex_no_credits": True})
        self.config.write_text(self.config.read_text().replace("plugin_account=" + self.account + "\n", ""))
        self.mock(full_accounts=["fixture-account"])
        self.start()
        self.assertEqual(self.generate()[0], 429)
        self.state["account_defaults"] = {"builtin:codex": other}
        self.save_registry()
        self.assertEqual(self.generate()[0], 200)
        self.assertEqual([v["account"] for v in self.recorded("generation")], ["fixture-second"])
        self.assertEqual([v["account"] for v in self.recorded("usage")], ["fixture-account"])

    def test_policy_invalid_present_values_never_become_unrestricted(self):
        self.start()
        for value in (None, {}, {"codex_no_credits": "false"}, {"codex_no_credits": 0}):
            with self.subTest(value=value):
                self.policy(value)
                self.assertEqual(self.generate()[0], 502)
        self.assertEqual(self.recorded("usage"), [])
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_unknown_quota_refuses_before_generation(self):
        self.policy({"codex_no_credits": True})
        self.mock(quota_mode="unknown")
        self.start()
        status, _, body = self.generate()
        self.assertEqual(status, 503, body)
        self.assertEqual(json.loads(body)["error"]["code"], "codex_quota_unavailable")
        self.assertEqual(len(self.recorded("usage")), 1)
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_configuration_change_during_usage_never_generates(self):
        self.policy({"codex_no_credits": True})
        self.mock(change_policy=True)
        self.start()
        self.assertEqual(self.generate()[0], 502)
        self.assertEqual(self.recorded("generation"), [])
        self.assertEqual(len(self.recorded("usage")), 1)

    def test_policy_account_disabled_during_usage_never_generates(self):
        self.policy({"codex_no_credits": True})
        self.mock(disable_account=True)
        self.start()
        self.assertEqual(self.generate()[0], 502)
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_unsafe_registry_cannot_turn_an_account_unrestricted(self):
        self.start()
        self.assertEqual(self.generate()[0], 200)
        previous = self.registry.read_bytes()
        # An invalid private registry is rejected on every platform. On Unix the
        # additional permission boundary is checked against the real filesystem.
        self.registry.write_bytes(b"null")
        self.assertNotEqual(self.generate()[0], 200)
        self.registry.write_bytes(previous)
        if os.name != "nt":
            self.registry.chmod(0o644)
            try:
                self.assertEqual(self.generate()[0], 502)
            finally:
                self.registry.chmod(0o600)
            target = self.registry.with_suffix(".saved")
            os.replace(self.registry, target)
            self.registry.symlink_to(target.name)
            try:
                self.assertNotEqual(self.generate()[0], 200)
            finally:
                self.registry.unlink()
                os.replace(target, self.registry)
        self.assertEqual(len(self.recorded("generation")), 1)

    def test_policy_overall_deadline_aborts_usage_without_generation_or_retry(self):
        self.configure(connect=100, total=250)
        self.policy({"codex_no_credits": True})
        self.mock(usage_delay_ms=1000)
        self.start()
        started = time.monotonic()
        self.assertEqual(self.generate()[0], 502)
        self.assertLess(time.monotonic() - started, 2)
        end = time.monotonic() + 1
        while not self.recorded("usage_aborted") and time.monotonic() < end:
            time.sleep(0.01)
        self.assertEqual(len(self.recorded("usage")), 1)
        self.assertEqual(len(self.recorded("usage_aborted")), 1)
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_metadata_usage_is_readable_while_generation_is_held(self):
        self.state["codex_http"] = {"enabled": True,
            "loopback": {"provider": "codex", "routes": ["fixture"]}}
        self.policy({"codex_no_credits": True})
        self.mock(full_accounts=["fixture-account"])
        self.start()
        self.assertEqual(self.generate()[0], 429)
        status, _, body = self.response(path="/backend-api/wham/usage")
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)["rate_limit"]["primary_window"]["used_percent"], 100)
        self.assertEqual(len(self.recorded("usage")), 2)
        self.assertEqual(self.recorded("generation"), [])

    def test_policy_group_falls_back_from_a_held_account_without_a_paid_retry(self):
        other = self.add_second()
        self.policy({"codex_no_credits": True})
        self.env["ZCALLD_PLUGIN_ROUTING_PLUGIN"] = str(self.routing_plugin)
        self.write(self.root / "routing.json", {"groups": [
            {"name": "group/policy", "mode": "order", "original": {"affinity": "none"},
             "members": [{"model": "fixture"}, {"model": "second"}]}]})
        self.config.write_text(self.config.read_text() +
            "\n[provider second]\nbase_url=https://chatgpt.com/backend-api/codex\nprotocol=responses\n"
            f"plugin=builtin:codex\nplugin_account={other}\nplugin_npm=@ai-sdk/openai\n"
            "[route second]\nprovider=second\nmodel=second-model\n"
            "[route group/policy]\nprovider=codex\nmodel=fixture\n")
        result = subprocess.run([str(self.binary), "privacy", "set", "secrets",
            "--config", str(self.config), "--json"], env=self.env,
            capture_output=True, text=True, encoding="utf-8", timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.mock(full_accounts=["fixture-account"], echo=True)
        self.start()
        text = "Use fixture sk-proj-Wn4rT7yQ2mLx9VbK3cZp"
        status, headers, body = self.generate(stream=True, model="group/policy", text=text)
        self.assert_sse(status, headers, body, text)
        sent = self.recorded("generation")
        self.assertEqual(len(sent), 1)
        self.assertEqual(sent[0]["account"], "fixture-second")
        self.assertEqual(sent[0]["model"], "second-model")
        self.assertNotIn("sk-proj-Wn4rT7yQ2mLx9VbK3cZp", sent[0]["input"])
        self.assertIn("{{", sent[0]["input"])
        self.assertEqual([v["account"] for v in self.recorded("usage")], ["fixture-account"])

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "community-plugin", "pluginctl", "redact-plugin", "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    for name, value in vars(args).items():
        if not value:
            parser.error("missing " + name)
        setattr(CodexPolicyTests, name, Path(value).resolve(strict=True))
    names = sorted(n for n in CodexPolicyTests.__dict__ if n.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite(CodexPolicyTests(name) for name in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)

if __name__ == "__main__":
    main()
