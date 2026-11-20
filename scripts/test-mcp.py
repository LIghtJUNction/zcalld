#!/usr/bin/env python3
"""Real native HTTP + optional community MCP tests; local fixture tools only."""
import argparse
import concurrent.futures
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


MOCK = r'''
import { appendFileSync } from "node:fs"
import { join } from "node:path"
export default async ({directory}) => {
  const record=(file,text)=>appendFileSync(join(directory,file),text+"\n")
  record("loads.log",String(process.pid))
  return {tool:{
    echo:{description:"Echo fixture input",args:{type:"object",properties:{text:{type:"string"}}},execute:async(args)=>{record("calls.log","echo");return args.text}},
    fail:{description:"Throw fixture failure",execute:async()=>{record("calls.log","fail");throw new Error("fixture failure")}},
    large:{description:"Bounded fixture output",execute:async()=>{record("calls.log","large");return "x".repeat(200000)}},
    wait:{description:"Abortable fixture wait",execute:async(args,ctx)=>{
      record("started.log","wait")
      return await new Promise((resolve,reject)=>{
        const timer=setTimeout(()=>resolve("finished"),3000)
        const abort=()=>{clearTimeout(timer);record("aborted.log","wait");reject(new Error("fixture aborted"))}
        if(ctx.abort.aborted)abort();else ctx.abort.addEventListener("abort",abort,{once:true})
      })
    }}
  }}
}
'''


