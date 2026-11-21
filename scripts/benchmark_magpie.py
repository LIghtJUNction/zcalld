#!/usr/bin/env python3
"""Compare zcalld and Magpie using isolated local routing and usage workloads.

Both daemons route the same group to one local API provider and record token
usage. JSON and SSE responses are checked semantically; Magpie may rewrite the
response model and JSON formatting. The whole fixture runs in a fresh Linux
network namespace, so background update/catalog requests cannot leave the host.
"""

from __future__ import annotations

import argparse
import concurrent.futures
from datetime import datetime, timezone
import http.client
import http.server
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from benchmark_rewrites import (BenchmarkParser, MemorySampler, benchmark_entry,
                                digest, percentile, process_tree, save_report)


MODEL = "group/benchmark"
UPSTREAM_MODEL = "upstream-model"
PROMPT = "Reply with the benchmark response; preserve this fixed local fixture."
TEXT = "benchmark complete"
UPSTREAM_KEY = "fixture-upstream-key"
CALLER_KEY = "fixture-caller-key"
USAGE = {"prompt_tokens": 19, "completion_tokens": 7, "total_tokens": 26}
MAX_RESPONSE = 128 * 1024


def isolated_environment(root: Path, port: int) -> dict[str, str]:
    for name in ("config", "data", "cache", "state", "runtime"):
        (root / name).mkdir(mode=0o700)
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "TZ": "UTC",
        "HOME": str(root), "USERPROFILE": str(root), "APPDATA": str(root / "config"),
        "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
        "XDG_CACHE_HOME": str(root / "cache"), "XDG_STATE_HOME": str(root / "state"),
        "XDG_RUNTIME_DIR": str(root / "runtime"), "MAGPIE_ADDR": f"127.0.0.1:{port}",
        "MAGPIE_NO_ACCOUNTS": "1", "DO_NOT_TRACK": "1", "MAGPIE_DRAIN": "0",
        "NO_PROXY": "127.0.0.1,localhost", "BENCH_UPSTREAM_KEY": UPSTREAM_KEY,
    }


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class Upstream(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64

    def __init__(self, chunk_delay: float) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.sequence = 0
        self.chunk_delay = chunk_delay
        self.streaming = False
        self.generations = 0
        self.catalog_requests = 0
        self.errors: list[str] = []
        self.thread = threading.Thread(target=self.serve_forever,
                                       kwargs={"poll_interval": 0.05}, daemon=True)
        self.thread.start()

    def begin(self, streaming: bool) -> None:
        with self.lock:
            self.streaming = streaming
            self.generations = 0
            self.catalog_requests = 0
            self.errors = []

    def observed(self) -> dict:
        with self.lock:
            return {"generations": self.generations, "catalog_requests": self.catalog_requests,
                    "errors": list(self.errors)}

    def close(self) -> None:
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.connection.settimeout(15)

    def log_message(self, *_: object) -> None:
        pass

    def respond(self, status: int, data: bytes, content_type: str = "application/json") -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def do_GET(self) -> None:
        server: Upstream = self.server
        with server.lock:
            server.catalog_requests += 1
        if self.path != "/v1/models":
            with server.lock:
                server.errors.append("unexpected upstream metadata path")
            self.respond(404, b'{"error":"unknown fixture route"}')
            return
        self.respond(200, json.dumps({"object": "list", "data": [
            {"id": UPSTREAM_MODEL, "object": "model", "owned_by": "fixture"},
        ]}, separators=(",", ":")).encode())

    def do_POST(self) -> None:
        server: Upstream = self.server
        with server.lock:
            server.sequence += 1
            sequence = server.sequence
            server.generations += 1
        error = None
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if not 0 < length <= 64 * 1024:
                raise ValueError("unexpected upstream request size")
            value = json.loads(self.rfile.read(length))
            if self.path != "/v1/chat/completions":
                raise ValueError("unexpected upstream generation path")
            if self.headers.get("Authorization") != "Bearer " + UPSTREAM_KEY:
                raise ValueError("provider credential was changed or caller credential was forwarded")
            if value.get("model") != UPSTREAM_MODEL:
                raise ValueError("upstream model mismatch")
            if value.get("messages") != [{"role": "user", "content": PROMPT}]:
                raise ValueError("prompt content or roles changed")
            if value.get("stream", False) != server.streaming:
                raise ValueError("stream mode mismatch")
        except (ValueError, TypeError, KeyError, OSError) as failure:
            error = str(failure)
        if error:
            with server.lock:
                if len(server.errors) < 16:
                    server.errors.append(error)
            self.respond(400, b'{"error":"fixture validation failed"}')
            return
        identity = f"benchmark-{sequence:016d}"
        if not server.streaming:
            body = {"id": identity, "object": "chat.completion", "model": UPSTREAM_MODEL,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": TEXT},
                                 "finish_reason": "stop"}], "usage": USAGE}
            self.respond(200, json.dumps(body, separators=(",", ":")).encode())
            return
        base = {"id": identity, "object": "chat.completion.chunk", "model": UPSTREAM_MODEL}
        events = [
            {**base, "choices": [{"index": 0, "delta": {"role": "assistant"}}]},
            {**base, "choices": [{"index": 0, "delta": {"content": "benchmark "}}]},
            {**base, "choices": [{"index": 0, "delta": {"content": "complete"}}]},
            {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            {**base, "choices": [], "usage": USAGE},
        ]
        chunks = [("data: " + json.dumps(event, separators=(",", ":")) + "\n\n").encode()
                  for event in events] + [b"data: [DONE]\n\n"]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(sum(map(len, chunks))))
        self.end_headers()
        try:
            for index, chunk in enumerate(chunks):
                self.wfile.write(chunk)
                self.wfile.flush()
                if index < 3 and server.chunk_delay:
                    time.sleep(server.chunk_delay)
        except (BrokenPipeError, ConnectionResetError):
            pass


