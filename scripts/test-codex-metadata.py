#!/usr/bin/env python3
"""Native Codex metadata contract: generated bundle, fake auth, no real network."""
import argparse
import contextlib
import http.client
import importlib.util
import json
import socket
import ipaddress
import os
from pathlib import Path
import shutil
import time
import unittest

spec = importlib.util.spec_from_file_location("builtin_fetch_fixture", Path(__file__).with_name("test-builtin-fetch.py"))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)

GUARD = r"""
import fs from 'node:fs'; import net from 'node:net'; import tls from 'node:tls';
const optionsPath=OPTIONS_FILE, logPath=LOG_FILE;
const record=value=>fs.appendFileSync(logPath,JSON.stringify(value)+'\n',{mode:0o600});
record({kind:'host_started'});
const blocked=()=>{record({kind:'forbidden_network'});throw new Error('fixture forbids network')};
globalThis.WebSocket=class{constructor(){blocked()}};
net.Socket.prototype.connect=blocked;net.connect=net.createConnection=tls.connect=blocked;
Bun.connect=Bun.listen=Bun.serve=blocked;
const allowed=new Set(['/backend-api/codex/models','/backend-api/wham/usage',
  '/backend-api/wham/accounts/check','/backend-api/wham/config/bundle']);
globalThis.fetch=async(url,init)=>{
  const parsed=new URL(String(url));
  if(parsed.origin!=='https://chatgpt.com'||!allowed.has(parsed.pathname)||init?.method!=='GET')blocked();
  const options=JSON.parse(fs.readFileSync(optionsPath,'utf8'));
  if(init.redirect!=='error'||init.credentials!=='omit'||init.body!==undefined||
      init.headers.get('chatgpt-account-id')!==(options.expected_account??'fixture-account'))
    throw new Error('fixture metadata contract failed');
  record({kind:'metadata',path:parsed.pathname,query:parsed.search,method:init.method});
  init.signal.addEventListener('abort',()=>record({kind:'aborted'}),{once:true});
  if(options.delay_ms){
    await new Promise((resolve,reject)=>{
      const timer=setTimeout(resolve,options.delay_ms);
      const abort=()=>{clearTimeout(timer);reject(new DOMException('fixture aborted','AbortError'))};
      if(init.signal.aborted)abort();else init.signal.addEventListener('abort',abort,{once:true});
    });
  }
  const model={slug:'fixture',display_name:'Official fixture',base_instructions:'UPSTREAM INSTRUCTIONS {{personality}}',
    model_messages:{instructions_template:'UPSTREAM TEMPLATE',instructions_variables:{personality_default:'official'}},
    supported_reasoning_levels:[{effort:'high',description:'Official'}],default_reasoning_level:'high',shell_type:'shell_command',
    visibility:'list',supported_in_api:true,priority:3,support_verbosity:true,
    truncation_policy:{mode:'bytes',limit:100000},experimental_supported_tools:['web'],
    opaque_extension:{retained:['a',true,7]}};
  const value=parsed.pathname.endsWith('/models')?{models:[model,{...model,slug:'not-bound'}],custom_extension:true}:
    parsed.pathname.endsWith('/usage')?{plan_type:'plus',rate_limit:{allowed:true,limit_reached:false,
      primary_window:{used_percent:37,limit_window_seconds:18000,reset_after_seconds:1800,reset_at:1900000000}}}:
    parsed.pathname.endsWith('/check')?{accounts:{fixture:{account:{account_id:'fixture-account'},is_eligible_for_codex:true}},account_ordering:['fixture']}:
    {config_toml:{enterprise_managed:[]}};
  if(options.model_upgrade&&parsed.pathname.endsWith('/models'))value.models[0].upgrade={model:'not-bound'};
  if(options.bad_usage_schema&&parsed.pathname.endsWith('/usage'))value.rate_limit.primary_window.used_percent=37.5;
  let raw=JSON.stringify(value);
  if(options.bad_json)raw='not valid JSON';
  if(options.large)raw=JSON.stringify({padding:'x'.repeat((1<<20)+1)});
  if(options.missing_instructions){delete value.models[0].base_instructions;delete value.models[0].model_messages;raw=JSON.stringify(value);}
  const bytes=new TextEncoder().encode(raw);let at=0;
  const body=new ReadableStream({
    pull(c){if(at===bytes.length){c.close();return;}
      const end=Math.min(bytes.length,at+8192);c.enqueue(bytes.slice(at,end));at=end;},
    cancel(){record({kind:'body_cancelled'})}
  },{highWaterMark:0});
  return new Response(body,{status:options.status??200,headers:{
    'content-type':options.content_type??'application/json',
    'etag':'"official-etag"','set-cookie':'PRIVATE-COOKIE-CANARY',
    'authorization':'Bearer PRIVATE-HEADER-CANARY',
    'location':'https://untrusted.invalid/should-not-follow'}});
};
await import(HOST_URL);
"""