class MCPTests(unittest.TestCase):
    binary = community_plugin = pluginctl = bun = None

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="zcalld-mcp-")
        self.root = Path(self.tmp.name).resolve()
        self.ext = self.root / "community"
        self.ext.mkdir(mode=0o700)
        self.processes = []
        self.logs = []
        self.source = self.root / "tools-fixture.mjs"
        self.source.write_text(MOCK, encoding="utf-8", newline="\n")
        self.helper("install", str(self.source), "--name", "fixture")
        self.helper("enable", "fixture")
        runtime = self.helper("runtime")
        config = {"bun": str(self.bun), "host": runtime["host"], "pi": runtime["pi"],
                  "registry": str(self.ext / "registry.json"), "auth_path": str(self.ext / "plugin-auth.json")}
        self.host_config = self.root / "community-host.json"
        self.host_config.write_text(json.dumps(config), encoding="utf-8", newline="\n")
        (self.ext / "plugin-auth.json").write_text("{}", encoding="utf-8", newline="\n")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self.config = self.root / "zcalld.conf"
        self.write_config()

    def helper(self, *args):
        run = subprocess.run([str(self.pluginctl), "--root", str(self.ext), *args],
                             capture_output=True, text=True, timeout=20, encoding="utf-8")
        self.assertEqual(run.returncode, 0, run.stderr)
        return json.loads(run.stdout)

    def write_config(self, maximum=1048576, caller_keys=False, trust=True):
        caller = "caller_keys_file=caller-keys.json\n" if caller_keys else ""
        self.config.write_text(
            f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers=4\nqueue=16\n"
            f"max_body_bytes=4194304\nmax_response_bytes={maximum}\nrequest_timeout_ms=5000\n"
            f"upstream_timeout_ms=10000\ntrust_loopback={'true' if trust else 'false'}\n{caller}"
            "[provider mock]\nbase_url=http://127.0.0.1:9\nprotocol=chat\n"
            "[route mock]\nprovider=mock\nmodel=mock\n"
            f"[plugin tools]\npath={self.community_plugin}\nconfig_file={self.host_config}\n",
            encoding="utf-8", newline="\n")

    def start(self):
        log = tempfile.TemporaryFile()
        self.logs.append(log)
        options = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {}
        process = subprocess.Popen([str(self.binary), "serve", "--config", str(self.config)],
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log, **options)
        self.processes.append(process)
        end = time.monotonic() + 8
        while time.monotonic() < end:
            if process.poll() is not None:
                log.seek(0)
                self.fail("fixture gateway exited: " + log.read().decode(errors="replace"))
            try:
                if self.http("GET", "/healthz")[0] == 200:
                    return process
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.02)
        self.fail("fixture gateway did not become ready")

    def stop(self, process):
        if process.poll() is None:
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
                self.fail("fixture gateway did not drain after shutdown")

    def tearDown(self):
        try:
            for p in reversed(self.processes):
                self.stop(p)
        finally:
            for log in self.logs:
                log.close()
            self.tmp.cleanup()

    def http(self, method, path="/mcp/magpie/tools", body=None, headers=None):
        data = body if isinstance(body, (bytes, str)) or body is None else json.dumps(body, ensure_ascii=False)
        if isinstance(data, str):
            data = data.encode()
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=12)) as conn:
            conn.request(method, path, body=data,
                         headers={"Content-Type": "application/json", **(headers or {})})
            response = conn.getresponse()
            return response.status, dict(response.getheaders()), response.read()

    def rpc(self, method, params=None, ident=1, headers=None, path="/mcp/magpie/tools"):
        body = {"jsonrpc": "2.0", "method": method, "id": ident}
        if params is not None:
            body["params"] = params
        status, head, raw = self.http("POST", path, body, headers)
        return status, head, json.loads(raw) if raw else None

    def tools(self, headers=None):
        status, _, data = self.rpc("tools/list", headers=headers)
        self.assertEqual(status, 200)
        return data["result"]["tools"]

    def tool(self, suffix):
        return next(t["name"] for t in self.tools() if t["name"].endswith("_" + suffix))

    def counts(self, file):
        path = self.ext / file
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def test_handshake_ping_notifications_do_not_start_bun(self):
        self.start()
        status, head, data = self.rpc("initialize", {"protocolVersion": "2025-03-26"})
        self.assertEqual(status, 200)
        self.assertEqual(data["result"]["protocolVersion"], "2025-03-26")
        self.assertTrue(any(k.lower() == "mcp-session-id" for k in head))
        self.assertEqual(self.rpc("ping")[2]["result"], {})
        status, _, body = self.http("POST", body={"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.assertEqual((status, body), (202, b""))
        self.assertFalse(self.counts("loads.log"))

    def test_real_installed_tool_list_call_and_unknown_error(self):
        self.start()
        name = self.tool("echo")
        status, _, data = self.rpc("tools/call", {"name": name, "arguments": {"text": "你好\nλ"}}, ident=9007199254740993)
        self.assertEqual(status, 200)
        self.assertEqual(data["id"], 9007199254740993)
        self.assertEqual(data["result"], {"content": [{"type": "text", "text": "你好\nλ"}], "isError": False})
        self.assertEqual(self.rpc("tools/call", {"name": "unknown"})[2]["error"]["code"], -32602)
        self.assertEqual(self.counts("calls.log"), ["echo"])
        fail = self.rpc("tools/call", {"name": self.tool("fail")})[2]
        self.assertTrue(fail["result"]["isError"])
        self.assertIn("fixture failure", fail["result"]["content"][0]["text"])

    def test_per_plugin_endpoint_and_canonical_alias(self):
        self.start()
        name = self.tool("echo")
        prefix = name.removesuffix("_echo")
        status, _, data = self.rpc("tools/list", path="/mcp/magpie/tools/" + prefix)
        self.assertEqual(status, 200)
        self.assertIn("echo", [t["name"] for t in data["result"]["tools"]])
        self.assertEqual(self.rpc("tools/list", path="/mcp/zcalld/tools")[2]["result"]["tools"], self.tools())

    def test_gate_disabled_then_enabled_is_real_and_lazy(self):
        self.helper("mcp", "disable")
        self.start()
        self.assertEqual(self.tools(), [])
        self.assertTrue(self.rpc("tools/call", {"name": "fixture-mjs_echo"})[2]["result"]["isError"])
        self.assertFalse(self.counts("loads.log"))
        self.helper("mcp", "enable")
        self.assertEqual(len(self.tools()), 4)

    def test_disabled_community_plugin_is_not_loaded_or_listed(self):
        self.helper("disable", "fixture")
        self.start()
        self.assertEqual(self.tools(), [])
        self.assertEqual(self.rpc("tools/call", {"name": "fixture-mjs_echo"})[2]["error"]["code"], -32602)
        self.assertFalse(self.counts("loads.log"))

    def test_scoped_key_filters_and_refuses_tools(self):
        keys = [{"id": key, "name": key, "secret": "fixture-" + key, "lan": False} for key in ("scoped", "readonly", "unmapped")]
        private = self.root / "caller-keys.json"
        private.write_text(json.dumps(keys), encoding="utf-8", newline="\n")
        if os.name != "nt":
            private.chmod(0o600)
        self.write_config(caller_keys=True)
        registry_file = self.ext / "registry.json"
        registry = json.loads(registry_file.read_text(encoding="utf-8"))
        registry["mcp_permissions"] = {"scoped": {"mcp": ["magpie-tools/fixture-mjs_echo"]}, "readonly": {"read_only": True}}
        registry_file.write_text(json.dumps(registry), encoding="utf-8", newline="\n")
        self.start()
        headers = {"Authorization": "Bearer fixture-scoped"}
        self.assertEqual([t["name"] for t in self.tools(headers)], ["fixture-mjs_echo"])
        self.assertEqual(self.rpc("tools/call", {"name": "fixture-mjs_fail"}, headers=headers)[2]["error"]["code"], -32602)
        self.assertEqual(self.rpc("ping", headers={"Authorization": "Bearer fixture-readonly"})[0], 403)
        self.assertEqual(self.head_wire({"Authorization": "Bearer fixture-readonly"}), 403)
        self.assertEqual(self.rpc("ping", headers={"Authorization": "Bearer fixture-unmapped"})[0], 403)
        self.assertFalse(self.counts("calls.log"))

    def head_wire(self, headers=None):
        lines = ["HEAD /mcp/magpie/tools HTTP/1.1", f"Host: 127.0.0.1:{self.port}", "Connection: close"]
        lines.extend(f"{key}: {value}" for key, value in (headers or {}).items())
        with socket.create_connection(("127.0.0.1", self.port), timeout=3) as sock:
            sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
            chunks = []
            while True:
                data = sock.recv(16384)
                if not data:
                    break
                chunks.append(data)
        head, separator, body = b"".join(chunks).partition(b"\r\n\r\n")
        self.assertTrue(separator)
        self.assertEqual(body, b"", "HEAD emitted response body bytes")
        return int(head.split(b" ", 2)[1])

    def test_head_never_emits_a_response_body(self):
        self.start()
        self.assertEqual(self.head_wire(), 405)
        self.assertFalse(self.counts("loads.log"))
        self.helper("mcp", "disable")
        self.assertEqual(self.head_wire(), 405)

    def test_protocol_framing_and_origin_guards(self):
        self.start()
        status, headers, _ = self.http("GET")
        self.assertEqual(status, 405)
        self.assertEqual(next(v for k, v in headers.items() if k.lower() == "allow"), "POST")
        self.assertEqual(self.http("POST", body=b"{")[0], 400)
        self.assertEqual(self.rpc("ping", headers={"MCP-Protocol-Version": "bad"})[0], 400)
        self.assertEqual(self.rpc("ping", headers={"Origin": "https://untrusted.invalid"})[0], 403)
        self.assertEqual(self.rpc("ping", headers={"Host": "untrusted.invalid"})[0], 403)
        self.assertEqual(self.rpc("method/missing")[2]["error"]["code"], -32601)
        self.assertFalse(self.counts("loads.log"))

    def test_batch_and_notifications_never_run_unacknowledged_tools(self):
        self.start()
        body = [{"jsonrpc": "2.0", "method": "ping", "id": "a"},
                {"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "fixture-mjs_echo"}},
                {"jsonrpc": "2.0", "method": "tools/list", "id": "b"}]
        status, _, raw = self.http("POST", body=body)
        self.assertEqual(status, 200)
        self.assertEqual([m["id"] for m in json.loads(raw)], ["a", "b"])
        self.assertFalse(self.counts("calls.log"))

    def test_large_output_crosses_multiple_credit_frames(self):
        self.start()
        status, _, data = self.rpc("tools/call", {"name": self.tool("large")})
        self.assertEqual(status, 200)
        self.assertEqual(len(data["result"]["content"][0]["text"]), 200000)
        self.assertEqual(self.counts("calls.log"), ["large"])

    def test_response_limit_never_replays_side_effect(self):
        self.write_config(maximum=8192)
        self.start()
        status, _, _ = self.rpc("tools/call", {"name": self.tool("large")})
        self.assertEqual(status, 502)
        self.assertEqual(self.counts("calls.log"), ["large"])
        self.assertEqual(self.rpc("ping")[0], 200)

    def test_shared_host_is_one_process_across_workers_and_traits(self):
        self.start()
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            lists = list(pool.map(lambda _: self.tools(), range(8)))
        self.assertTrue(all(len(t) == 4 for t in lists))
        self.assertEqual(len(self.counts("loads.log")), 1)

    def test_shutdown_cancels_running_tool_and_drains_owned_host(self):
        process = self.start()
        name = self.tool("wait")
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        pending = pool.submit(self.rpc, "tools/call", {"name": name})
        owned_handle = None
        kernel32 = None
        try:
            end = time.monotonic() + 3
            while not self.counts("started.log") and time.monotonic() < end:
                time.sleep(0.01)
            self.assertTrue(self.counts("started.log"))
            if os.name == "nt":
                import ctypes
                from ctypes import wintypes

                kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
                kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
                kernel32.OpenProcess.restype = wintypes.HANDLE
                kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
                kernel32.WaitForSingleObject.restype = wintypes.DWORD
                kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
                kernel32.CloseHandle.restype = wintypes.BOOL
                loads = self.counts("loads.log")
                self.assertEqual(len(loads), 1, "fixture must own exactly one community host")
                # Keep this fixture's process identity before stopping its parent.
                owned_handle = kernel32.OpenProcess(0x00100000 | 0x1000, False, int(loads[0]))
                self.assertTrue(owned_handle, f"cannot observe owned community host: Windows error {ctypes.get_last_error()}")
            start = time.monotonic()
            self.stop(process)
            self.assertLess(time.monotonic() - start, 4)
            self.assertEqual(self.counts("aborted.log"), ["wait"])
            with contextlib.suppress(OSError, http.client.HTTPException):
                pending.result(timeout=2)
            if os.name == "nt":
                result = kernel32.WaitForSingleObject(owned_handle, 2000)
                self.assertEqual(result, 0, f"owned community host survived gateway drain (wait={result}, Windows error {ctypes.get_last_error()})")
            elif os.name == "posix":
                pid = int(self.counts("loads.log")[0])
                end = time.monotonic() + 2
                while time.monotonic() < end:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail("owned community host survived gateway drain")
        finally:
            if owned_handle is not None:
                kernel32.CloseHandle(owned_handle)
            pool.shutdown(wait=True, cancel_futures=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=os.environ.get("ZCALLD_TEST_BINARY", "target/debug/zcalld"))
    parser.add_argument("--community-plugin", required=True)
    parser.add_argument("--pluginctl", required=True)
    parser.add_argument("--bun", default=shutil.which("bun"))
    args = parser.parse_args()
    if not args.bun:
        parser.error("Bun is required for the optional MCP fixture")
    for name in ("binary", "community_plugin", "pluginctl", "bun"):
        setattr(MCPTests, name, Path(getattr(args, name)).resolve(strict=True))
    unittest.main(argv=[__file__], verbosity=2)
