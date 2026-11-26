#!/usr/bin/env python3
"""Exercise native decision forwarding and virtual routing with local upstreams."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread


def check(binary, routing):
    calls = []

    class Upstream(BaseHTTPRequestHandler):
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            calls.append((self.path, body, self.headers.get("Idempotency-Key")))
            if body["model"] == "jev-latest" and isinstance(body.get("state"), dict) and body["state"].get("simulate_limit"):
                self.send_response(429)
                self.send_header("Retry-After", "1")
                self.send_header("Content-Length", "2")
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b"{}")
                return
            answers = (
                {"paid": {"type": "noul", "noul": 0.875}}
                if self.path == "/v1/systemone"
                else [{"type": "predicate", "name": "paid", "probability": 0.875}]
            )
            raw = json.dumps({"model": body["model"], "answers": answers}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.send_header("X-Idempotency-Replayed", "false")
            self.end_headers()
            self.wfile.write(raw)

        def log_message(self, *_):
            pass

    with (
        tempfile.TemporaryDirectory(prefix="zcalld-decisions-") as folder,
        ThreadingHTTPServer(("127.0.0.1", 0), Upstream) as upstream,
        socket.socket() as reserve,
    ):
        Thread(target=upstream.serve_forever, daemon=True).start()
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
        reserve.close()
        root = Path(folder)
        env = dict(os.environ, HOME=folder, XDG_CONFIG_HOME=folder, ZCALLD_PLUGIN_ROUTING_PLUGIN=str(routing))
        path = root / "config.conf"
        upstream_url = f"http://127.0.0.1:{upstream.server_port}/v1"
        path.write_text(
            f"[server]\nlisten = 127.0.0.1:{port}\nunix_socket = none\n"
            f"[provider decision-a]\nbase_url = {upstream_url}\nprotocol = systemone\n"
            f"[provider decision-b]\nbase_url = {upstream_url}/systemone\nprotocol = systemone\n"
            f"[provider openai]\nbase_url = {upstream_url}/decisions\nprotocol = decisions\n"
            "[route a]\nprovider = decision-a\nmodel = jev-latest\n"
            "[route b]\nprovider = decision-b\nmodel = jev-1.14.0\n"
            "[route oai]\nprovider = openai\nmodel = gpt-6-luna\n"
        )
        catalog_id = "typesafe/jev-latest"
        (root / "models-dev.json").write_text(json.dumps({
            catalog_id: {"id": catalog_id, "name": "Jev", "type": "decision"},
            "openai/gpt-6-luna": {"id": "openai/gpt-6-luna", "name": "GPT-6 Luna"},
        }))

        def cli(*args, definition=None, ok=True):
            call = [str(binary), *args, "--json", "--config", str(path)]
            body = None if definition is None else json.dumps(definition)
            result = subprocess.run(call, input=body, capture_output=True, text=True, env=env, timeout=10)
            assert (result.returncode == 0) == ok, result.stdout + result.stderr
            return json.loads(result.stdout)

        assert cli("model-catalog", "list", "--type", "decision")["models"][0]["id"] == catalog_id
        cli("groups", "set", "virtual", "--stdin", definition={
            "members": ["a", "b"], "mode": "rotate", "affinity": "none", "catalog_model": catalog_id
        })
        cli("groups", "set", "ordered", "--stdin", definition={
            "members": ["a", "b"], "mode": "order", "affinity": "none"
        })
        cli("groups", "set", "openai-virtual", "--stdin", definition={
            "members": ["oai"], "mode": "order", "catalog_model": "openai/gpt-6-luna"
        })
        cli("groups", "set", "mixed", "--stdin",
            definition={"members": ["a", "oai"], "mode": "order"}, ok=False)
        assert next(
            row for row in cli("models")["data"] if row["id"] == "virtual"
        )["catalog_metadata"]["id"] == catalog_id

        process = subprocess.Popen(
            [str(binary), "serve", "--config", str(path)],
            env=env, stderr=subprocess.PIPE, text=True,
        )
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def request(path, data=None, headers=None, status=200):
            body = None if data is None else json.dumps(data).encode()
            headers = {"Content-Type": "application/json", **(headers or {})}
            req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", body, headers)
            try:
                response = opener.open(req, timeout=3)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                assert response.code == status, response.read()
                return json.load(response), response.headers

        try:
            for _ in range(80):
                try:
                    request("/healthz")
                    break
                except (urllib.error.URLError, ConnectionResetError):
                    if process.poll() is not None:
                        raise AssertionError(process.stderr.read())
                    time.sleep(0.05)
            else:
                raise AssertionError("gateway did not start")
            system_one = {"model": "a", "state": {"amount": 9007199254740993},
                          "questions": {"paid": {"type": "noul"}}}
            answer, headers = request("/v1/systemone", system_one, {"Idempotency-Key": "unique-key"})
            assert answer["answers"]["paid"]["noul"] == 0.875
            assert headers["X-Idempotency-Replayed"] == "false"
            assert calls[0] == ("/v1/systemone", {**system_one, "model": "jev-latest"}, "unique-key")
            system_one["model"] = "virtual"
            request("/v1/systemone", system_one, {"Idempotency-Key": "virtual-key"}, status=422)
            request("/v1/systemone", system_one)
            request("/v1/systemone", system_one)
            assert all(call[1]["state"]["amount"] == 9007199254740993 for call in calls[1:3])
            assert {call[1]["model"] for call in calls[1:3]} == {"jev-latest", "jev-1.14.0"}, calls
            decision = {"model": "openai-virtual", "input": "Invoice",
                        "questions": [{"type": "predicate", "name": "paid", "instructions": "Paid?"}]}
            answer, _ = request("/v1/decisions", decision)
            assert answer["answers"][0]["probability"] == 0.875
            assert calls[3][0:2] == ("/v1/decisions", {**decision, "model": "gpt-6-luna"})
            request("/v1/decisions", {**decision, "model": "a"}, status=422)
            assert len(calls) == 4
            system_one["model"] = "ordered"
            system_one["state"]["simulate_limit"] = True
            answer, _ = request("/v1/systemone", system_one)
            assert answer["model"] == "jev-1.14.0"
            assert [call[1]["model"] for call in calls[4:]] == ["jev-latest", "jev-1.14.0"]
            assert all(call[1]["state"] == system_one["state"] for call in calls[4:])
        finally:
            process.terminate()
            try:
                process.wait(4)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            process.stderr.close()
            upstream.shutdown()

        assert cli("providers", "disable", "decision-a")["enabled"] is False
        assert "a" not in {row["id"] for row in cli("models")["data"]}
        assert "virtual" in {row["id"] for row in cli("models")["data"]}
        assert {"a", "b"} <= {row["name"] for row in cli("routes", "list")["routes"]}
        assert cli("providers", "enable", "decision-a")["enabled"] is True
        assert cli("providers", "remove", "decision-a")["detached_members"] == 2
        for group in cli("groups", "list")["groups"]:
            if group["name"] in {"ordered", "virtual"}:
                assert [member["model"] for member in group["members"]] == ["b"], group
        assert cli("providers", "sync")["restart_required"] is False


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("routing_library", type=Path)
    args = parser.parse_args()
    check(args.binary.resolve(), args.routing_library.resolve())
    print("Native decision endpoints, typed responses, metadata and virtual routing passed")