class CodexMetadataTests(base.BuiltinFetchTests):
    def setUp(self):
        super().setUp()
        host = json.loads(self.host_config.read_text())
        generated = self.helper("runtime")
        Path(host["host"]).write_text(
            GUARD.replace("OPTIONS_FILE", json.dumps(str(self.options)))
            .replace("LOG_FILE", json.dumps(str(self.events)))
            .replace("HOST_URL", json.dumps(Path(generated["host"]).as_uri())),
            encoding="utf-8", newline="\n")
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["fixture"]}})
        self.auth_before = (self.ext / "plugin-auth.json").read_bytes()
        self.auth_mtime = (self.ext / "plugin-auth.json").stat().st_mtime_ns
        self.external_before = (self.root / "external-auth.json").read_bytes()
        self.external_mtime = (self.root / "external-auth.json").stat().st_mtime_ns

    def binding(self, value):
        file = self.root / "codex-binding.json"
        self.write(file, value)
        result = self.helper("codex", "configure", "--file", str(file))
        self.assertTrue(result["ok"])

    def metadata(self, path="/backend-api/wham/usage", method="GET", headers=(), body=b"", host=None):
        with contextlib.closing(http.client.HTTPConnection(host or "127.0.0.1", self.port, timeout=8)) as conn:
            conn.putrequest(method, path, skip_host=any(name.lower() == "host" for name, _ in headers))
            for key, value in headers:
                conn.putheader(key, value)
            if body:
                conn.putheader("Content-Length", len(body))
            conn.endheaders(body or None)
            response = conn.getresponse()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), response.read()

    def tearDown(self):
        try:
            self.stop()
            self.assertEqual((self.ext / "plugin-auth.json").read_bytes(), self.auth_before)
            self.assertEqual((self.ext / "plugin-auth.json").stat().st_mtime_ns, self.auth_mtime)
            self.assertEqual((self.root / "external-auth.json").read_bytes(), self.external_before)
            self.assertEqual((self.root / "external-auth.json").stat().st_mtime_ns, self.external_mtime)
        finally:
            super().tearDown()

    def test_four_official_contracts_share_the_bounded_native_transport(self):
        self.start()
        for path in ["/backend-api/codex/models?client_version=0.170.0",
                     "/backend-api/wham/usage", "/backend-api/wham/accounts/check",
                     "/backend-api/wham/config/bundle"]:
            status, headers, body = self.metadata(path)
            self.assertEqual(status, 200, body.decode())
            self.assertIsInstance(json.loads(body), dict)
            self.assertNotIn("set-cookie", headers)
            self.assertNotIn("authorization", headers)
            self.assertNotIn("location", headers)
            self.assertNotIn(b"PRIVATE", body)
        self.assertEqual(len(self.recorded("metadata")), 4)

    def test_exact_custom_base_aliases_map_only_to_official_readonly_paths(self):
        self.start()
        for path in ["/api/codex/usage", "/api/codex/accounts/check", "/api/codex/config/bundle",
                     "/codex/config/bundle"]:
            self.assertEqual(self.metadata(path)[0], 200)
        self.assertEqual([r["path"] for r in self.recorded("metadata")],
                         ["/backend-api/wham/usage", "/backend-api/wham/accounts/check",
                          "/backend-api/wham/config/bundle", "/backend-api/wham/config/bundle"])

    def test_explicit_alias_keeps_instructions_and_opaque_model_capabilities(self):
        self.config.write_text(self.config.read_text() + "\n[route public-alias]\nprovider=codex\nmodel=fixture\n")
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["public-alias"]}})
        self.start()
        status, headers, raw = self.metadata("/backend-api/codex/models?client_version=0.170.0")
        self.assertEqual(status, 200, raw.decode())
        data = json.loads(raw)
        self.assertEqual([m["slug"] for m in data["models"]], ["public-alias"])
        item = data["models"][0]
        self.assertEqual(item["base_instructions"], "UPSTREAM INSTRUCTIONS {{personality}}")
        self.assertEqual(item["model_messages"]["instructions_template"], "UPSTREAM TEMPLATE")
        self.assertEqual(item["opaque_extension"], {"retained": ["a", True, 7]})
        self.assertTrue(data["custom_extension"])
        self.assertRegex(headers["etag"], r'^"zcalld-[0-9a-f]{64}"$')

    def test_model_upgrade_uses_an_advertised_alias_or_is_omitted(self):
        self.config.write_text(self.config.read_text() +
            "\n[route public-alias]\nprovider=codex\nmodel=fixture\n" +
            "\n[route next-alias]\nprovider=codex\nmodel=not-bound\n")
        self.binding({"enabled": True, "loopback": {
            "provider": "codex", "routes": ["public-alias", "next-alias"]}})
        self.mock(model_upgrade=True)
        self.start()
        status, _, raw = self.metadata("/backend-api/codex/models?client_version=0.170.0")
        self.assertEqual(status, 200, raw.decode())
        models = json.loads(raw)["models"]
        self.assertEqual([model["slug"] for model in models], ["public-alias", "next-alias"])
        self.assertEqual(models[0]["upgrade"], {"model": "next-alias"})
        self.binding({"enabled": True, "loopback": {
            "provider": "codex", "routes": ["public-alias"]}})
        status, _, raw = self.metadata("/backend-api/codex/models?client_version=0.170.0")
        self.assertEqual(status, 200, raw.decode())
        self.assertIsNone(json.loads(raw)["models"][0]["upgrade"])

    def test_methods_query_and_unrecognized_paths_cannot_become_upstream_requests(self):
        self.start()
        for method in ["POST", "OPTIONS", "HEAD", "DELETE"]:
            with self.subTest(method=method):
                self.assertEqual(self.metadata(method=method, headers=[("Content-Type", "application/json")], body=b"{}" if method == "POST" else b"")[0], 405)
        for path in ["/backend-api/codex/models", "/backend-api/codex/models?client_version=x&url=evil",
                     "/backend-api/codex/models?client_version=a&client_version=b",
                     "/backend-api/wham/usage?account=other"]:
            self.assertEqual(self.metadata(path)[0], 400)
        self.assertEqual(self.metadata(body=b"{}")[0], 400)
        self.assertNotEqual(self.metadata("/api/codex/tasks")[0], 200)
        self.assertEqual(self.recorded("metadata"), [])

    def test_no_first_enabled_account_fallback(self):
        text = self.config.read_text().replace(f"plugin_account={self.account}\n", "")
        self.config.write_text(text)
        self.start()
        status, _, body = self.metadata()
        self.assertEqual(status, 403, body.decode())
        self.assertEqual(self.recorded("metadata"), [])
        registry = json.loads((self.ext / "registry.json").read_bytes())
        registry["account_defaults"] = {"builtin:codex": self.account}
        self.write(self.ext / "registry.json", registry)
        self.assertEqual(self.metadata()[0], 200)

    def test_disabled_and_wrong_provider_bindings_fail_before_network(self):
        self.binding({"enabled": True, "loopback": {"provider": "missing", "routes": ["fixture"]}})
        self.start()
        self.assertEqual(self.metadata()[0], 403)
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["fixture"]}})
        registry = json.loads((self.ext / "registry.json").read_bytes())
        registry["accounts"][self.account]["enabled"] = False
        self.write(self.ext / "registry.json", registry)
        self.assertEqual(self.metadata()[0], 403)
        self.assertEqual(self.recorded("metadata"), [])

    def test_client_headers_cannot_grant_a_missing_caller_binding(self):
        self.binding({"enabled": True, "callers": {"verified-caller": {"provider": "codex", "routes": ["fixture"]}}})
        self.start()
        status, _, body = self.metadata(headers=[("X-Caller-Key-Id", "verified-caller"),
            ("X-Loopback", "true"), ("ChatGPT-Account-Id", "fixture-account")])
        self.assertEqual(status, 403, body.decode())
        self.assertEqual(self.recorded("metadata"), [])

    def test_account_hint_is_reject_only_and_duplicate_headers_are_not_collapsed(self):
        self.start()
        for values in [[""], ["fixture-account", "fixture-account"], ["fixture-account", "other"]]:
            status, _, body = self.metadata(headers=[("ChatGPT-Account-Id", v) for v in values])
            self.assertEqual(status, 400, body.decode())
        self.assertEqual(self.metadata(headers=[("ChatGPT-Account-Id", "different")])[0], 403)
        self.assertEqual(self.recorded("metadata"), [])
        self.assertEqual(self.metadata(headers=[("ChatGPT-Account-Id", "fixture-account")])[0], 200)
        self.assertEqual(len(self.recorded("metadata")), 1)

    def test_bad_upstream_metadata_does_not_poison_the_same_worker(self):
        self.start()
        for options in [{"status": 302}, {"content_type": "text/html"}, {"bad_json": True},
                        {"large": True}, {"missing_instructions": True}]:
            self.mock(**options)
            path = "/backend-api/codex/models?client_version=0.170.0" if options.get("missing_instructions") else "/backend-api/wham/usage"
            self.assertEqual(self.metadata(path)[0], 502)
            self.mock()
            self.assertEqual(self.metadata()[0], 200)
        self.assertEqual(len(self.recorded("metadata")), 10)

    def test_overall_deadline_cancels_once_without_replaying(self):
        self.configure(total=250)
        self.mock(delay_ms=1000)
        self.start()
        status, _, body = self.metadata()
        self.assertIn(status, [502, 504], body.decode())
        time.sleep(0.05)
        self.assertEqual(len(self.recorded("metadata")), 1)
        self.assertEqual(len(self.recorded("aborted")), 1)

    def test_disable_is_local_and_keeps_old_binding(self):
        self.start()
        result = self.helper("codex", "disable")
        self.assertFalse(result["codex_http"]["enabled"])
        self.assertEqual(result["codex_http"]["loopback"]["provider"], "codex")
        self.assertNotEqual(self.metadata()[0], 200)
        self.assertEqual(self.recorded("metadata"), [])


    def test_real_lan_peer_cannot_claim_loopback_or_another_caller_with_headers(self):
        addresses = []
        try:
            addresses = [row[4][0] for row in socket.getaddrinfo(socket.gethostname(), 0, socket.AF_INET, socket.SOCK_STREAM)]
        except OSError:
            pass
        # UDP connect here only asks the local kernel for its chosen source address;
        # no packet is sent, and no provider/remote service is contacted.
        if not any(not ipaddress.ip_address(value).is_loopback for value in addresses):
            try:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
                    route.connect(("192.0.2.1", 9))
                    addresses.append(route.getsockname()[0])
            except OSError:
                pass
        host = next((value for value in addresses if not ipaddress.ip_address(value).is_loopback and not ipaddress.ip_address(value).is_unspecified), None)
        if host is None:
            self.skipTest("runner has no non-loopback IPv4 interface for the actual LAN peer case")
        self.write(self.root / "caller-keys.json", [
            {"id": "bound", "name": "Bound fixture", "lan": True, "secret": "fixture-bound-key"},
            {"id": "other", "name": "Other fixture", "lan": True, "secret": "fixture-other-key"},
        ])
        self.config.write_text(self.config.read_text()
            .replace(f"listen=127.0.0.1:{self.port}", f"listen=0.0.0.0:{self.port}")
            .replace("[server]\n", "[server]\ncaller_keys_file=caller-keys.json\n"))
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["fixture"]},
                      "callers": {"bound": {"provider": "codex", "routes": ["fixture"]}}})
        self.start()
        forged = [("Authorization", "Bearer fixture-other-key"), ("Host", "localhost"),
                  ("X-Loopback", "true"), ("X-Caller-Key-Id", "bound"),
                  ("ChatGPT-Account-Id", "fixture-account")]
        self.assertEqual(self.metadata(headers=forged, host=host)[0], 403)
        self.assertEqual(self.metadata(headers=[("Host", "localhost"), ("X-Caller-Key-Id", "bound")], host=host)[0], 401)
        self.assertEqual(self.metadata(headers=[("Authorization", "Bearer fixture-other-key"), ("Host", "untrusted.invalid")], host=host)[0], 403)
        self.assertEqual(self.recorded("metadata"), [])
        self.assertEqual(self.metadata(headers=[("Authorization", "Bearer fixture-bound-key")], host=host)[0], 200)
        self.assertEqual(len(self.recorded("metadata")), 1)

    def test_groups_and_cross_provider_routes_never_borrow_member_capabilities(self):
        self.env["ZCALLD_PLUGIN_ROUTING_PLUGIN"] = str(self.routing_plugin)
        self.config.write_text(self.config.read_text() +
            "\n[provider other]\nbase_url=https://chatgpt.com/backend-api/codex\nprotocol=responses\n"
            f"plugin=builtin:codex\nplugin_account={self.account}\nplugin_npm=@ai-sdk/openai\n"
            "[route other-route]\nprovider=other\nmodel=fixture\n"
            "[route group/aggregate]\nprovider=codex\nmodel=fixture\n")
        self.write(self.root / "routing.json", {"groups": [
            {"name": "group/aggregate", "mode": "order", "members": [{"model": "fixture"}]}]})
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["group/aggregate"]}})
        self.start()
        self.assertEqual(self.metadata("/backend-api/codex/models?client_version=0.170.0")[0], 403)
        self.binding({"enabled": True, "loopback": {"provider": "codex", "routes": ["other-route"]}})
        self.assertEqual(self.metadata("/backend-api/codex/models?client_version=0.170.0")[0], 403)
        self.assertEqual(self.recorded("metadata"), [])

    def test_disabled_by_default_allocates_no_extra_host(self):
        registry = json.loads((self.ext / "registry.json").read_bytes())
        registry.pop("codex_http")
        self.write(self.ext / "registry.json", registry)
        self.start()
        self.assertNotEqual(self.metadata()[0], 200)
        self.assertEqual(self.metadata("/v1/models")[0], 200)
        self.assertEqual(self.recorded("host_started"), [])
        self.assertEqual(self.recorded("metadata"), [])

    def initializing_plugin(self, *, delay_ms=0, fail=False):
        module = self.root / "initialization-fixture.mjs"
        module.write_text(
            "import fs from 'node:fs';\n"
            + "const log=" + json.dumps(str(self.events)) + ";\n"
            + "const record=v=>fs.appendFileSync(log,JSON.stringify(v)+'\\n');\n"
            + "export default async()=>{record({kind:'init_started'});"
            + f"await new Promise(r=>setTimeout(r,{delay_ms}));"
            + ("throw new Error('PRIVATE-INIT-ERROR-CANARY');" if fail else "record({kind:'init_completed'});return {};")
            + "};\n", encoding="utf-8", newline="\n")
        registry = json.loads((self.ext / "registry.json").read_bytes())
        entry = {
            "name": "initialization-fixture", "source": "initialization-fixture",
            "path": str(module), "kind": "community", "enabled": True,
        }
        if fail:
            # Also require this middleware on generation, so a shared failed
            # init cannot be treated as an absent/no-op gateway hook.
            entry["gateway"] = str(module)
        registry["plugins"].append(entry)
        self.write(self.ext / "registry.json", registry)

    def test_cold_initialization_is_in_deadline_and_does_not_cancel_shared_ready(self):
        self.configure(total=250)
        self.initializing_plugin(delay_ms=1000)
        self.start()
        status, _, body = self.metadata()
        self.assertIn(status, [502, 504], body.decode())
        self.assertEqual(self.recorded("init_completed"), [])
        self.assertEqual(self.recorded("metadata"), [])
        deadline = time.monotonic() + 4
        while not self.recorded("init_completed") and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertEqual(len(self.recorded("init_completed")), 1)
        self.assertEqual(self.metadata()[0], 200)
        self.assertEqual(len(self.recorded("host_started")), 1)
        self.assertEqual(len(self.recorded("metadata")), 1)

    def test_failed_init_stays_failed_for_other_workers_and_generation(self):
        self.initializing_plugin(fail=True)
        self.config.write_text(self.config.read_text().replace("workers=1", "workers=2"))
        self.start()
        import concurrent.futures
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            replies = list(pool.map(lambda _: self.metadata(), range(2)))
        for status, _, body in replies:
            self.assertEqual(status, 502, body.decode())
            self.assertNotIn(b"PRIVATE-INIT-ERROR-CANARY", body)
        self.assertEqual(self.metadata()[0], 502)
        status, _, body = self.request(False)
        self.assertEqual(status, 503, body.decode())
        self.assertNotIn(b"PRIVATE-INIT-ERROR-CANARY", body)
        self.assertEqual(len(self.recorded("host_started")), 1)
        self.assertEqual(self.recorded("metadata"), [])

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("binary", "community-plugin", "pluginctl", "redact-plugin", "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + arg, required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    for key, value in vars(args).items():
        if not value:
            parser.error(f"missing {key}")
        setattr(CodexMetadataTests, key, Path(value).resolve())
    # Do not rerun unrelated inherited provider cases from the shared fixture.
    names = sorted(n for n in CodexMetadataTests.__dict__ if n.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.TestSuite(CodexMetadataTests(name) for name in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
