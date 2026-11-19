#!/usr/bin/env python3
"""Measure a release zcalld process against a local deterministic mock upstream."""

from __future__ import annotations

import argparse
import concurrent.futures
from datetime import datetime, timezone
import http.client
import json
import math
import os
from pathlib import Path
import platform
import statistics
import threading
import time

import integration


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, math.ceil(len(ordered) * fraction) - 1))]


def process_metrics(pid: int) -> dict:
    status = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        name, _, value = line.partition(":")
        status[name] = value.strip()
    children_file = Path(f"/proc/{pid}/task/{pid}/children")
    children = children_file.read_text().split() if children_file.exists() else []
    return {
        "rss_kib": int(status.get("VmRSS", "0 kB").split()[0]),
        "rss_high_water_kib": int(status.get("VmHWM", "0 kB").split()[0]),
        "threads": int(status.get("Threads", "0")),
        "open_fds": len(list(Path(f"/proc/{pid}/fd").iterdir())),
        "child_processes": len(children),
    }


def latency_summary(values: list[float]) -> dict:
    return {
        "min_ms": round(min(values), 4),
        "mean_ms": round(statistics.fmean(values), 4),
        "p50_ms": round(percentile(values, 0.50), 4),
        "p95_ms": round(percentile(values, 0.95), 4),
        "p99_ms": round(percentile(values, 0.99), 4),
        "max_ms": round(max(values), 4),
    }


def request_once(port: int, payload: bytes, authenticated: bool) -> float:
    headers = {"Content-Type": "application/json"}
    if authenticated:
        headers["Authorization"] = f"Bearer {integration.LOCAL_TOKEN}"
    started = time.perf_counter_ns()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request("POST", "/v1/chat/completions", payload, headers)
        response = connection.getresponse()
        body = response.read()
        if response.status != 200:
            raise RuntimeError(f"benchmark request returned HTTP {response.status}")
        decoded = json.loads(body)
        if decoded["choices"][0]["message"]["content"] != "hello ✓":
            raise RuntimeError("benchmark response content did not match the mock")
    finally:
        connection.close()
    return (time.perf_counter_ns() - started) / 1_000_000


def workload(port: int, payload: bytes, count: int, concurrency: int, authenticated: bool) -> dict:
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        started = time.perf_counter()
        futures = [executor.submit(request_once, port, payload, authenticated) for _ in range(count)]
        latencies = [future.result() for future in futures]
        elapsed = time.perf_counter() - started
    return {
        "requests": count,
        "successful_requests": len(latencies),
        "concurrency": concurrency,
        "elapsed_seconds": round(elapsed, 6),
        "requests_per_second": round(count / elapsed, 2),
        "latency": latency_summary(latencies),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/zcalld")
    parser.add_argument("--binary", dest="binary_option")
    parser.add_argument("--requests", type=int, default=300)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--with-plugins", action="store_true")
    parser.add_argument("--plugin-dir", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not (1 <= args.requests <= 1_000_000 and 1 <= args.concurrency <= 256
            and 1 <= args.workers <= 128 and 0 <= args.warmup <= 10000):
        parser.error("invalid workload size, concurrency, worker count or warmup")
    binary = Path(args.binary_option or args.binary).resolve()
    if not binary.is_file():
        parser.error("build a release zcalld binary before benchmarking")
    integration.PLUGIN_DIR = args.plugin_dir
    upstream = integration.MockUpstream()
    gateway = integration.Gateway(
        binary, upstream.url, plugins=args.with_plugins, workers=args.workers,
        queue=max(16, args.concurrency * 2),
    )
    samples: list[int] = []
    stop_sampling = threading.Event()
    sampler: threading.Thread | None = None
    try:
        gateway.start()
        pid = gateway.process.pid
        idle = process_metrics(pid)
        public_model = "fast" if args.with_plugins else "local"
        payload = json.dumps(integration.post_body(public_model), separators=(",", ":")).encode()
        direct_payload = json.dumps(integration.post_body("upstream-model"),
                                    separators=(",", ":")).encode()
        for _ in range(args.warmup):
            request_once(gateway.port, payload, True)
        warm = process_metrics(pid)

        def sample_memory() -> None:
            while not stop_sampling.wait(0.01):
                try:
                    samples.append(process_metrics(pid)["rss_kib"])
                except (OSError, ValueError):
                    return

        sampler = threading.Thread(target=sample_memory, daemon=True)
        sampler.start()
        through_gateway = workload(gateway.port, payload, args.requests, args.concurrency, True)
        stop_sampling.set()
        sampler.join(timeout=2)
        final = process_metrics(pid)
        direct = workload(upstream.server.server_port, direct_payload,
                          args.requests, args.concurrency, False)
        version = gateway.cli("version")
        if version.returncode != 0:
            raise RuntimeError("could not read gateway version")
        report = {
            "schema_version": 1,
            "measured_at_utc": datetime.now(timezone.utc).isoformat(),
            "version": version.stdout.strip(),
            "environment": {
                "platform": "Linux",
                "architecture": platform.machine(),
                "kernel": platform.release(),
                "python": platform.python_version(),
                "available_cpu_count": len(os.sched_getaffinity(0)),
            },
            "configuration": {
                "workers": args.workers,
                "queue": max(16, args.concurrency * 2),
                "plugins": ["aliases", "policy"] if args.with_plugins else [],
                "configured_routes": 11,
                "warmup_requests": args.warmup,
                "payload_bytes": len(payload),
                "new_downstream_connection_per_request": True,
                "mock_transport": "HTTP over loopback; no TLS or provider generation",
                "mock_tcp_nodelay": True,
            },
            "binary_bytes": binary.stat().st_size,
            "startup_to_health_ms": round(gateway.startup_ms, 3),
            "idle": idle,
            "after_warmup": warm,
            "after_workload": final,
            "sampled_peak_rss_kib": max([idle["rss_kib"], warm["rss_kib"], final["rss_kib"], *samples]),
            "gateway": through_gateway,
            "direct_mock_reference": direct,
            "notes": [
                "Measurements include loopback HTTP, the Python mock, and client scheduling.",
                "Latency starts when a benchmark worker begins a request; executor queue time is excluded.",
                "RSS is from the daemon's /proc status; child process count is reported explicitly.",
                "The direct mock reference uses the same client concurrency and fresh connections.",
                "No comparison with Magpie or real model-provider latency is implied.",
            ],
        }
        serialized = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(serialized)
        print(serialized, end="")
        if gateway.stop() != 0 or gateway.killed:
            raise RuntimeError("gateway did not stop cleanly after the benchmark")
    finally:
        stop_sampling.set()
        if sampler is not None:
            sampler.join(timeout=2)
        gateway.close()
        upstream.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
