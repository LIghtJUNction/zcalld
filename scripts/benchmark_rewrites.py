#!/usr/bin/env python3
"""Compare two real gateways rewriting large requests through the native router.

Only loopback fixtures are used. Payloads are prepared once, before timing, and
the same upstream checks every forwarded model and complete prompt. The default
workload alternates AB/BA and has a three-minute execution budget.
"""

from __future__ import annotations

import argparse
import concurrent.futures
from datetime import datetime, timezone
import hashlib
import http.client
import http.server
import json
import math
import os
from pathlib import Path
import platform
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time


TOKEN = "rewrite-benchmark-local-fixture"
UPSTREAM_MODEL = "rewrite-benchmark-upstream"
RESPONSE = json.dumps({
    "id": "rewrite-benchmark",
    "object": "chat.completion",
    "model": UPSTREAM_MODEL,
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                 "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 7, "completion_tokens": 1, "total_tokens": 8},
}, separators=(",", ":")).encode()
SAMPLE_SECONDS = 0.01


class BenchmarkParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        self.print_usage(sys.stderr)
        raise ValueError(message)


def save_report(report: dict, output: Path | None) -> None:
    if output is not None:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def benchmark_entry(main) -> int:
    """Persist setup/launcher errors as well as ordinary measured results."""
    output = None
    for index, value in enumerate(sys.argv[1:], 1):
        if value == "--output" and index + 1 < len(sys.argv):
            output = Path(sys.argv[index + 1])
        elif value.startswith("--output="):
            output = Path(value.partition("=")[2])
    report = {"schema_version": 1, "status": "starting", "results": [], "completed_runs": 0,
              "measured_at_utc": datetime.now(timezone.utc).isoformat()}
    initialized = False
    try:
        if not any(value in ("--help", "-h") for value in sys.argv[1:]):
            save_report(report, output)
            initialized = output is not None
        code = main()
        if code and initialized:
            current = json.loads(output.read_text(encoding="utf-8"))
            if current.get("status") == "starting":
                raise RuntimeError(f"benchmark launcher exited with code {code} before producing results")
        return code
    except (Exception, KeyboardInterrupt) as error:
        if initialized:
            try:
                report = json.loads(output.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                pass
        report.update(status="incomplete", stop_reason="setup_or_runtime_error",
                      error=f"{type(error).__name__}: {error}",
                      completed_runs=len(report.get("results", [])))
        try:
            save_report(report, output)
        except OSError as write_error:
            report["output_error"] = str(write_error)
        print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)
        return 130 if isinstance(error, KeyboardInterrupt) else 2


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fixture(size_mib: int) -> dict:
    length = size_mib * 1024 * 1024
    unit = "Rewrite benchmark: preserve every character. "
    text = (unit * ((length + len(unit) - 1) // len(unit)))[:length]
    marker = f"prompt-{size_mib}-mib"
    body = json.dumps({
        "model": "group/benchmark", "stream": False,
        "messages": [{"role": "user", "content": text}],
        "benchmark_marker": marker,
    }, separators=(",", ":")).encode()
    return {"payload": body, "prompt_bytes": length,
            "prompt_sha256": hashlib.sha256(text.encode()).hexdigest(),
            "marker": marker}


class Upstream(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.expected: dict = {}
        self.count = 0
        self.errors: list[str] = []
        self.thread = threading.Thread(target=self.serve_forever,
                                       kwargs={"poll_interval": 0.05}, daemon=True)
        self.thread.start()

    def begin(self, expected: dict) -> None:
        with self.lock:
            self.expected = expected
            self.count = 0
            self.errors = []

    def observed(self) -> dict:
        with self.lock:
            return {"requests": self.count, "errors": list(self.errors)}

    def close(self) -> None:
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.connection.settimeout(20)

    def log_message(self, *_: object) -> None:
        pass

    def do_POST(self) -> None:
        server: Upstream = self.server
        error = None
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if not 0 < length <= 16 * 1024 * 1024:
                raise ValueError("invalid upstream request size")
            body = json.loads(self.rfile.read(length))
            expected = server.expected
            if self.path != "/v1/chat/completions":
                raise ValueError("unexpected upstream path")
            if self.headers.get("Authorization") is not None:
                raise ValueError("caller authorization was forwarded")
            if body.get("model") != UPSTREAM_MODEL or body.get("stream") is not False:
                raise ValueError("routing model or stream mode mismatch")
            if body.get("benchmark_marker") != expected["marker"]:
                raise ValueError("provider extension was not preserved")
            messages = body.get("messages")
            if (not isinstance(messages, list) or len(messages) != 1
                    or messages[0].get("role") != "user"):
                raise ValueError("messages were not preserved")
            content = messages[0].get("content")
            if not isinstance(content, str):
                raise ValueError("prompt type mismatch")
            encoded = content.encode()
            if (len(encoded) != expected["prompt_bytes"]
                    or hashlib.sha256(encoded).hexdigest() != expected["prompt_sha256"]):
                raise ValueError("forwarded prompt bytes changed")
        except (KeyError, TypeError, ValueError, OSError) as failure:
            error = str(failure)
        with server.lock:
            server.count += 1
            if error and len(server.errors) < 16:
                server.errors.append(error)
        data = RESPONSE if error is None else b'{"error":"benchmark validation failed"}'
        self.send_response(200 if error is None else 400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def process_tree(pid: int) -> dict:
    """Sum RSS for the daemon and descendants, including children of workers."""
    pending = [pid]
    seen = set()
    records = []
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        root = Path(f"/proc/{current}")
        try:
            status = dict(line.split(":", 1) for line in (root / "status").read_text().splitlines())
            record = {"pid": current, "rss_kib": int(status.get("VmRSS", "0 kB").split()[0]),
                      "hwm_kib": int(status.get("VmHWM", "0 kB").split()[0]),
                      "swap_kib": int(status.get("VmSwap", "0 kB").split()[0]),
                      "threads": int(status.get("Threads", "0"))}
            records.append(record)
            for children in (root / "task").glob("*/children"):
                try:
                    pending.extend(int(value) for value in children.read_text().split())
                except FileNotFoundError:
                    pass
        except (FileNotFoundError, ProcessLookupError):
            if current == pid:
                raise RuntimeError("gateway exited during memory sampling")
    daemon = next(record for record in records if record["pid"] == pid)
    return {"daemon_rss_kib": daemon["rss_kib"], "daemon_hwm_kib": daemon["hwm_kib"],
            "tree_rss_kib": sum(record["rss_kib"] for record in records),
            "tree_swap_kib": sum(record["swap_kib"] for record in records),
            "processes": len(records), "threads": sum(record["threads"] for record in records)}


class MemorySampler:
    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.stop_event = threading.Event()
        self.peak = {"daemon_rss_kib": 0, "tree_rss_kib": 0,
                     "tree_swap_kib": 0, "processes": 0, "threads": 0}
        self.samples = 0
        self.errors = 0
        self.thread = threading.Thread(target=self.run, daemon=True)

    def observe(self, snapshot: dict) -> None:
        for name in self.peak:
            self.peak[name] = max(self.peak[name], snapshot[name])
        self.samples += 1

    def run(self) -> None:
        while not self.stop_event.wait(SAMPLE_SECONDS):
            try:
                self.observe(process_tree(self.pid))
            except (OSError, ValueError, RuntimeError):
                self.errors += 1
                return

    def stop(self) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)


class Gateway:
    def __init__(self, binary: Path, plugin: Path, upstream: int, workers: int) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="zcalld-rewrite-benchmark-")
        self.root = Path(self.temporary.name)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        groups = self.root / "benchmark-routing.json"
        groups.write_text(json.dumps({"groups": [{
            "name": "group/benchmark", "mode": "order", "members": [{"model": "local"}],
            "original": {"affinity": "none", "effort": "auto"},
        }]}), encoding="utf-8")
        config = self.root / "config.conf"
        config.write_text(
            f"[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers={workers}\n"
            "queue=16\nmax_body_bytes=16777216\nmax_response_bytes=65536\n"
            "request_timeout_ms=15000\nupstream_timeout_ms=15000\nconnect_timeout_ms=1000\n"
            "token_env=ZCALLD_BENCH_TOKEN\n"
            f"[provider mock]\nbase_url=http://127.0.0.1:{upstream}/v1\nprotocol=chat\n"
            f"[route local]\nprovider=mock\nmodel={UPSTREAM_MODEL}\n"
            f"[route group/benchmark]\nprovider=mock\nmodel={UPSTREAM_MODEL}\n"
            f"[plugin routing]\npath={plugin}\nconfig={groups}\n", encoding="utf-8")
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith(("ZCALLD_", "CODEX_", "XDG_"))
                       and key.lower() not in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
                       and key != "NOTIFY_SOCKET"}
        environment.update(HOME=str(self.root), USERPROFILE=str(self.root),
                           XDG_CONFIG_HOME=str(self.root / ".config"),
                           APPDATA=str(self.root / "AppData/Roaming"),
                           NO_PROXY="127.0.0.1,localhost", ZCALLD_BENCH_TOKEN=TOKEN)
        self.log = tempfile.TemporaryFile()
        self.process = subprocess.Popen([str(binary), "serve", "--config", str(config)],
                                        stdout=self.log, stderr=self.log, env=environment)
        self.startup_ms = 0.0

    def start(self, deadline: float) -> None:
        started = time.monotonic()
        while time.monotonic() < min(deadline, started + 10):
            if self.process.poll() is not None:
                break
            connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=0.2)
            try:
                connection.request("GET", "/healthz")
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
        raise RuntimeError("gateway did not become healthy: " + self.log.read()[-2000:].decode(errors="replace"))

    def close(self) -> dict:
        killed = False
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                killed = True
                self.process.kill()
                self.process.wait(timeout=2)
        result = {"exit_code": self.process.returncode, "killed": killed}
        self.log.close()
        self.temporary.cleanup()
        return result


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def request_once(port: int, payload: bytes, deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("benchmark execution budget exhausted")
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=min(15, remaining))
    started = time.perf_counter_ns()
    try:
        connection.request("POST", "/v1/chat/completions", payload,
                           {"Content-Type": "application/json", "Authorization": f"Bearer {TOKEN}"})
        response = connection.getresponse()
        data = response.read(65537)
        if response.status != 200:
            raise RuntimeError(f"gateway returned HTTP {response.status}: {data[:200]!r}")
        if data != RESPONSE:
            raise RuntimeError("gateway response differs from the fixed upstream response")
    finally:
        connection.close()
    return (time.perf_counter_ns() - started) / 1_000_000


