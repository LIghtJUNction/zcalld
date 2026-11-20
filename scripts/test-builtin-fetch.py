#!/usr/bin/env python3
"""Exercise generated Codex modules through native gateway/provider/privacy hooks."""
import argparse
import base64
import contextlib
import http.client
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import unittest

GUARD = r"""
import fs from 'node:fs'; import net from 'node:net'; import tls from 'node:tls';
const optionsPath=OPTIONS_FILE, logPath=LOG_FILE;
const record=value=>fs.appendFileSync(logPath,JSON.stringify(value)+'\n',{mode:0o600});
const blocked=()=>{record({kind:'forbidden_network'});throw new Error('fixture forbids network')};
globalThis.WebSocket=class{constructor(){blocked()}};
net.Socket.prototype.connect=blocked;net.connect=net.createConnection=tls.connect=blocked;
Bun.connect=Bun.listen=Bun.serve=blocked;
globalThis.fetch=async(url,init)=>{
  if(String(url)!=='https://chatgpt.com/backend-api/codex/responses'||init?.method!=='POST')blocked();
  const options=JSON.parse(fs.readFileSync(optionsPath,'utf8'));
  const body=JSON.parse(init.body);
  if(body.stream!==true||init.headers.get('chatgpt-account-id')!=='fixture-account')throw new Error('fixture adapter contract failed');
  const input=body.input[0]?.content?.[0]?.text??'';
  record({kind:'fetch',input,stream:body.stream});
  if(options.worker_exit)process.exit(31);
  if(options.rpc_error)throw new Error('private-fixture-error-canary');
  const abort=()=>record({kind:'aborted'});
  init.signal.addEventListener('abort',abort,{once:true});
  if(options.delay_ms){
    await new Promise((resolve,reject)=>{
      const timer=setTimeout(resolve,options.delay_ms);
      const abort=()=>{clearTimeout(timer);reject(new Error('fixture request aborted'))};
      if(init.signal.aborted)abort();else init.signal.addEventListener('abort',abort,{once:true});
    });
  }
  const text=options.echo?input:'OK你好';
  const final={id:'fixture-response',object:'response',status:'completed',model:'fixture',
    output:[{type:'message',role:'assistant',content:[{type:'output_text',text}]}],
    usage:{input_tokens:3,output_tokens:2},fixture_extension:{kept:true}};
  const events=[{type:'response.created',response:{id:final.id}},
    {type:'response.output_text.delta',delta:text},{type:'response.completed',response:final}];
  let wire=events.map(value=>'event: '+value.type+'\r\ndata: '+JSON.stringify(value)+'\r\n\r\n').join('');
  if(options.mode==='invalid')wire='data: {"type":"unrelated"}\n\n';
  if(options.mode==='large')wire=':'+'x'.repeat(70<<10)+'\n\n'+wire;
  const bytes=new TextEncoder().encode(wire);let at=0;
  const stream=new ReadableStream({
    pull(controller){
      if(at===bytes.length){controller.close();return;}
      const end=Math.min(bytes.length,at+(options.fragment??3));
      controller.enqueue(bytes.slice(at,end));at=end;
    },
    cancel(){record({kind:'body_cancelled'});}
  },{highWaterMark:0});
  const headers={'cache-control':'no-cache','x-request-id':'fixture'};
  if(options.content_type)headers['content-type']=options.content_type;
  if(options.header_bytes)headers['x-fixture-large']='x'.repeat(options.header_bytes);
  return new Response(stream,{headers});
};
await import(HOST_URL);
"""