class Gateway:
    def __init__(self, variant: str, binary: Path, plugin: Path | None,
                 upstream: int, workers: int, deadline: float) -> None:
        self.variant, self.binary = variant, binary
        self.temporary = tempfile.TemporaryDirectory(prefix=f"{variant}-comparison-")
        self.root = Path(self.temporary.name)
        self.port = free_port()
        self.environment = isolated_environment(self.root, self.port)
        self.process = None
        self.log = tempfile.TemporaryFile()
        self.startup_ms = 0.0
        self.setup = []
        try:
            if variant == "magpie":
                for args in [
                    ["provider", "add", "benchmark", "id=benchmark",
                     f"url=http://127.0.0.1:{upstream}/v1", "key=" + UPSTREAM_KEY,
                     "models=" + UPSTREAM_MODEL],
                    ["group", "add", "benchmark", "models=benchmark/" + UPSTREAM_MODEL,
                     "routing=order", "stays=off"],
                ]:
                    result = subprocess.run([str(binary), *args], env=self.environment,
                                            text=True, capture_output=True,
                                            timeout=max(0.1, min(15, deadline - time.monotonic())))
                    self.setup.append({"command": args[:3], "exit_code": result.returncode})
                    if result.returncode:
                        raise RuntimeError("Magpie fixture configuration failed: "
                                           + (result.stdout + result.stderr)[-3000:])
                command = [str(binary), "serve"]
            else:
                groups = self.root / "routing.json"
                self.usage_file = self.root / "usage.jsonl"
                groups.write_text(json.dumps({"groups": [{
                    "name": MODEL, "mode": "order", "members": [{"model": "benchmark/" + UPSTREAM_MODEL}],
                    "original": {"affinity": "none", "effort": "auto"},
                }], "usage_file": str(self.usage_file)}), encoding="utf-8")
                config = self.root / "config.conf"
                config.write_text(
                    f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers={workers}\n"
                    "queue=16\nmax_body_bytes=65536\nmax_response_bytes=131072\n"
                    "request_timeout_ms=15000\nupstream_timeout_ms=15000\nconnect_timeout_ms=1000\n"
                    f"[provider benchmark]\nbase_url=http://127.0.0.1:{upstream}/v1\nprotocol=chat\n"
                    "api_key_env=BENCH_UPSTREAM_KEY\n"
                    f"[route benchmark/{UPSTREAM_MODEL}]\nprovider=benchmark\nmodel={UPSTREAM_MODEL}\n"
                    f"[route {MODEL}]\nprovider=benchmark\nmodel={UPSTREAM_MODEL}\n"
                    f"[plugin routing]\npath={plugin}\nconfig={groups}\n", encoding="utf-8")
                command = [str(binary), "serve", "--config", str(config)]
            started = time.monotonic()
            self.process = subprocess.Popen(command, env=self.environment, stdout=self.log, stderr=self.log)
            while time.monotonic() < min(deadline, started + 15):
                if self.process.poll() is not None:
                    break
                connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=0.2)
                try:
                    connection.request("GET", "/" if variant == "magpie" else "/healthz")
                    response = connection.getresponse()
                    response.read()
                    if response.status == 200:
                        self.startup_ms = (time.monotonic() - started) * 1000
                        return
                except OSError:
                    pass
                finally:
                    connection.close()
                time.sleep(0.01)
            self.log.seek(0)
            raise RuntimeError("gateway did not become healthy: "
                               + self.log.read()[-3000:].decode(errors="replace"))
        except BaseException:
            self.close()
            raise

    def usage(self, expected: int, deadline: float) -> dict:
        end = min(deadline, time.monotonic() + 2)
        while True:
            paths = list(self.root.rglob("usage.jsonl")) if self.variant == "magpie" else [self.usage_file]
            rows, ignored = [], 0
            for path in paths:
                if not path.exists():
                    continue
                for line in path.read_bytes().splitlines():
                    try:
                        value = json.loads(line)
                    except (ValueError, UnicodeDecodeError):
                        continue
                    requested = value.get("req") if self.variant == "magpie" else value.get("model")
                    if requested != MODEL or value.get("kind") == "model_list":
                        ignored += 1
                        continue
                    rows.append(value)
            totals = {"requests": len(rows), "input_tokens": 0, "output_tokens": 0, "failed": 0}
            for row in rows:
                totals["input_tokens"] += row.get("in", row.get("input_tokens", 0))
                totals["output_tokens"] += row.get("out", row.get("output_tokens", 0))
                totals["failed"] += int(row.get("status", 200) >= 400)
            valid = totals == {"requests": expected, "input_tokens": 19 * expected,
                               "output_tokens": 7 * expected, "failed": 0}
            if valid or time.monotonic() >= end:
                return {**totals, "verified": valid, "ignored_metadata_records": ignored,
                        "files": [str(path.relative_to(self.root)) for path in paths]}
            time.sleep(0.02)

    def close(self) -> dict:
        killed = False
        result = {"exit_code": None, "killed": False, "clean": True}
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    killed = True
                    self.process.kill()
                    self.process.wait(timeout=2)
            allowed = (0, -signal.SIGTERM) if self.variant == "magpie" else (0,)
            result = {"exit_code": self.process.returncode, "killed": killed,
                      "clean": not killed and self.process.returncode in allowed}
        self.log.close()
        self.temporary.cleanup()
        return result