def workload(port: int, payload: bytes, requests: int, concurrency: int, deadline: float) -> dict:
    barrier = threading.Barrier(concurrency + 1)

    def worker(index: int) -> tuple[list[float], list[str]]:
        values, errors = [], []
        barrier.wait()
        for _ in range(index, requests, concurrency):
            try:
                values.append(request_once(port, payload, deadline))
            except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
                errors.append(str(error))
                break
        return values, errors

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [executor.submit(worker, index) for index in range(concurrency)]
        started = time.perf_counter()
        barrier.wait()
        parts = [future.result() for future in futures]
        elapsed = time.perf_counter() - started
    latencies = [value for values, _ in parts for value in values]
    errors = [error for _, errors in parts for error in errors]
    result = {"requests": requests, "successful_requests": len(latencies), "errors": errors,
              "elapsed_seconds": round(elapsed, 6),
              "requests_per_second": round(len(latencies) / elapsed, 3),
              "latencies_ms": [round(value, 4) for value in latencies]}
    if latencies:
        result["latency"] = {"p50_ms": round(percentile(latencies, 0.5), 4),
                             "p95_ms": round(percentile(latencies, 0.95), 4),
                             "mean_ms": round(statistics.fmean(latencies), 4),
                             "min_ms": round(min(latencies), 4), "max_ms": round(max(latencies), 4)}
    return result


