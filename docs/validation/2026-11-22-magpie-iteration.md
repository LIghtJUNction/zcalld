# Native gateway and GUI iteration — 2026-11-22

This iteration adds Git skill fetch/update, accurate bounded usage collection and native recording controls, updates the GUI dependency, and reduces request-hook copying. It passed the native checks below and two four-round comparisons. The [Magpie alignment matrix](../MAGPIE_PARITY.md) records the remaining product gaps; this report does not establish complete feature parity or overall product superiority.

Dates use Asia/Taipei for the report title; raw reports retain UTC measurement timestamps. The core baseline is `d22ccce`; the candidate is the source revision containing this report. The companion GUI revision is [`daa540307573`](https://github.com/LIghtJUNction/zcall-gui/commit/daa540307573d86838b108f4a76aa45230fca5f5). Tested release binaries were copied out of the shared Cargo target before execution and their SHA-256 values were checked again after both benchmarks.

## Delivered changes

- **Usage:** compact and fragmented SSE, Anthropic `message_start`, long JSON, cache reads/writes and reasoning details. The collector retains bounded usage objects instead of response text. Native enable/disable validates the plugin and output destination before saving, works without groups, preserves history, and avoids duplicate explicit/automatic observer loading.
- **GUI:** aggregate Usage with saved recording state, enable/disable, separate confirmed restart, refresh and clear loading/error/empty/older-CLI states. Counts are labelled upstream attempts because retries are recorded separately. Provider search matches names, endpoints, models and protocols without cloning the complete provider collection on each paint.
- **Git Skills:** selected repository subtree and ref, recorded commit, binary and executable-bit preservation, remembered updates, and conflict refusal. Git objects are read without a checkout or execution. The tests protect files and registry edits made during a blocked fetch, including with `--force`. File/directory type changes remain an explicit [transaction boundary](../LIBRARY.md).
- **Request memory:** changed hooks transfer owned buffers and reuse scratch space. Unchanged hooks retain the borrowed input. Matching the routing hook's initial headroom avoids a retry that previously grew the full request allocation; a shrinking final transform releases excess retained capacity.

## Native verification

| Check | Result |
| --- | --- |
| Core workspace Rust tests | 183 passed; 0 failed; 0 ignored |
| GUI Rust tests | 43 passed; 0 failed; 0 ignored |
| Core and GUI formatting / Clippy | Passed; all targets, warnings denied |
| Optimized core workspace and GUI builds | Passed, locked dependency builds |
| Library / control / usage / gateway / privacy-routing fixtures | 113 passed |
| Usage state windows | Seven scenarios in zh-CN, English, zh-TW and Japanese: 28 passed |
| Recording interactions | Dark/light English plus light zh-TW and dark Japanese: enable, cancel/confirm restart, clear pending, disable and retained history passed |
| Native navigation | Seven pages, provider/agent workflows and theme persistence passed |

The gateway and CLI tests use real native binaries with temporary configuration and loopback upstreams. The privacy-routing chain uses a frozen installed privacy worker. UI tests use the native window and a stateful synthetic CLI; they verify argument flow and interaction state without restarting the user's service. All four locale catalogs have 282 matching keys. The language checks exercise dark Simplified Chinese, light English, light Traditional Chinese and dark Japanese; English recording interactions cover both themes. Xvfb runs are sequential to avoid display allocation races. Closing the owned window can make xdotool's remaining key-release event report `BadWindow`; the test separately requires a timely native exit code 0 and normal renderer completion. No production service was switched and no live model was called.

The [acceptance JSON](../benchmarks/iteration-20261122-acceptance.json) records commands' outcomes, test totals, GUI checks, dependency identities and native artifact hashes. Screenshots and executable artifacts remain outside Git.

## Matched Magpie comparison

Reference installation: **Magpie 0.1.2512**, with its exact executable hash below. Each variant uses one local API provider, a single-member order group, usage recording, no affinity, no privacy worker, no subscriptions, no GUI and no TLS. Both execute inside a fresh network namespace with external networking unavailable. The harness verifies upstream request content, downstream text and termination, and persisted **19 input / 7 output tokens per attempt**. Magpie's model-catalog probes are excluded from generation counts.

Each of four rounds alternates the variant order. Each variant and scenario has 20 warm-up and 400 measured requests per round; all **12,800 / 12,800 measured requests** succeeded across 32 runs. The workload uses fresh downstream connections, TCP_NODELAY upstream sockets, and configured 1 ms pauses after the first three SSE frames. zcalld's worker count equals client concurrency. This tests local gateway processing; it excludes remote-provider latency and maximum-capacity claims.

Cells report the median of four per-run values. Latency percentiles are computed per run and then summarized; samples are not pooled. Reported percentage changes compare the two variants' medians, rather than taking the median of paired percentage changes. Arrows run from Magpie to zcalld.

| Workload | Magpie requests/s | zcalld requests/s | p95 completion, ms | Peak process-tree RSS, MiB |
| --- | ---: | ---: | ---: | ---: |
| JSON, 1 client | 617.3 | 1,381.7 | 2.443 → 1.298 | 95.55 → 12.79 |
| JSON, 4 clients | 1,330.1 | 1,428.2 | 4.316 → 3.998 | 99.05 → 15.19 |
| SSE, 1 client | 192.0 | 238.6 | 6.668 → 4.679 | 96.26 → 12.75 |
| SSE, 4 clients | 677.5 | 809.6 | 7.294 → 6.466 | 99.30 → 15.13 |

For SSE, time to first token means arrival of the first nonempty text delta, not arrival of response headers:

| Concurrent clients | Magpie p95 first text, ms | zcalld p95 first text, ms |
| --- | ---: | ---: |
| 1 | 4.205 | 2.258 |
| 4 | 4.405 | 3.692 |

Four-round variation is material on a shared machine. These cells show median (minimum–maximum):

| Workload / variant | Requests/s | p95 completion, ms | Peak tree RSS, MiB |
| --- | ---: | ---: | ---: |
| JSON, 1 client, Magpie | 617.3 (497.2–740.6) | 2.443 (2.160–2.974) | 95.55 (94.92–95.92) |
| JSON, 1 client, zcalld | 1,381.7 (650.4–2,595.1) | 1.298 (0.751–2.859) | 12.79 (12.56–13.02) |
| JSON, 4 clients, Magpie | 1,330.1 (775.1–1,728.6) | 4.316 (3.395–10.145) | 99.05 (98.57–99.45) |
| JSON, 4 clients, zcalld | 1,428.2 (1,190.6–1,501.1) | 3.998 (3.718–11.190) | 15.19 (15.08–15.33) |
| SSE, 1 client, Magpie | 192.0 (173.3–209.0) | 6.668 (5.495–7.735) | 96.26 (95.90–96.59) |
| SSE, 1 client, zcalld | 238.6 (204.7–257.9) | 4.679 (4.238–6.747) | 12.75 (12.63–12.98) |
| SSE, 4 clients, Magpie | 677.5 (603.0–820.8) | 7.294 (5.783–8.495) | 99.30 (98.81–99.61) |
| SSE, 4 clients, zcalld | 809.6 (599.1–974.7) | 6.466 (4.684–18.324) | 15.13 (14.98–15.25) |

Raw evidence: [magpie-20261122.json](../benchmarks/magpie-20261122.json). The report includes individual timings, response/accounting checks, process counts, memory, environment and artifact identities. Detailed token fixtures cover same-protocol Chat, Responses, Anthropic and Gemini paths; complete cache/reasoning preservation through every protocol conversion remains unverified.

## Large-request rewrite comparison

The previous core and this candidate use the same single native routing plugin, mock, prompt hash and provider extensions. A noncanonical `benchmark-routing.json` filename gives both variants exactly one explicit plugin binding; the older core's automatic-binding duplication cannot inflate the baseline work. Sizes are approximately 1 MiB and 4 MiB. Each variant and scenario has four warm-up and 24 measured requests per round across four alternating rounds: all **768 / 768 measured requests** passed, with 32 complete runs.

Arrows run from the baseline to the candidate. The values are medians of four per-run measurements.

| Workload | Requests/s | p95 latency, ms | Peak tree RSS, MiB | Daemon HWM, MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 MiB, 1 client | 78.6 → 76.0 | 14.980 → 15.427 | 22.08 → 21.08 | 23.17 → 21.32 |
| 1 MiB, 4 clients | 183.6 → 178.2 | 25.289 → 25.212 | 45.00 → 37.17 | 48.71 → 45.11 |
| 4 MiB, 1 client | 17.3 → 19.8 | 67.958 → 57.781 | 52.46 → 48.40 | 52.26 → 48.70 |
| 4 MiB, 4 clients | 47.3 → 52.9 | 97.509 → 91.423 | 163.49 → 121.86 | 169.77 → 139.35 |

Raw evidence: [rewrites-20261122.json](../benchmarks/rewrites-20261122.json). This compares whole builds in a native request-rewriting workload. Other candidate changes mean the gains cannot be attributed to buffer reuse alone. It does not measure the complete compatibility-worker deployment or every plugin chain.

The 4 MiB cases show higher median throughput and lower observed memory. The 1 MiB cases have roughly 3% lower throughput medians despite lower memory, and the round ranges overlap. Reducing copies therefore does not establish a speedup for every request size; the tradeoff is retained here rather than selecting only favorable cases.

## Measurement limits

The host is shared Linux x86_64, kernel `7.3.5-arch1-1`, Python `3.14.10`, with 24 available logical CPUs. This task ran its benchmarks sequentially after its builds and UI tests; other host activity was not disabled. Starting load averages and CPU affinity are preserved in raw JSON. Client/mock work and scheduling are included in latency, but executor queue time is excluded.

RSS sums the daemon and descendants and excludes the Python driver/mock. Shared pages may be counted in more than one process; this is not PSS. Sampling occurs every 10 ms and can miss short peaks, while daemon HWM includes startup and warm-up. GUI, optional Bun/compatibility workers, privacy and remote-provider features need their own matching workload before comparing complete-product memory. The numbers are observations from these runs; no significance test or universal speed advantage is claimed.

## Dependency and artifact identity

The GUI dependency advanced from zgpui 0.1.6 through 0.1.7 to the current observed **0.1.8** source, including fixed-strike emoji sizing. Other internal dependency locks were checked against current default-branch heads and already matched. Development manifests retain canonical Git URLs without an internal release/version selector; locks and source gitlinks identify the observed revision.

| Dependency | Version | Exact source |
| --- | --- | --- |
| zcall | 0.4.2 | [`aaac54236f7c`](https://github.com/LIghtJUNction/zcall/commit/aaac54236f7ce0f4e1559c6e3b78d680d9dabf41) |
| zconv | 0.4.1 | [`f78b97b7065b`](https://github.com/LIghtJUNction/zconv/commit/f78b97b7065bbb7cf9ddfd0914d7f2296bbfe712) |
| zplugin | 0.3.1 | [`592a1be623b8`](https://github.com/LIghtJUNction/zplugin/commit/592a1be623b81a775026c470e6f0efe91fcf78f3) |
| zsys | 0.1.1 | [`808fcfdb7358`](https://github.com/LIghtJUNction/zsys/commit/808fcfdb73584ca7ca122e20b76f0209f56ab0f7) |
| zgpui | 0.1.8 | [`eeb909e956ed`](https://github.com/LIghtJUNction/zgpui/commit/eeb909e956ed0cff05e14525d4f29f5a78c553ec) |

| Executable | Reported version | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| Baseline core | zcalld 0.4.4 | 2,903,088 | `f99c11d3ae89f0d0431b51fe521e89eed4deb3402399c6d40da9882ab6f6ae34` |
| Candidate core | zcalld 0.4.4 | 2,983,488 | `f5a313c773e40b195e1094fa65875396ff0edf94943cdad73ec5a245c0294290` |
| Magpie | magpie 0.1.2512 | 71,381,880 | `d4e6f59b0c76e404317e2cb917d0e20ce4582ec8af8701c7bef63aab761a5159` |
| Candidate GUI | 0.1.6 | 1,051,104 | `e9f2517279d1232331edd827a82fdedca6e4814a4237cb78bb8222814c100123` |

Routing-plugin hashes and the privacy-worker/GUI identities are available in the raw evidence. Candidate package versions remain development versions; this source iteration does not create a release tag or replace the installed service.

## Reproduce

Run [benchmark_magpie.py](../../scripts/benchmark_magpie.py) and [benchmark_rewrites.py](../../scripts/benchmark_rewrites.py) against frozen release artifacts. Both scripts validate responses, stop on incomplete cases, checkpoint progress, bound execution time and verify artifact identity.

```sh
python3 scripts/benchmark_magpie.py --magpie-bin /usr/bin/magpie \
  --zcalld-bin /absolute/candidate/zcalld --zcalld-plugin-dir /absolute/candidate \
  --requests 400 --warmup 20 --rounds 4 --budget-seconds 180 --output magpie.json
python3 scripts/benchmark_rewrites.py \
  --baseline-bin /absolute/baseline/zcalld --baseline-plugin-dir /absolute/baseline \
  --candidate-bin /absolute/candidate/zcalld --candidate-plugin-dir /absolute/candidate \
  --requests 24 --warmup 4 --rounds 4 --budget-seconds 180 --output rewrites.json
```