def request_once(port: int, payload: bytes, streaming: bool, deadline: float) -> dict:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("benchmark execution budget exhausted")
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=min(15, remaining))
    started = time.perf_counter_ns()
    text, usage, first_text, finished, done = [], {}, None, False, False
    try:
        connection.request("POST", "/v1/chat/completions", payload,
                           {"Content-Type": "application/json", "Authorization": "Bearer " + CALLER_KEY})
        response = connection.getresponse()
        headers_ms = (time.perf_counter_ns() - started) / 1_000_000
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}: {response.read(400)!r}")
        if not streaming:
            data = response.read(MAX_RESPONSE + 1)
            first_text = (time.perf_counter_ns() - started) / 1_000_000
            if len(data) > MAX_RESPONSE:
                raise RuntimeError("oversized fixture response")
            value = json.loads(data)
            text = [value["choices"][0]["message"]["content"]]
            usage = value.get("usage", {})
            finished = value["choices"][0].get("finish_reason") == "stop"
            done = True
        else:
            if "text/event-stream" not in response.getheader("Content-Type", ""):
                raise RuntimeError("stream request did not return SSE")
            data_lines, total = [], 0
            while True:
                raw = response.readline(MAX_RESPONSE + 1)
                total += len(raw)
                if total > MAX_RESPONSE:
                    raise RuntimeError("oversized fixture SSE response")
                if not raw:
                    break
                line = raw.rstrip(b"\r\n")
                if line.startswith(b"data:"):
                    data_lines.append(line[5:].lstrip(b" "))
                elif not line and data_lines:
                    data = b"\n".join(data_lines)
                    data_lines.clear()
                    if data == b"[DONE]":
                        done = True
                        continue
                    value = json.loads(data)
                    if value.get("usage"):
                        usage = value["usage"]
                    for choice in value.get("choices", []):
                        content = choice.get("delta", {}).get("content")
                        if content:
                            if first_text is None:
                                first_text = (time.perf_counter_ns() - started) / 1_000_000
                            text.append(content)
                        finished |= choice.get("finish_reason") == "stop"
        if "".join(text) != TEXT or not finished or not done:
            raise RuntimeError("response text, finish reason or stream termination mismatch")
        if usage.get("prompt_tokens") != 19 or usage.get("completion_tokens") != 7:
            raise RuntimeError("response token usage mismatch")
    finally:
        connection.close()
    return {"latency_ms": (time.perf_counter_ns() - started) / 1_000_000,
            "ttft_ms": first_text, "headers_ms": headers_ms}