def run_case(binary: Path, plugin: Path, upstream: Upstream, payload: dict,
             concurrency: int, args: argparse.Namespace, deadline: float) -> dict:
    upstream.begin(payload)
    gateway = Gateway(binary, plugin, upstream.server_port, concurrency)
    sampler = None
    result = {"status": "failed", "workers": concurrency, "concurrency": concurrency,
              "prompt_bytes": payload["prompt_bytes"], "payload_bytes": len(payload["payload"]),
              "payload_sha256": hashlib.sha256(payload["payload"]).hexdigest()}
    try:
        gateway.start(deadline)
        result["startup_to_health_ms"] = round(gateway.startup_ms, 3)
        result["idle"] = process_tree(gateway.process.pid)
        warmup = max(args.warmup, concurrency)
        result["warmup"] = workload(gateway.port, payload["payload"], warmup, concurrency, deadline)
        if result["warmup"]["successful_requests"] != warmup:
            raise RuntimeError("warm-up requests failed")
        result["after_warmup"] = process_tree(gateway.process.pid)
        sampler = MemorySampler(gateway.process.pid)
        sampler.observe(result["after_warmup"])
        sampler.thread.start()
        result["workload"] = workload(gateway.port, payload["payload"], args.requests, concurrency, deadline)
        sampler.stop()
        result["after_workload"] = process_tree(gateway.process.pid)
        sampler.observe(result["after_workload"])
        result["measured_peak"] = {**sampler.peak, "samples": sampler.samples,
                                   "sampling_errors": sampler.errors}
        result["upstream"] = upstream.observed()
        if result["workload"]["successful_requests"] != args.requests:
            raise RuntimeError("measured requests failed")
        if result["upstream"] != {"requests": warmup + args.requests, "errors": []}:
            raise RuntimeError("upstream request count or payload validation failed")
        if sampler.errors:
            raise RuntimeError("memory sampling did not finish successfully")
        result["status"] = "passed"
    except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
        result["error"] = str(error)
        result["upstream"] = upstream.observed()
    finally:
        if sampler is not None and sampler.thread.is_alive():
            sampler.stop()
        result["shutdown"] = gateway.close()
        if result["shutdown"] != {"exit_code": 0, "killed": False}:
            result["status"] = "failed"
            result.setdefault("error", "gateway did not stop cleanly")
    return result


