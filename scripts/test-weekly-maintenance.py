#!/usr/bin/env python3
"""Run native recovery -> one-shot Go worker -> bundled Bun with only fake I/O.

The preload is proven active before a gateway starts. It blocks every real
network primitive and permits only in-process responses at fixed Codex paths.
No native credentials, running services or user configuration are read.
"""
import argparse
import base64
import contextlib
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import unittest

PRELOAD = r"""
import fs from 'node:fs';
import net from 'node:net';
import tls from 'node:tls';
const root=ROOT;
const log=root+'/events.jsonl', options=root+'/options.json', reset=root+'/reset.json';
const record=v=>fs.appendFileSync(log,JSON.stringify({pid:process.pid,...v})+'\n',{mode:0o600});
const blocked=()=>{record({kind:'forbidden_network'});throw new Error('fixture forbids network')};
globalThis.WebSocket=class{constructor(){blocked()}};
net.Socket.prototype.connect=blocked;net.connect=net.createConnection=tls.connect=blocked;
Bun.connect=Bun.listen=Bun.serve=blocked;
globalThis.__zcalldWeeklyFixture=true;
const initial=JSON.parse(fs.readFileSync(options,'utf8'));
Date.now=()=>initial.now_ms;
record({kind:'host_started'});
const response=v=>new Response(JSON.stringify(v),{headers:{'content-type':'application/json'}});
globalThis.fetch=async(input,init={})=>{
  const url=new URL(String(input)), headers=new Headers(init.headers);
  if(url.origin!=='https://chatgpt.com'||url.search&&
      !(url.pathname==='/backend-api/codex/models'&&url.search==='?client_version=0.170.0'))blocked();
  if(headers.get('chatgpt-account-id')!=='fixture-account')throw new Error('account mismatch');
  const opts=JSON.parse(fs.readFileSync(options,'utf8'));
  const done=fs.existsSync(reset);
  const get=()=>{if(init.method!=='GET'||init.redirect!=='error'||init.body!==undefined)blocked()};
  if(url.pathname==='/backend-api/wham/usage'){
    get();record({kind:'usage',after_reset:done});
    if(opts.delay_usage){
      await new Promise((_,reject)=>{
        const abort=()=>{record({kind:'aborted',operation:'usage'});reject(new Error('fixture cancelled'))};
        if(init.signal?.aborted)abort();else init.signal?.addEventListener('abort',abort,{once:true});
      });
    }
    const first=root+'/first-usage-pid';
    try{fs.writeFileSync(first,String(process.pid),{flag:'wx',mode:0o600})}catch(error){if(error.code!=='EEXIST')throw error}
    if(done&&opts.unknown_after_reset&&fs.readFileSync(first,'utf8')===String(process.pid))return response({});
    const full=!done&&opts.full;
    return response({rate_limit:{allowed:!full,limit_reached:!!full,
      secondary_window:{used_percent:full?100:0,limit_window_seconds:604800,reset_at:opts.reset_at}},
      credits:{has_credits:true,unlimited:false,balance:'10'},
      rate_limit_reset_credits:{available_count:opts.no_card?0:1}});
  }
  if(url.pathname==='/backend-api/codex/models'){
    get();record({kind:'models'});
    return response({models:[{slug:'fixture-mini',display_name:'Fixture',
      base_instructions:'Synthetic instructions',supported_reasoning_levels:[{effort:'low',description:'Fixture'}]}]});
  }
  if(url.pathname==='/backend-api/wham/rate-limit-reset-credits'){
    get();record({kind:'cards'});
    return response({available_count:opts.no_card?0:1,credits:opts.no_card?[]:
      [{id:'synthetic-card',status:'available',expires_at:null}]});
  }
  if(url.pathname==='/backend-api/wham/rate-limit-reset-credits/consume'){
    if(init.method!=='POST'||init.redirect!=='error')blocked();
    const body=JSON.parse(init.body);
    if(body.credit_id!=='synthetic-card'||!/^[0-9a-f-]{36}$/u.test(body.redeem_request_id??''))throw new Error('invalid persisted reset intent');
    const persisted=fs.readFileSync(root+'/community/maintenance/state.json','utf8');
    if(!persisted.includes(body.redeem_request_id))throw new Error('reset intent was not persisted before POST');
    record({kind:'consume',redeem_request_id:body.redeem_request_id,intent_was_persisted:true});
    fs.writeFileSync(reset,JSON.stringify({reset:true}),{mode:0o600});
    return response({code:'reset',windows_reset:['secondary']});
  }
  if(url.pathname==='/backend-api/codex/responses'){
    if(init.method!=='POST')blocked();
    if(Object.hasOwn(init,'zcalldAccountPolicy')||Object.hasOwn(init,'zcalld_fresh_quota_account'))throw new Error('private intent reached wire');
    const body=JSON.parse(init.body);
    if(body.stream!==true)throw new Error('stream contract');
    record({kind:'generation',after_reset:done,model:body.model});
    const final={id:'fixture-response',object:'response',status:'completed',model:'fixture-mini',
      output:[{type:'message',role:'assistant',content:[{type:'output_text',text:'OK'}]}],
      usage:{input_tokens:3,output_tokens:1}};
    return new Response('event: response.completed\ndata: '+JSON.stringify({type:'response.completed',response:final})+'\n\n',
      {headers:{'content-type':'text/event-stream'}});
  }
  blocked();
};
"""


