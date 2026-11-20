#!/usr/bin/env python3
"""Portable native transport + dynamic plugin acceptance (no real providers)."""
import argparse
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
args = parser.parse_args()
binary = Path(args.binary).resolve()
prefix = "" if os.name == "nt" else "lib"
suffix = ".dll" if os.name == "nt" else ".dylib" if os.sys.platform == "darwin" else ".so"
observed = []

class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        observed.append((self.path, dict(self.headers), body))
        if body.get("stream"):
            response = (b'data: {"id":"fixture","object":"chat.completion.chunk","model":"upstream",'
                        b'"choices":[{"index":0,"delta":{"content":"OK"}}]}\n\ndata: [DONE]\n\n')
            content_type = "text/event-stream"
        else:
            response = json.dumps({"id":"fixture","object":"chat.completion","model":"upstream",
                "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).encode()
            content_type = "application/json"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)
        self.wfile.flush()

upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
thread = threading.Thread(target=upstream.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix="zcalld-native-") as temporary:
        root = Path(temporary).resolve()
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
        listener.close()
        config = root / "config.conf"
        config.write_text(
            f"[server]\nlisten = 127.0.0.1:{port}\nunix_socket = none\nworkers = 2\n"
            "token_env = NATIVE_GATEWAY_TOKEN\n"
            f"[provider fixture]\nbase_url = http://127.0.0.1:{upstream.server_port}/v1\n"
            "protocol = chat\napi_key_env = NATIVE_PROVIDER_KEY\n"
            "[route public]\nprovider = fixture\nmodel = upstream\n"
            f"[plugin alias]\npath = {binary.parent / (prefix+'zcalld_aliases'+suffix)}\nconfig = fast=public\n"
            f"[plugin policy]\npath = {binary.parent / (prefix+'zcalld_policy'+suffix)}\nconfig = allow=public\n", encoding="utf-8", newline="\n"
        )
        env = dict(os.environ, NATIVE_GATEWAY_TOKEN="gateway-fixture", NATIVE_PROVIDER_KEY="provider-fixture")
        with (root / "gateway.log").open("w+") as log:
            proc = subprocess.Popen([str(binary), "serve", "--config", str(config)], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while True:
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as r:
                            assert r.status == 200
                        break
                    except (OSError, urllib.error.URLError):
                        if proc.poll() is not None or time.monotonic() >= deadline:
                            log.seek(0)
                            raise AssertionError("native gateway startup failed: " + log.read()[-4000:])
                        time.sleep(.05)
                def generate(model, streaming=False):
                    body = json.dumps({"model":model, "messages":[{"role":"user","content":"fixture"}], "stream":streaming}).encode()
                    request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions", body,
                        {"Authorization":"Bearer gateway-fixture","Content-Type":"application/json"})
                    return urllib.request.urlopen(request, timeout=10)
                with generate("fast") as r:
                    assert json.load(r)["choices"][0]["message"]["content"] == "OK"
                with generate("fast", True) as r:
                    body = r.read()
                    assert b"OK" in body and b"[DONE]" in body
                try:
                    generate("blocked")
                    raise AssertionError("native policy did not reject route")
                except urllib.error.HTTPError as error:
                    assert error.code == 403
                assert len(observed) == 2
                for path, headers, body in observed:
                    assert path == "/v1/chat/completions"
                    assert body["model"] == "upstream"
                    assert headers.get("Authorization") == "Bearer provider-fixture"
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try: proc.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        proc.kill(); proc.wait()
                        raise AssertionError("native gateway failed to terminate")
finally:
    upstream.shutdown()
    upstream.server_close()
    thread.join(timeout=2)
print("Native runtime passed: TCP, JSON, SSE, dynamic alias/policy ABI, credential separation")