def summarize(results: list[dict]) -> list[dict]:
    summaries = []
    for size in (1, 4):
        for concurrency in (1, 4):
            for variant in ("baseline", "candidate"):
                runs = [run for run in results if run["status"] == "passed"
                        and (run["size_mib"], run["concurrency"], run["variant"]) == (size, concurrency, variant)]
                if not runs:
                    continue
                metrics = {
                    "requests_per_second": [run["workload"]["requests_per_second"] for run in runs],
                    "p50_ms": [run["workload"]["latency"]["p50_ms"] for run in runs],
                    "p95_ms": [run["workload"]["latency"]["p95_ms"] for run in runs],
                    "peak_tree_rss_kib": [run["measured_peak"]["tree_rss_kib"] for run in runs],
                    "peak_daemon_rss_kib": [run["measured_peak"]["daemon_rss_kib"] for run in runs],
                    "daemon_hwm_kib": [run["after_workload"]["daemon_hwm_kib"] for run in runs],
                }
                summaries.append({"size_mib": size, "concurrency": concurrency, "variant": variant,
                                  "rounds": len(runs), "metrics": {
                                      name: {"median": round(statistics.median(values), 4),
                                             "min": min(values), "max": max(values)}
                                      for name, values in metrics.items()}})
    return summaries


def binary_identity(binary: Path, plugin_dir: Path) -> dict:
    plugin = plugin_dir / "libzcalld_plugin_routing.so"
    for path in (binary, plugin):
        if not path.is_file():
            raise ValueError(f"required artifact does not exist: {path}")
    version = subprocess.run([str(binary), "version"], text=True, capture_output=True,
                             timeout=5, check=True).stdout.strip()
    return {"binary": str(binary), "binary_sha256": digest(binary), "binary_bytes": binary.stat().st_size,
            "version": version, "routing_plugin": str(plugin), "routing_plugin_sha256": digest(plugin),
            "routing_plugin_bytes": plugin.stat().st_size}