class BuiltinFetchTests(unittest.TestCase):
    binary = community_plugin = pluginctl = bun = redact_plugin = privacy_worker = None

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="zbf-")
        self.root = Path(self.tmp.name).resolve()
        self.ext = self.root / "community"
        self.ext.mkdir(mode=0o700)
        self.process = None
        self.log = tempfile.TemporaryFile()
        self.env = {key: os.environ[key] for key in (
            "PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT",
            "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH") if key in os.environ}
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "CODEX_HOME",
                    "TMP", "TEMP", "TMPDIR"):
            directory = self.root / ("private-" + key.lower())
            directory.mkdir(mode=0o700, exist_ok=True)
            self.env[key] = str(directory)
        self.env["ZCALLD_REDACT_PLUGIN"] = str(self.redact_plugin)
        self.env["ZCALLD_PRIVACY_WORKER"] = str(self.privacy_worker)
        source = self.root / "empty-magpie"
        source.mkdir(mode=0o700)
        self.write(source / "logins.json", [])
        imported = self.helper("import-builtin", "--source", str(source))
        self.assertEqual(imported["builtin_accounts"], 0)
        self.assertEqual(imported["network_requests"], 0)
        runtime = self.helper("runtime")
        bundles = list((self.ext / "runtime" / "builtin").iterdir())
        self.assertEqual(len(bundles), 1)
        bundle = bundles[0]
        self.assertTrue((bundle / "codex-impl.mjs").is_file())

        claims = {"exp": int(time.time()) + 3600,
                  "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account"}}
        payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        access = "fixture." + payload + ".fixture"
        external = self.root / "external-auth.json"
        self.write(external, {"tokens": {"access_token": access, "account_id": "fixture-account",
                                        "refresh_token": "fixture-refresh"}})
        self.account = "builtin:codex#fixture"
        self.auth = {self.account: {"type": "oauth", "access": access, "refresh": "fixture-refresh",
                     "accountId": "fixture-account", "expires": int(time.time() * 1000) + 3600000,
                     "refreshOwner": "external", "sourceAuthFile": str(external)}}
        self.write(self.ext / "plugin-auth.json", self.auth)
        registry = {
            "version": 1,
            "plugins": [{"name": "builtin-codex", "kind": "community", "enabled": True,
                         "source": "builtin-codex", "path": str(bundle / "codex.mjs")}],
            "config": {"provider": {"builtin:codex": {
                "npm": "@ai-sdk/openai", "models": {"fixture": {"name": "Fixture"}}}}},
            "accounts": {self.account: {"provider": "builtin:codex", "enabled": True}},
        }
        self.write(self.ext / "registry.json", registry)
        self.options = self.root / "mock-options.json"
        self.events = self.root / "mock-events.jsonl"
        self.mock()
        guard = self.root / "guard.mjs"
        guard.write_text(GUARD.replace("OPTIONS_FILE", json.dumps(str(self.options)))
                         .replace("LOG_FILE", json.dumps(str(self.events)))
                         .replace("HOST_URL", json.dumps(Path(runtime["host"]).as_uri())),
                         encoding="utf-8", newline="\n")
        host_config = self.root / "host.json"
        self.write(host_config, {"bun": str(self.bun), "host": str(guard), "pi": runtime["pi"],
                                "registry": str(self.ext / "registry.json"),
                                "auth_path": str(self.ext / "plugin-auth.json")})
        self.host_config = host_config
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.config = self.root / "config.conf"
        self.configure()

    @staticmethod
    def write(path, value):
        path.write_text(json.dumps(value), encoding="utf-8", newline="\n")
        if os.name != "nt":
            path.chmod(0o600)

    def helper(self, *args):
        result = subprocess.run([str(self.pluginctl), "--root", str(self.ext), *args],
                                env=self.env, capture_output=True, text=True,
                                encoding="utf-8", timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def mock(self, **options):
        self.write(self.options, options)

    def configure(self, connect=100, total=2000):
        self.config.write_text(
            f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers=1\nqueue=4\n"
            f"request_timeout_ms=5000\nconnect_timeout_ms={connect}\nupstream_timeout_ms={total}\n"
            "max_response_bytes=1048576\ntrust_loopback=true\n"
            "[provider codex]\nbase_url=https://chatgpt.com/backend-api/codex\nprotocol=responses\n"
            f"plugin=builtin:codex\nplugin_account={self.account}\nplugin_npm=@ai-sdk/openai\n"
            "[route fixture]\nprovider=codex\nmodel=fixture\n"
            f"[plugin community]\npath={self.community_plugin}\nconfig_file={self.host_config}\n",
            encoding="utf-8", newline="\n")

    def request(self, stream, text="Reply exactly OK"):
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)) as conn:
            conn.request("POST", "/v1/responses",
                         body=json.dumps({"model": "fixture", "input": text, "stream": stream}),
                         headers={"Content-Type": "application/json"})
            response = conn.getresponse()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), response.read()

    def start(self):
        flags = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}
        self.process = subprocess.Popen(
            [str(self.binary), "serve", "--config", str(self.config)],
            env=self.env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log, **flags)
        end = time.monotonic() + 8
        while time.monotonic() < end:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail("fixture gateway exited: " + self.log.read().decode(errors="replace"))
            try:
                with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=1)) as conn:
                    conn.request("GET", "/healthz")
                    response = conn.getresponse()
                    response.read()
                    if response.status == 200:
                        return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.02)
        self.fail("fixture gateway did not become ready")

    def recorded(self, kind):
        if not self.events.exists():
            return []
        return [value for line in self.events.read_text(encoding="utf-8").splitlines()
                if (value := json.loads(line)).get("kind") == kind]

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
                self.fail("fixture gateway failed to drain")

    def tearDown(self):
        try:
            self.stop()
            self.assertFalse(self.recorded("forbidden_network"), "fixture attempted an unmocked network request")
            self.assertEqual(json.loads((self.ext / "plugin-auth.json").read_bytes()), self.auth)
        finally:
            self.log.close()
            self.tmp.cleanup()

    def assert_sse(self, status, headers, body, text="OK你好"):
        self.assertEqual(status, 200, body.decode(errors="replace"))
        self.assertEqual(headers["content-type"], "text/event-stream")
        events = [json.loads(line[6:]) for line in body.decode().splitlines() if line.startswith("data: ")]
        self.assertEqual(events[-1]["type"], "response.completed")
        self.assertEqual("".join(value.get("delta", "") for value in events), text)
        self.assertEqual(events[-1]["response"]["output"][0]["content"][0]["text"], text)

    def test_missing_type_json_then_sse_through_generated_builtin(self):
        self.start()
        status, headers, body = self.request(False)
        self.assertEqual(status, 200, body.decode(errors="replace"))
        self.assertEqual(headers["content-type"], "application/json")
        value = json.loads(body)
        self.assertEqual(value["output"][0]["content"][0]["text"], "OK你好")
        self.assertEqual(value["usage"], {"input_tokens": 3, "output_tokens": 2})
        self.assertEqual(value["fixture_extension"], {"kept": True})
        self.assert_sse(*self.request(True))
        self.assertEqual(len(self.recorded("fetch")), 2)

    def test_invalid_or_explicit_wrong_type_cancels_and_does_not_poison_next_request(self):
        self.mock(mode="invalid")
        self.start()
        status, _, body = self.request(False)
        self.assertEqual(status, 502)
        self.assertEqual(json.loads(body)['error']['message'], 'provider request failed')
        self.mock()
        self.assert_sse(*self.request(True))
        self.mock(content_type="application/json")
        self.assertEqual(self.request(False)[0], 502)
        self.mock()
        self.assert_sse(*self.request(True))
        self.assertEqual(len(self.recorded("fetch")), 4)
        self.assertGreaterEqual(len(self.recorded("body_cancelled")), 2)

    def test_probe_limit_rejects_without_replay(self):
        self.mock(mode="large", fragment=127)
        self.start()
        self.assertEqual(self.request(True)[0], 502)
        self.assertEqual(len(self.recorded("fetch")), 1)
        self.assertEqual(len(self.recorded("body_cancelled")), 1)

    def test_response_head_can_arrive_after_connection_timeout(self):
        self.mock(delay_ms=200)
        self.start()
        self.assert_sse(*self.request(True))
        self.assertEqual(len(self.recorded("fetch")), 1)

    def test_total_deadline_cancels_pending_fetch_without_replay(self):
        self.configure(connect=100, total=250)
        self.mock(delay_ms=1000)
        self.start()
        status, _, body = self.request(True)
        self.assertEqual(status, 502)
        self.assertIn('provider request timed out', json.loads(body)['error']['message'])
        end = time.monotonic() + 1
        while not self.recorded("aborted") and time.monotonic() < end:
            time.sleep(0.01)
        self.assertEqual(len(self.recorded("fetch")), 1)
        self.assertEqual(len(self.recorded("aborted")), 1)

    def test_rpc_error_is_classified_without_forwarding_plugin_details(self):
        self.mock(rpc_error=True)
        self.start()
        status, _, body = self.request(True)
        self.assertEqual(status, 502)
        self.assertEqual(json.loads(body)["error"]["message"], "provider request failed")
        self.assertNotIn(b"private-fixture-error-canary", body)
        self.assertEqual(len(self.recorded("fetch")), 1)

    def test_oversized_response_head_has_a_distinct_static_error(self):
        self.mock(header_bytes=20000)
        self.start()
        status, _, body = self.request(True)
        self.assertEqual(status, 502)
        self.assertEqual(json.loads(body)["error"]["message"], "invalid provider response metadata")
        self.assertEqual(len(self.recorded("fetch")), 1)

    def test_worker_exit_has_a_distinct_static_error(self):
        self.mock(worker_exit=True)
        self.start()
        status, _, body = self.request(True)
        self.assertEqual(status, 502)
        self.assertEqual(json.loads(body)["error"]["message"], "provider worker or transport ended")
        self.assertEqual(len(self.recorded("fetch")), 1)

    def test_privacy_and_missing_type_preserve_json_sse_and_restore_the_fixture_secret(self):
        result = subprocess.run(
            [str(self.binary), "privacy", "set", "secrets", "--config", str(self.config), "--json"],
            env=self.env, capture_output=True, text=True, encoding="utf-8", timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["configured"])
        self.mock(echo=True)
        self.start()
        text = "Use this fixture key sk-proj-Wn4rT7yQ2mLx9VbK3cZp"
        status, headers, body = self.request(False, text)
        self.assertEqual(status, 200, body.decode(errors="replace"))
        self.assertEqual(headers["content-type"], "application/json")
        self.assertEqual(json.loads(body)["output"][0]["content"][0]["text"], text)
        self.assert_sse(*self.request(True, text), text=text)
        sent = self.recorded("fetch")
        self.assertEqual(len(sent), 2)
        for value in sent:
            self.assertNotIn("sk-proj-Wn4rT7yQ2mLx9VbK3cZp", value["input"])
            self.assertIn("{{", value["input"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=os.environ.get("ZCALLD_TEST_BINARY", "target/debug/zcalld"))
    parser.add_argument("--community-plugin", required=True)
    parser.add_argument("--pluginctl", required=True)
    parser.add_argument("--redact-plugin", required=True)
    parser.add_argument("--privacy-worker", required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    if not args.bun:
        parser.error("Bun is required")
    for name in ("binary", "community_plugin", "pluginctl", "redact_plugin", "privacy_worker", "bun"):
        setattr(BuiltinFetchTests, name, Path(getattr(args, name)).resolve(strict=True))
    unittest.main(argv=[__file__], verbosity=2)
