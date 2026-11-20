#!/usr/bin/env python3
"""Actual Responses compaction controls through decompression, privacy and routing."""
import argparse
import contextlib
import gzip
import http.client
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import unittest

spec = importlib.util.spec_from_file_location("builtin_fetch_fixture", Path(__file__).with_name("test-builtin-fetch.py"))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)

OPAQUE_IN = "opaque-input jane.doe@acme.io 15551234567 sk-proj-Wn4rT7yQ2mLx9VbK3cZp /+=="
OPAQUE_OUT = "opaque-output jane.doe@acme.io 15551234567 sk-proj-Wn4rT7yQ2mLx9VbK3cZp /+=="
PLAIN = "Use the fixture key sk-proj-Wn4rT7yQ2mLx9VbK3cZp for this local test."
GUARD = r"""
import fs from 'node:fs'; import net from 'node:net'; import tls from 'node:tls';
const logPath=LOG_FILE;
const record=value=>fs.appendFileSync(logPath,JSON.stringify(value)+'\n',{mode:0o600});
const blocked=()=>{record({kind:'forbidden_network'});throw new Error('fixture forbids network')};
globalThis.WebSocket=class{constructor(){blocked()}};
net.Socket.prototype.connect=blocked;net.connect=net.createConnection=tls.connect=blocked;
Bun.connect=Bun.listen=Bun.serve=blocked;
globalThis.fetch=async(url,init)=>{
  if(String(url)!=='https://chatgpt.com/backend-api/codex/responses'||init?.method!=='POST')blocked();
  const body=JSON.parse(init.body);
  record({kind:'generation',body,content_encoding:init.headers.get('content-encoding')});
  const text=body.input.find(item=>item.role==='user').content[0].text;
  const compact={type:'compaction',id:'cmp_output',encrypted_content:OPAQUE_OUTPUT,
    internal_chat_message_metadata_passthrough:{fixture:true}};
  const final={id:'response-fixture',object:'response',status:'completed',model:'fixture',
    output:[{type:'message',role:'assistant',content:[{type:'output_text',text}]},compact],
    usage:{input_tokens:7,output_tokens:2}};
  const events=[
    {type:'response.created',response:{id:final.id}},
    {type:'response.output_text.delta',delta:text},
    {type:'response.output_item.added',output_index:1,item:compact},
    {type:'response.output_item.done',output_index:1,item:compact},
    {type:'response.completed',response:final},
  ];
  const bytes=new TextEncoder().encode(events.map(value=>'event: '+value.type+'\r\ndata: '+JSON.stringify(value)+'\r\n\r\n').join(''));
  let offset=0;
  return new Response(new ReadableStream({
    pull(c){if(offset===bytes.length){c.close();return;}
      const end=Math.min(bytes.length,offset+7);c.enqueue(bytes.slice(offset,end));offset=end},
    cancel(){record({kind:'body_cancelled'})}
  },{highWaterMark:0}),{headers:{'content-type':'text/event-stream'}});
};
await import(HOST_URL);
"""