def workload(port: int, payload: bytes, streaming: bool, count: int,
             concurrency: int, deadline: float) -> dict:
    barrier = threading.Barrier(concurrency + 1)

    def worker(index: int) -> tuple[list[dict], list[str]]:
        values, errors = [], []
        barrier.wait()
        for _ in range(index, count, concurrency):
            try:
                values.append(request_once(port, payload, streaming, deadline))
            except (OSError, RuntimeError, ValueError, KeyError, http.client.HTTPException) as error:
                errors.append(str(error))
                break
        return values, errors

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [executor.submit(worker, index) for index in range(concurrency)]
        started = time.perf_counter()
        barrier.wait()
        parts = [future.result() for future in futures]
        elapsed = time.perf_counter() - started
    samples = [value for values, _ in parts for value in values]
    result = {"requests": count, "successful_requests": len(samples),
              "errors": [error for _, errors in parts for error in errors],
              "elapsed_seconds": round(elapsed, 6), "requests_per_second": round(len(samples) / elapsed, 3),
              "samples": [{name: round(value, 4) for name, value in sample.items()} for sample in samples]}
    if samples:
        result["metrics"] = {name: {"p50": round(percentile([sample[name] for sample in samples], .5), 4),
                                   "p95": round(percentile([sample[name] for sample in samples], .95), 4)}
                             for name in ("latency_ms", "ttft_ms", "headers_ms")}
    return result