class WeeklyMaintenanceTests(unittest.TestCase):
    binary = community_plugin = pluginctl = bun = temp_root = None

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zweekly-", dir=self.temp_root)
        self.root = Path(self.temp.name).resolve()
        self.ext = self.root / "community"
        self.ext.mkdir(mode=0o700)
        self.process = None
        self.log = tempfile.TemporaryFile()
        self.env = {key: os.environ[key] for key in
                    ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT",
                     "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH") if key in os.environ}
        self.env["PATH"] = str(self.bun.parent) + os.pathsep + self.env.get("PATH", "")
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "CODEX_HOME",
                    "TMP", "TEMP", "TMPDIR"):
            path = self.root / ("private-" + key.lower())
            path.mkdir(mode=0o700, exist_ok=True)
            self.env[key] = str(path)
        empty = self.root / "empty-source"
        empty.mkdir(mode=0o700)
        self.write(empty / "logins.json", [])
        self.assertEqual(self.helper("import-builtin", "--source", str(empty))["network_requests"], 0)
        runtime = self.helper("runtime")
        self.account = "builtin:codex#0123456789abcdefabcd"
        bundle, = (self.ext / "runtime/builtin").iterdir()
        entry = bundle / "codex.mjs"
        now = int(time.time())
        claims = {"exp": now + 3600, "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account"}}
        access = "fixture." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + ".fixture"
        self.external = self.root / "native-auth.json"
        self.write(self.external, {"tokens": {"access_token": access, "refresh_token": "fixture-refresh",
                                            "account_id": "fixture-account"}})
        self.write(self.ext / "plugin-auth.json", {self.account: {
            "type": "oauth", "access": access, "refresh": "fixture-refresh", "accountId": "fixture-account",
            "expires": (now + 3600) * 1000, "refreshOwner": "external", "sourceAuthFile": str(self.external)}})
        self.registry = {
            "version": 1, "plugins": [{"name": "builtin-codex", "source": "builtin-codex",
                "kind": "community", "enabled": True, "path": str(entry),
                "sha256": hashlib.sha256(entry.read_bytes()).hexdigest()}],
            "accounts": {self.account: {"label": "Fixture", "enabled": True}},
            "account_policies": {self.account: {"codex_no_credits": False}},
            "config": {"provider": {"builtin:codex": {"npm": "@ai-sdk/openai",
                "models": {"fixture-mini": {"name": "Fixture"}}}}}}
        self.write(self.ext / "registry.json", self.registry)
        maintenance = self.ext / "maintenance"
        maintenance.mkdir(mode=0o700)
        self.state = {"version": 1, "policy": {"enabled": False,
            "enabled_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 3600)),
            "accounts": {self.account: {"weekly_warmup": False, "weekly_reset": True,
                "expiring_reset": False, "reset_lead_minutes": 30}}},
            "ledger": {self.account: {"windows": {}, "completed_cards": {}}}}
        self.write(maintenance / "state.json", self.state)
        self.options = {"now_ms": now * 1000, "reset_at": now + 86400, "full": False}
        self.write(self.root / "options.json", self.options)
        self.preload = self.root / "preload.mjs"
        self.preload.write_text(PRELOAD.replace("ROOT", json.dumps(str(self.root))), encoding="utf-8")
        self.env["BUN_OPTIONS"] = "--preload=" + str(self.preload)
        probe = subprocess.run([str(self.bun), "-e",
            "console.log(globalThis.__zcalldWeeklyFixture===true?'PRELOAD_OK':'PRELOAD_MISSING')"],
            env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(probe.returncode, 0, probe.stderr)
        self.assertEqual(probe.stdout.strip(), "PRELOAD_OK", "do not start a host without the network guard")
        (self.root / "events.jsonl").unlink()
        self.binding = self.root / "host.json"
        self.write(self.binding, {"bun": str(self.bun), "host": runtime["host"], "pi": runtime["pi"],
            "pluginctl": str(self.pluginctl), "registry": str(self.ext / "registry.json"),
            "auth_path": str(self.ext / "plugin-auth.json")})
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.config = self.root / "config.conf"
        self.config.write_text(
            f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers=1\nqueue=4\n"
            "request_timeout_ms=5000\nconnect_timeout_ms=100\nupstream_timeout_ms=60000\n"
            "max_response_bytes=1048576\ntrust_loopback=true\n"
            "[provider codex]\nbase_url=https://chatgpt.com/backend-api/codex\nprotocol=responses\n"
            f"plugin=builtin:codex\nplugin_account={self.account}\nplugin_npm=@ai-sdk/openai\n"
            "[route fixture]\nprovider=codex\nmodel=fixture-mini\n"
            f"[plugin community]\npath={self.community_plugin}\nconfig_file={self.binding}\n",
            encoding="utf-8")
        self.original_auth = (self.ext / "plugin-auth.json").read_bytes()
        self.original_external = self.external.read_bytes()

    @staticmethod
    def write(path, data):
        path.write_text(json.dumps(data), encoding="utf-8")
        if os.name != "nt":
            path.chmod(0o600)

    def helper(self, *args):
        result = subprocess.run([str(self.pluginctl), "--root", str(self.ext), *args],
            env=self.env, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def setup_policy(self, *, enabled=False, restricted=False, **options):
        self.state["policy"]["enabled"] = enabled
        self.registry["account_policies"][self.account]["codex_no_credits"] = restricted
        self.options.update(options)
        self.write(self.ext / "maintenance/state.json", self.state)
        self.write(self.ext / "registry.json", self.registry)
        self.write(self.root / "options.json", self.options)

    def events(self, kind=None):
        path = self.root / "events.jsonl"
        rows = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        return [row for row in rows if kind is None or row["kind"] == kind]

    def start(self):
        flags = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {"start_new_session": True}
        self.process = subprocess.Popen([str(self.binary), "serve", "--config", str(self.config)],
            env=self.env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log, **flags)
        until = time.monotonic() + 8
        while time.monotonic() < until:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode(errors="replace"))
            try:
                with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=.5)) as conn:
                    conn.request("GET", "/healthz")
                    response = conn.getresponse()
                    response.read()
                    if response.status == 200:
                        return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(.02)
        self.fail("gateway did not become ready")

    def request(self):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=15)) as conn:
            conn.request("POST", "/v1/responses",
                body=json.dumps({"model": "fixture", "input": "Reply exactly OK", "stream": False}),
                headers={"Content-Type": "application/json"})
            response = conn.getresponse()
            return response.status, response.read()

    def assert_completed(self, body):
        response = json.loads(body)
        self.assertEqual(response["status"], "completed")
        self.assertEqual(response["output"][0]["content"][0]["text"], "OK")

    def assert_reset_ledger(self):
        consumes = self.events("consume")
        self.assertEqual(len(consumes), 1)
        state = json.loads((self.ext / "maintenance/state.json").read_text())
        self.assertTrue(consumes[0]["intent_was_persisted"])
        self.assertIn(self.account, state["ledger"])

    def tearDown(self):
        try:
            if self.process is not None and self.process.poll() is None:
                self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
                try:
                    self.process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    if os.name != "nt":
                        os.killpg(self.process.pid, signal.SIGKILL)
                    else:
                        self.process.kill()
                    self.process.wait(timeout=3)
                    self.fail("owned gateway and maintenance children did not drain")
            self.assertEqual(self.events("forbidden_network"), [])
            self.assertEqual((self.ext / "plugin-auth.json").read_bytes(), self.original_auth)
            self.assertEqual(self.external.read_bytes(), self.original_external)
        finally:
            self.log.close()
            self.temp.cleanup()

    def test_disabled_weekly_uses_only_the_generation_host(self):
        self.start()
        status, body = self.request()
        self.assertEqual(status, 200, body)
        self.assert_completed(body)
        self.assertEqual(len(self.events("host_started")), 1)
        self.assertEqual(len(self.events("generation")), 1)
        self.assertEqual(self.events("usage") + self.events("consume"), [])

    def test_before_credits_resets_once_before_generation(self):
        self.setup_policy(enabled=True, full=True)
        self.start()
        status, body = self.request()
        self.assertEqual(status, 200, body)
        self.assert_completed(body)
        self.assert_reset_ledger()
        self.assertEqual(len(self.events("generation")), 1)
        self.assertTrue(self.events("generation")[0]["after_reset"])

    def test_exhausted_no_credits_forces_fresh_usage_in_the_existing_host(self):
        self.setup_policy(enabled=True, restricted=True, full=True)
        self.start()
        status, body = self.request()
        self.assertEqual(status, 200, body)
        self.assert_completed(body)
        self.assert_reset_ledger()
        generation, = self.events("generation")
        same_host = [row for row in self.events("usage") if row["pid"] == generation["pid"]]
        self.assertEqual([row["after_reset"] for row in same_host], [False, True])
        self.assertTrue(generation["after_reset"])

    def test_reset_does_not_bypass_unknown_fresh_no_credits_usage(self):
        self.setup_policy(enabled=True, restricted=True, full=True, unknown_after_reset=True)
        self.start()
        status, body = self.request()
        self.assertEqual(status, 503, body)
        self.assert_reset_ledger()
        self.assertEqual(self.events("generation"), [])

    def test_no_card_never_reports_recovery_or_sends_generation(self):
        self.setup_policy(enabled=True, restricted=True, full=True, no_card=True)
        self.start()
        status, body = self.request()
        self.assertEqual(status, 429, body)
        self.assertEqual(self.events("generation") + self.events("consume"), [])

    def test_client_disconnect_cancels_the_one_shot_worker_before_any_post(self):
        self.setup_policy(enabled=True, full=True, delay_usage=True)
        self.start()
        body = json.dumps({"model": "fixture", "input": "Reply exactly OK", "stream": False}).encode()
        with socket.create_connection(("127.0.0.1", self.port), timeout=5) as sock:
            sock.sendall(f"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body)
            until = time.monotonic() + 5
            while not self.events("usage") and time.monotonic() < until:
                time.sleep(.02)
            self.assertTrue(self.events("usage"), "worker never reached the fake usage GET")
            # FIN is a valid write-half-close; reset the connection to cancel it.
            linger = struct.pack("HH" if os.name == "nt" else "ii", 1, 0)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
        until = time.monotonic() + 5
        while not self.events("aborted") and time.monotonic() < until:
            time.sleep(.02)
        self.assertTrue(self.events("aborted"), "disconnect did not reach the Bun AbortSignal")
        self.assertEqual(self.events("generation") + self.events("consume"), [])


def main():
    parser = argparse.ArgumentParser()
    for name in ("binary", "community-plugin", "pluginctl", "bun"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--temp-root", type=Path)
    args, rest = parser.parse_known_args()
    for name in ("binary", "community_plugin", "pluginctl", "bun"):
        value = getattr(args, name).resolve()
        if not value.is_file():
            parser.error(f"missing {name}")
        setattr(WeeklyMaintenanceTests, name, value)
    WeeklyMaintenanceTests.temp_root = str(args.temp_root.resolve()) if args.temp_root else None
    unittest.main(argv=[__file__, *rest])


if __name__ == "__main__":
    main()
