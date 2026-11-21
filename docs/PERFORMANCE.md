# Performance evidence

The [2026-11-22 iteration report](validation/2026-11-22-magpie-iteration.md) compares the native gateway with Magpie on matched local JSON/SSE routing and measures large-request rewrites against the previous core. It records exact artifacts, accounting checks, process-tree memory and workload limits.

## Historical reference: zcalld 0.1.0

> These are historical **0.1.0 minimal-daemon** measurements. For the installed
> **0.2.4 gateway with 0.1.5-2 compatibility workers**, see the
> [2026-11-20 deployment acceptance](DEPLOYMENT-2026-11-20.md), which reports the
> complete gateway process tree, the separate GUI and swap accounting. The
> approximately 10 MiB baseline below does not describe that compatibility deployment.

Six runs completed **30,000 of 30,000 measured gateway requests successfully**.
Across three runs per configuration, throughput medians were **2,210.08 requests/s**
with no plugins and **2,379.19 requests/s** with aliases and policy enabled.
The observed throughput ranges overlap; these measurements describe this workload
on a shared development machine and do not establish a plugin speedup or penalty.

The tested core is [797ac0e](https://github.com/LIghtJUNction/zcalld/commit/797ac0efac45dfcda589f64dc108ece76acb69f0).
All six runs used the same 1,174,304-byte release executable, approximately
1.12 MiB. Measurements were recorded on **2026-11-20, 03:09–03:13 Asia/Taipei**
(19:09–19:13 UTC on November 19).

## Results

Each cell gives the median across three runs, followed by the observed
minimum–maximum in parentheses. Latency rows summarize each run's percentile;
they are not percentiles of a pooled 15,000-request sample.

| Metric | No plugins | Aliases + policy |
| --- | ---: | ---: |
| Throughput, requests/s | 2,210.08 (1,996.48–2,334.87) | 2,379.19 (1,689.42–2,500.78) |
| p50 latency, ms | 1.6021 (1.5666–1.6479) | 1.5522 (1.5371–2.0546) |
| p95 latency, ms | 3.6091 (2.4729–4.1393) | 2.8218 (2.3041–4.2286) |
| p99 latency, ms | 5.2506 (2.9850–5.4215) | 4.0207 (2.8702–9.2050) |
| Idle RSS, KiB | 9,920 (9,784–10,004) | 10,584 (10,548–10,660) |
| Sampled peak RSS, KiB | 13,064 (13,024–13,188) | 13,704 (13,652–13,752) |
| Process creation to healthy response, ms | 11.371 (10.920–12.147) | 15.416 (10.944–26.747) |
| Successful measured requests | 15,000 / 15,000 | 15,000 / 15,000 |
| Observed daemon threads | 5 | 5 |
| Observed child processes | 0 | 0 |

Median idle memory was **9.69 MiB** without plugins and **10.34 MiB** with both
plugins. Median sampled peak memory was **12.76 MiB** and **13.38 MiB** respectively.
RSS comes from the daemon's Linux `/proc` status and includes its mapped shared
libraries. Thread and child counts were observed at idle, after warm-up, and after
the measured workload. Sampling occurs every 10 ms; short memory spikes may fall
between samples.

## Every run and its raw evidence

| Run | Configuration | Requests/s | p50, ms | Idle RSS, KiB | Sampled peak RSS, KiB | Raw result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| 1 | No plugins | 2,334.87 | 1.6479 | 10,004 | 13,188 | [baseline-1.json](benchmarks/baseline-1.json) |
| 1 | Aliases + policy | 1,689.42 | 2.0546 | 10,660 | 13,704 | [plugins-1.json](benchmarks/plugins-1.json) |
| 2 | No plugins | 2,210.08 | 1.5666 | 9,784 | 13,024 | [baseline-2.json](benchmarks/baseline-2.json) |
| 2 | Aliases + policy | 2,500.78 | 1.5371 | 10,584 | 13,752 | [plugins-2.json](benchmarks/plugins-2.json) |
| 3 | No plugins | 1,996.48 | 1.6021 | 9,920 | 13,064 | [baseline-3.json](benchmarks/baseline-3.json) |
| 3 | Aliases + policy | 2,379.19 | 1.5522 | 10,548 | 13,652 | [plugins-3.json](benchmarks/plugins-3.json) |

The initial pair showed lower throughput with plugins. In both later pairs, the
plugin run had higher throughput than the corresponding baseline. The direct
mock reference also varied substantially: 1,045.71–1,724.30 requests/s across the
baseline runs and 1,082.96–1,494.55 requests/s across the plugin runs. These
observations support reporting run variation alongside the medians. They do not
isolate its causes or the execution cost of individual hooks.

## Workload and measurement

All runs used Linux x86_64, kernel `7.3.4-arch1-1`, Python `3.14.10`, and 24
available logical CPUs. The machine was shared and the benchmark was not pinned
to CPUs. Follow-up runs executed sequentially in the order baseline 2, plugins 2,
baseline 3, plugins 3, without concurrent builds by this task.

Every run used 4 gateway workers, 4 concurrent clients, queue capacity 16,
11 configured routes, 100 warm-up requests, and 5,000 measured requests. Warm-up
requests are excluded from the success totals above. The plugin configuration
loads the native aliases and policy libraries on each worker. Its public model
`fast` maps to `local`; both configurations select the same mock upstream model.
Payloads are 79 bytes for the baseline and 78 bytes for the alias configuration.

The [benchmark client](../scripts/benchmark.py) creates a new downstream HTTP
connection for each request. The gateway reuses upstream connections per worker.
The [mock](../scripts/integration.py) returns a deterministic small JSON response
over loopback HTTP, with TCP_NODELAY enabled. There is no TLS handshake, network
provider call, or model generation in this workload. Latency includes local
HTTP processing, mock work, and client scheduling; it begins when an executor
worker starts the request and excludes time waiting in the executor queue.
Startup measurements include process creation and a health poll at 10 ms intervals.

Each run also sends 5,000 requests directly to the same mock. All 30,000 direct
reference requests succeeded. That path creates a fresh mock connection for
every request, while the gateway path pools its upstream connections. The
threaded Python mock creates a handler thread per connection. These different
connection topologies explain why the pooled gateway path can outperform the
fresh-connection reference. Ratios between them do not isolate gateway overhead.

The figures cover this small, non-streaming request workload. They do not measure
maximum capacity, TLS/provider latency, long SSE sessions, large conversation
bodies, or cross-protocol conversion cost. The HTTP integration suite exercises
streaming and conversion correctness separately.

## Reproduce

Build the workspace, then alternate the two configurations:

```sh
cargo build --release --workspace --locked
python3 scripts/benchmark.py --requests 5000 --warmup 100 \
  --workers 4 --concurrency 4 --output docs/benchmarks/baseline-new.json
python3 scripts/benchmark.py --requests 5000 --warmup 100 \
  --workers 4 --concurrency 4 --with-plugins \
  --output docs/benchmarks/plugins-new.json
```

The script owns its temporary configuration, mock, daemon, and local test
credentials; it stops the daemon and removes the temporary files after the run.
Keep raw JSON results with any published summary so the workload and observed
variation remain inspectable.