class CodexCompactionTests(base.BuiltinFetchTests):
    routing_plugin = None

    def setUp(self):
        super().setUp()
        self.env["ZCALLD_PLUGIN_ROUTING_PLUGIN"] = str(self.routing_plugin)
        config = json.loads(self.host_config.read_bytes())
        generated = self.helper("runtime")
        Path(config["host"]).write_text(
            GUARD.replace("LOG_FILE", json.dumps(str(self.events)))
            .replace("OPAQUE_OUTPUT", json.dumps(OPAQUE_OUT))
            .replace("HOST_URL", json.dumps(Path(generated["host"]).as_uri())),
            encoding="utf-8", newline="\n")
        self.config.write_text(self.config.read_text() +
            "\n[route group/compact]\nprovider=codex\nmodel=fixture\n")
        self.write(self.root / "routing.json", {"groups": [
            {"name": "group/compact", "mode": "order", "members": [{"model": "fixture"}]}]})
        result = subprocess.run([str(self.binary), "privacy", "set", "secrets",
            "--config", str(self.config), "--json"], env=self.env,
            capture_output=True, text=True, encoding="utf-8", timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["configured"])

    def response(self, stream, encoding):
        fixture = json.loads((Path(__file__).parent / "fixtures" / "codex-compaction.json").read_bytes())
        row = fixture["stream" if stream else "json"]
        payload = row["body"].encode()
        if encoding == "gzip":
            payload = gzip.compress(payload)
        elif encoding == "zstd":
            payload = bytes.fromhex(row["zstd_hex"])
        headers = {"Content-Type": "application/json"}
        if encoding:
            headers["Content-Encoding"] = encoding
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)) as conn:
            conn.request("POST", "/backend-api/codex/responses", payload, headers)
            response = conn.getresponse()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), response.read()

    def assert_forwarded(self, row):
        body = row["body"]
        self.assertEqual(body["model"], "fixture", "group routing did not select its configured member")
        self.assertTrue(body["stream"])
        self.assertFalse(body["store"])
        self.assertIsNone(row["content_encoding"])
        message = next(item for item in body["input"] if item.get("role") == "user")
        self.assertNotIn("sk-proj-Wn4rT7yQ2mLx9VbK3cZp", message["content"][0]["text"])
        self.assertIn("{{", message["content"][0]["text"])
        compact = next(item for item in body["input"] if item["type"] == "compaction")
        self.assertEqual(compact, {"type": "compaction", "id": "cmp_input", "encrypted_content": OPAQUE_IN,
            "internal_chat_message_metadata_passthrough": {"fixture": True}})
        self.assertEqual(body["input"][-1], {"type": "compaction_trigger"})
        reasoning = next(item for item in body["input"] if item["type"] == "reasoning")
        self.assertEqual(reasoning["encrypted_content"], OPAQUE_IN)

    def assert_compaction(self, compact):
        self.assertEqual(compact, {"type": "compaction", "id": "cmp_output", "encrypted_content": OPAQUE_OUT,
            "internal_chat_message_metadata_passthrough": {"fixture": True}})

    def test_compaction_controls_and_ciphertext_survive_json_and_compressed_input(self):
        self.start()
        for encoding in ["", "gzip", "zstd"]:
            with self.subTest(encoding=encoding):
                status, headers, body = self.response(False, encoding)
                self.assertEqual(status, 200, body.decode(errors="replace"))
                self.assertEqual(headers["content-type"], "application/json")
                value = json.loads(body)
                self.assertEqual(value["output"][0]["content"][0]["text"], PLAIN)
                self.assert_compaction(value["output"][1])
                self.assert_forwarded(self.recorded("generation")[-1])
        self.assertEqual(len(self.recorded("generation")), 3)

    def test_compaction_output_items_and_sse_remain_opaque_with_privacy_enabled(self):
        self.start()
        status, headers, body = self.response(True, "zstd")
        self.assertEqual(status, 200, body.decode(errors="replace"))
        self.assertEqual(headers["content-type"], "text/event-stream")
        events = [json.loads(line[6:]) for line in body.decode().splitlines() if line.startswith("data: ")]
        self.assertEqual("".join(item.get("delta", "") for item in events), PLAIN)
        for item in events:
            if item["type"] in ["response.output_item.added", "response.output_item.done"]:
                self.assert_compaction(item["item"])
        self.assert_compaction(events[-1]["response"]["output"][1])
        self.assert_forwarded(self.recorded("generation")[0])
        self.assertEqual(len(self.recorded("generation")), 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "community-plugin", "pluginctl", "redact-plugin", "privacy-worker", "routing-plugin"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    for name, value in vars(args).items():
        if not value:
            parser.error("missing " + name)
        setattr(CodexCompactionTests, name, Path(value).resolve(strict=True))
    names = sorted(n for n in CodexCompactionTests.__dict__ if n.startswith("test_"))
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.TestSuite(CodexCompactionTests(name) for name in names))
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