def run_case(variant: str, artifacts: dict, upstream: Upstream, payload: bytes,
             streaming: bool, concurrency: int, args: argparse.Namespace, deadline: float) -> dict:
    upstream.begin(streaming)
    gateway, sampler = None, None
    result = {"variant": variant, "mode": "sse" if streaming else "json", "concurrency": concurrency,
              "zcalld_workers": concurrency if variant == "zcalld" else None,
              "status": "failed", "payload_bytes": len(payload)}
    try:
        gateway = Gateway(variant, Path(artifacts[variant]["binary"]),
                          Path(artifacts[variant]["routing_plugin"]) if variant == "zcalld" else None,
                          upstream.server_port, concurrency, deadline)
        result["setup"] = gateway.setup
        result["startup_to_health_ms"] = round(gateway.startup_ms, 3)
        result["idle"] = process_tree(gateway.process.pid)
        warmup = max(args.warmup, concurrency) if args.warmup else 0
        result["warmup"] = workload(gateway.port, payload, streaming, warmup, concurrency, deadline)
        if result["warmup"]["successful_requests"] != warmup:
            raise RuntimeError("warm-up requests failed")
        result["after_warmup"] = process_tree(gateway.process.pid)
        sampler = MemorySampler(gateway.process.pid)
        sampler.observe(result["after_warmup"])
        sampler.thread.start()
        result["workload"] = workload(gateway.port, payload, streaming, args.requests, concurrency, deadline)
        sampler.stop()
        result["after_workload"] = process_tree(gateway.process.pid)
        sampler.observe(result["after_workload"])
        result["measured_peak"] = {**sampler.peak, "samples": sampler.samples,
                                   "sampling_errors": sampler.errors}
        result["upstream"] = upstream.observed()
        if result["workload"]["successful_requests"] != args.requests:
            raise RuntimeError("measured requests failed")
        if (result["upstream"]["generations"] != warmup + args.requests
                or result["upstream"]["errors"]):
            raise RuntimeError("upstream request count or content validation failed")
        result["persisted_usage"] = gateway.usage(warmup + args.requests, deadline)
        if not result["persisted_usage"]["verified"]:
            raise RuntimeError("persisted request/token usage did not match completed generations")
        if sampler.errors:
            raise RuntimeError("memory sampling failed")
        result["status"] = "passed"
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError, http.client.HTTPException) as error:
        result["error"] = str(error)
        result["upstream"] = upstream.observed()
    finally:
        if sampler is not None and sampler.thread.is_alive():
            sampler.stop()
        if gateway is not None:
            result["shutdown"] = gateway.close()
            if not result["shutdown"]["clean"]:
                result["status"] = "failed"
                result.setdefault("error", "gateway required an unclean shutdown")
    return result


def summarize(results: list[dict]) -> list[dict]:
    summary = []
    for mode in ("json", "sse"):
        for concurrency in (1, 4):
            for variant in ("magpie", "zcalld"):
                runs = [run for run in results if run["status"] == "passed"
                        and (run["mode"], run["concurrency"], run["variant"]) == (mode, concurrency, variant)]
                if not runs:
                    continue
                metrics = {
                    "requests_per_second": [run["workload"]["requests_per_second"] for run in runs],
                    "latency_p95_ms": [run["workload"]["metrics"]["latency_ms"]["p95"] for run in runs],
                    "ttft_p50_ms": [run["workload"]["metrics"]["ttft_ms"]["p50"] for run in runs],
                    "ttft_p95_ms": [run["workload"]["metrics"]["ttft_ms"]["p95"] for run in runs],
                    "peak_tree_rss_kib": [run["measured_peak"]["tree_rss_kib"] for run in runs],
                    "daemon_hwm_kib": [run["after_workload"]["daemon_hwm_kib"] for run in runs],
                }
                summary.append({"mode": mode, "concurrency": concurrency, "variant": variant, "rounds": len(runs),
                                "metrics": {name: {"median": round(statistics.median(values), 4),
                                                   "min": min(values), "max": max(values)}
                                            for name, values in metrics.items()}})
    return summary