def main() -> int:
    parser = BenchmarkParser(description=__doc__)
    for variant in ("baseline", "candidate"):
        parser.add_argument(f"--{variant}-bin", required=True, type=lambda value: Path(value).resolve())
        parser.add_argument(f"--{variant}-plugin-dir", required=True, type=lambda value: Path(value).resolve())
    parser.add_argument("--requests", type=int, default=12, help="measured requests per variant/scenario/round")
    parser.add_argument("--warmup", type=int, default=2, help="warm-up request count, at least concurrency")
    parser.add_argument("--rounds", type=int, default=2, help="round 1 is AB, round 2 is BA, then repeat")
    parser.add_argument("--budget-seconds", type=float, default=180,
                        help="stop scheduling cases after this monotonic execution budget")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux" or not Path("/proc/self/status").exists():
        parser.error("this benchmark uses Linux /proc for daemon and process-tree memory")
    if not (4 <= args.requests <= 10000 and 0 <= args.warmup <= 1000
            and 1 <= args.rounds <= 20 and 1 <= args.budget_seconds <= 3600):
        parser.error("invalid request count, warm-up count, rounds or execution budget")
    identities = {}
    try:
        for variant in ("baseline", "candidate"):
            identities[variant] = binary_identity(getattr(args, variant + "_bin"),
                                                   getattr(args, variant + "_plugin_dir"))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.error(str(error))
    fixtures = {size: fixture(size) for size in (1, 4)}
    started = time.monotonic()
    deadline = started + args.budget_seconds
    report = {
        "schema_version": 1, "measured_at_utc": datetime.now(timezone.utc).isoformat(),
        "script_sha256": digest(Path(__file__).resolve()), "artifacts": identities,
        "environment": {"platform": platform.system(), "architecture": platform.machine(),
                        "kernel": platform.release(), "python": platform.python_version(),
                        "available_cpus": len(os.sched_getaffinity(0)),
                        "cpu_affinity": sorted(os.sched_getaffinity(0)),
                        "load_average_at_start": list(os.getloadavg())},
        "configuration": {"sizes_mib": [1, 4], "concurrency": [1, 4], "workers": "equal to concurrency",
                          "requests_per_run": args.requests, "warmup_minimum": args.warmup,
                          "rounds": args.rounds, "queue": 16, "memory_sample_interval_ms": 10,
                          "execution_budget_seconds": args.budget_seconds,
                          "plugins": ["native routing: order, single member, no classifier"],
                          "response_bytes": len(RESPONSE), "new_downstream_connection_per_request": True},
        "notes": [
            "This measures local HTTP proxying with native request rewriting, not a remote model or TLS.",
            "Payload bytes are prepared once per size outside timing; the same mock validates full prompt hashes.",
            "Latency includes Python client/mock scheduling and validation; executor queue time is excluded.",
            "Only daemon/descendant RSS is counted; benchmark driver and mock memory are excluded.",
            "Process-tree RSS sums shared mappings in each process; it is not proportional set size (PSS).",
            "Sampled peaks can miss sub-10 ms spikes; daemon VmHWM covers startup, warm-up and measurement.",
            "Each percentile is per run; summary medians do not pool latency samples across rounds.",
            "Small samples and shared-machine load limit conclusions; no significance or overall product superiority is claimed.",
        ], "results": [],
    }
    upstream = Upstream()
    stop_reason = None
    try:
        for round_index in range(args.rounds):
            order = ("baseline", "candidate") if round_index % 2 == 0 else ("candidate", "baseline")
            for size in (1, 4):
                for concurrency in (1, 4):
                    for variant in order:
                        if time.monotonic() >= deadline:
                            stop_reason = "execution_budget_exhausted"
                            break
                        print(f"round {round_index + 1}/{args.rounds}: {variant}, {size} MiB, concurrency {concurrency}",
                              flush=True)
                        identity = identities[variant]
                        result = run_case(Path(identity["binary"]), Path(identity["routing_plugin"]),
                                          upstream, fixtures[size], concurrency, args, deadline)
                        result.update(variant=variant, size_mib=size, round=round_index + 1,
                                      pair_order=list(order))
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
    for variant, identity in identities.items():
        for field in ("binary", "routing_plugin"):
            try:
                if digest(Path(identity[field])) != identity[field + "_sha256"]:
                    changed.append(f"{variant}.{field}")
            except OSError:
                changed.append(f"{variant}.{field}")
    if changed:
        stop_reason = "artifacts_changed_during_measurement"
    report["artifact_identity_unchanged"] = not changed
    report["changed_artifacts"] = changed
    report.update(status="passed" if stop_reason is None else "incomplete",
                  stop_reason=stop_reason, elapsed_seconds=round(time.monotonic() - started, 3),
                  planned_runs=args.rounds * 8, completed_runs=len(report["results"]),
                  summary=summarize(report["results"]))
    serialized = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(serialized, encoding="utf-8")
    print(serialized, end="")
    return 0 if stop_reason is None else 1


if __name__ == "__main__":
    raise SystemExit(benchmark_entry(main))
