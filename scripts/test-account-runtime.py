#!/usr/bin/env python3
"""Exercise live account changes through the real native bridge and a loopback fixture."""
import argparse
import http.client
import importlib.util
import json
from pathlib import Path
import shutil
import sys
sys.dont_write_bytecode = True
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("transport_fixture", ROOT / "scripts/test-transport.py")
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)

class AccountRuntime(transport.TransportTests):
    pluginctl = None

    def account(self, *args):
        result = subprocess.run([str(self.pluginctl), "--root", str(self.root), "accounts", *args, "--json"],
                                capture_output=True, timeout=8, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def last_headers(self):
        return {key.lower(): value for key, value in self.upstream.requests[-1]["headers"].items()}

    def call(self):
        return transport.request(http.client.HTTPConnection("127.0.0.1", self.port, timeout=5),
                                 "/v1/chat/completions",
                                 {"model": "test", "messages": [{"role": "user", "content": "account fixture"}]})

    def test_account_selection_and_disable_reach_the_running_provider(self):
        self.community()
        (self.root / "provider.mjs").write_text(
            "export default async()=>({auth:{provider:'fixture',methods:[{type:'api',label:'Fixture'}],"
            "loader:async(getAuth)=>({apiKey:'fixture-upstream',fetch:async(url,init)=>{"
            "const auth=await getAuth();if(!auth)throw new Error('missing fixture auth');"
            "const headers=new Headers(init.headers);headers.set('x-zcalld-account',auth.key);"
            "headers.set('x-zcalld-host-pid',String(process.pid));"
            "return fetch(url,{...init,headers})}})}})")
        (self.root / "plugin-auth.json").write_text(json.dumps({
            "fixture": {"type": "api", "key": "synthetic-first"},
            "fixture#second": {"type": "api", "key": "synthetic-second"},
        }))
        # Canonicalize the fixture before the first host starts.
        self.account("enable", "fixture")
        rows = self.account("list")["accounts"]
        self.assertEqual(len(rows), 2)
        self.assertNotIn("synthetic-", json.dumps(rows))
        self.start()
        status, _ = self.call()
        self.assertEqual(status, 200)
        first_headers = self.last_headers()
        self.assertEqual(first_headers.get("x-zcalld-account"), "synthetic-first")
        pid = first_headers["x-zcalld-host-pid"]

        result = self.account("use", "fixture#second")
        self.assertEqual(result["selection"]["account"], "fixture#second")
        status, _ = self.call()
        self.assertEqual(status, 200)
        second_headers = self.last_headers()
        self.assertEqual(second_headers.get("x-zcalld-account"), "synthetic-second")
        self.assertEqual(second_headers["x-zcalld-host-pid"], pid)

        self.account("disable", "fixture#second")
        before = len(self.upstream.requests)
        status, _ = self.call()
        self.assertGreaterEqual(status, 500)
        self.assertEqual(len(self.upstream.requests), before)
        self.account("enable", "fixture#second")
        status, _ = self.call()
        self.assertEqual(status, 200)
        self.assertEqual(self.last_headers()["x-zcalld-host-pid"], pid)
        self.account("remove", "fixture#second")
        self.assertNotIn("fixture#second", json.loads((self.root / "plugin-auth.json").read_text()))
        status, _ = self.call()
        self.assertEqual(status, 200)
        self.assertEqual(self.last_headers().get("x-zcalld-account"), "synthetic-first")

if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", default=str(ROOT / "target/debug/zcalld"))
    p.add_argument("--community-plugin", default=str(ROOT / "target/debug/libzcalld_community.so"))
    p.add_argument("--pluginctl", default=str(ROOT / "extensions/compat/bin/zcalld-pluginctl"))
    p.add_argument("--bun", default=shutil.which("bun"))
    args = p.parse_args()
    for key in ("binary", "community_plugin", "pluginctl", "bun"):
        value = getattr(args, key)
        if not value: p.error(key + " is required")
        setattr(AccountRuntime, key, Path(value).resolve(strict=True))
    AccountRuntime.community_host = ROOT / "extensions/compat/community/vendor/host.js"
    AccountRuntime.community_pi = ROOT / "extensions/compat/community/vendor/pi.js"
    suite = unittest.TestSuite([AccountRuntime("test_account_selection_and_disable_reach_the_running_provider")])
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