def main() -> int:
    parser = BenchmarkParser(description=__doc__)
    parser.add_argument("--magpie-bin", default="/usr/bin/magpie", type=lambda value: Path(value).resolve())
    parser.add_argument("--zcalld-bin", required=True, type=lambda value: Path(value).resolve())
    parser.add_argument("--zcalld-plugin-dir", required=True, type=lambda value: Path(value).resolve())
    parser.add_argument("--requests", type=int, default=80)
    parser.add_argument("--warmup", type=int, default=8)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--budget-seconds", type=float, default=180)
    parser.add_argument("--sse-chunk-delay-ms", type=float, default=1)
    parser.add_argument("--probe-only", action="store_true", help="one JSON/SSE request per daemon at concurrency 1")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--outer-network-namespace", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Linux /proc and bubblewrap are required")
    if not (1 <= args.requests <= 10000 and 0 <= args.warmup <= 1000 and 1 <= args.rounds <= 20
            and 1 <= args.budget_seconds <= 3600 and 0 <= args.sse_chunk_delay_ms <= 100):
        parser.error("invalid workload size, timing or budget")
    namespace = os.readlink("/proc/self/ns/net")
    if not args.outer_network_namespace:
        bwrap = shutil.which("bwrap")
        if not bwrap:
            parser.error("bwrap is required: external network access must remain disabled")
        return subprocess.call([bwrap, "--unshare-net", "--die-with-parent", "--bind", "/", "/",
                                "--proc", "/proc", "--dev-bind", "/dev", "/dev",
                                sys.executable, str(Path(__file__).resolve()), *sys.argv[1:],
                                "--outer-network-namespace", namespace])
    if namespace == args.outer_network_namespace:
        parser.error("network namespace was not isolated")
    if args.probe_only:
        args.requests, args.warmup, args.rounds = 1, 0, 1
    binaries = {"magpie": args.magpie_bin, "zcalld": args.zcalld_bin}
    artifacts = {}
    for variant, binary in binaries.items():
        if not binary.is_file():
            parser.error(f"missing binary: {binary}")
        with tempfile.TemporaryDirectory(prefix="benchmark-version-") as temporary:
            environment = isolated_environment(Path(temporary), free_port())
            command = [str(binary), "--version" if variant == "magpie" else "version"]
            version = subprocess.run(command, env=environment, text=True, capture_output=True, timeout=5)
            if version.returncode:
                parser.error(f"could not read {variant} version: {(version.stdout + version.stderr)[-1000:]}")
        artifacts[variant] = {"binary": str(binary), "binary_bytes": binary.stat().st_size,
                              "binary_sha256": digest(binary), "version": version.stdout.strip()}
    plugin = args.zcalld_plugin_dir / "libzcalld_plugin_routing.so"
    if not plugin.is_file():
        parser.error(f"missing native routing plugin: {plugin}")
    artifacts["zcalld"].update(routing_plugin=str(plugin), routing_plugin_sha256=digest(plugin),
                               routing_plugin_bytes=plugin.stat().st_size)
    payloads = {}
    for streaming in (False, True):
        value = {"model": MODEL, "messages": [{"role": "user", "content": PROMPT}], "stream": streaming}
        if streaming:
            value["stream_options"] = {"include_usage": True}
        payloads[streaming] = json.dumps(value, separators=(",", ":")).encode()
    started = time.monotonic()
    deadline = started + args.budget_seconds
    report = {
        "schema_version": 1, "measured_at_utc": datetime.now(timezone.utc).isoformat(),
        "artifacts": artifacts, "script_sha256": digest(Path(__file__).resolve()),
        "environment": {"platform": platform.system(), "architecture": platform.machine(),
                        "kernel": platform.release(), "python": platform.python_version(),
                        "available_cpus": len(os.sched_getaffinity(0)),
                        "cpu_affinity": sorted(os.sched_getaffinity(0)),
                        "load_average_at_start": list(os.getloadavg()),
                        "network_namespace_isolated": True, "external_network_enabled": False},
        "configuration": {"routing": "order group with one local API provider and no affinity",
                          "usage": "enabled on both; persisted completed requests and 19 input/7 output tokens verified",
                          "privacy": "off in fresh configuration", "subscriptions": "disabled",
                          "gui": False, "tls": False, "concurrency": [1] if args.probe_only else [1, 4],
                          "requests_per_run": args.requests, "warmup_minimum": args.warmup,
                          "rounds": args.rounds, "probe_only": args.probe_only,
                          "sse_chunk_delay_ms": args.sse_chunk_delay_ms,
                          "memory_sample_interval_ms": 10, "execution_budget_seconds": args.budget_seconds,
                          "fresh_downstream_connection_per_request": True, "upstream_tcp_nodelay": True},
        "notes": [
            "This comparison covers JSON/SSE local API routing and usage recording, not full Magpie feature parity.",
            "Both variants use new temporary HOME/XDG directories and the same mock in a fresh network namespace.",
            "Magpie background external catalog/update attempts are blocked by the network namespace.",
            "Payloads are built outside timing; responses preserve fixed text and token counts with unique request IDs.",
            "SSE TTFT is the first nonempty content delta; JSON text availability waits for the complete JSON body.",
            "Latencies include the Python client/mock and their scheduling, but exclude executor queue time.",
            "Daemon/descendant RSS excludes the benchmark driver/mock and sums shared mappings, rather than PSS.",
            "10 ms sampling may miss short peaks; daemon HWM includes startup, warm-up and measured requests.",
            "Magpie model-list probe records are excluded from generation usage totals and counted separately.",
            "Summary medians describe per-run percentiles; small samples/shared load do not establish broad superiority.",
        ], "results": [],
    }
    upstream = Upstream(args.sse_chunk_delay_ms / 1000)
    stop_reason = None
    try:
        for round_index in range(args.rounds):
            order = ("magpie", "zcalld") if round_index % 2 == 0 else ("zcalld", "magpie")
            for streaming in (False, True):
                for concurrency in ([1] if args.probe_only else [1, 4]):
                    for variant in order:
                        if time.monotonic() >= deadline:
                            stop_reason = "execution_budget_exhausted"
                            break
                        print(f"round {round_index + 1}/{args.rounds}: {variant}, "
                              f"{'SSE' if streaming else 'JSON'}, concurrency {concurrency}", flush=True)
                        result = run_case(variant, artifacts, upstream, payloads[streaming], streaming,
                                          concurrency, args, deadline)
                        result.update(round=round_index + 1, pair_order=list(order))
                        report["results"].append(result)
                        save_report({**report, "status": "running"}, args.output)
                        if result["status"] != "passed":
                            stop_reason = "case_failed"
                            break
                    if stop_reason:
                        break
                if stop_reason:
                    break
            if stop_reason:
                break
    finally:
        upstream.close()
    changed = []
    for variant, identity in artifacts.items():
        for field in ("binary", "routing_plugin"):
            if field not in identity:
                continue
            try:
                if digest(Path(identity[field])) != identity[field + "_sha256"]:
                    changed.append(f"{variant}.{field}")
            except OSError:
                changed.append(f"{variant}.{field}")
    if changed:
        stop_reason = "artifacts_changed_during_measurement"
    report.update(status="passed" if stop_reason is None else "incomplete", stop_reason=stop_reason,
                  artifact_identity_unchanged=not changed, changed_artifacts=changed,
                  elapsed_seconds=round(time.monotonic() - started, 3), completed_runs=len(report["results"]),
                  planned_runs=args.rounds * (4 if args.probe_only else 8), summary=summarize(report["results"]))
    serialized = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(serialized, encoding="utf-8")
    print(serialized, end="")
    return 0 if stop_reason is None else 1


if __name__ == "__main__":
    raise SystemExit(benchmark_entry(main))
