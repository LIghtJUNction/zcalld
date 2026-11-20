# Deployment acceptance — 20 November 2026

## Final deployed state

zcalld 0.2.4-1 and zcalld-compat 0.1.5-2 were installed on an Arch Linux system. The user service was **active, running, and enabled** at final acceptance. The recorded checks passed, and two controlled production requests completed successfully: one JSON response and one Server-Sent Events (SSE) response.

| Component | Version | Role |
|---|---|---|
| [zcalld](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.2.4) | 0.2.4-1 | CLI, gateway daemon, configuration, and native plugin interfaces |
| [zcalld-compat](https://github.com/LIghtJUNction/zcalld-compat/releases/tag/v0.1.5) | 0.1.5-2 | Optional compatibility helpers and runtime resources |
| [zcall-gui](https://github.com/LIghtJUNction/zcall-gui/releases/tag/v0.1.0) | 0.1.0 | Optional native graphical frontend |
| [zcall-omarchy](https://github.com/LIghtJUNction/zcall-omarchy/releases/tag/v0.1.2) | 0.1.2 | Optional Omarchy menu frontend |

Core and compatibility versions include the Arch package release. Frontend entries identify their application releases. The existing GUI showed a connected gateway and gateway version 0.2.4 during final acceptance. This records its technical state; visual design acceptance is outside this report.

The existing TCP listener was preserved at **0.0.0.0:3425**. A Unix domain socket was also available with **0600** permissions. TCP and Unix health checks, the catalog checks, and the recorded MCP metadata checks completed successfully.

The original XDG login autostart entry was backed up and disabled. The replacement user systemd service was enabled; this records the startup mechanisms handled by this transaction.

The cutover transaction took **5.2965 seconds** from its recorded start to completion. This measures the transaction, including its coordination and validation work. Exact client-visible unavailability was not separately measured.

The [machine-readable acceptance record](benchmarks/deployment-2026-11-20.json) contains the sanitized counts, measurements, package identities, and scope of these observations.

## Preserved configuration and state

The final audit compared the active deployment with the recorded migration baselines.

| Item | Verified result |
|---|---|
| Configured providers | 8 retained |
| Public model routes and catalog | 99 retained, including the full recorded catalog metadata |
| Routing groups | 14 retained |
| Accounts | 7 retained |
| Community packages | 4 retained |
| Existing agent files | All 8 retained their recorded state |
| Previously absent agent paths | All 8 remained absent |
| Existing native authentication owners | 4 recorded process identities remained unchanged |
| Externally managed account | Its external ownership was retained |
| Privacy rule settings | All 5 source fields preserved |
| Active privacy key | The same 32-byte key retained |
| Local model assets | All 4 files unchanged |
| Existing privacy record files | All 27 prior file prefixes preserved |
| Privacy checked counters | Monotonic at the final audit |
| Usage history | The complete 161,013,002-byte baseline prefix preserved |

The usage file had grown to 161,013,332 bytes at acceptance. Its retained baseline was still intact, and the old source remained unchanged after handover. Privacy history was allowed to receive new audit entries and counter updates while retaining the existing records.

Preservation checks establish that recorded configuration and state survived the migration. They do not establish that every configured account or model is currently available upstream. Native authentication ownership remained explicit; the gateway did not silently take over the retained external owner.

Machine-specific paths, process identifiers, account and provider labels, credentials, key fingerprints, and per-tool installation details are omitted from this public record.

## Actual request and MCP acceptance

Two small, fixed production requests were issued with **zero automatic retries**. Both returned HTTP 200, matched the expected short answer, used the expected response media type, and reached a completed state.

| Response form | HTTP | Application bytes read | First byte, ms | Total elapsed, ms | Completion |
|---|---:|---:|---:|---:|---|
| JSON | 200 | 2,142 | 7,026.78 | 7,026.90 | Expected answer; completed |
| SSE | 200 | 7,409 | 5,426.19 | 6,020.25 | Expected answer; completed |

The streaming response contained 10 parsed events, including one text-delta event. The first-byte measurement reports when the client received response data; it is not a measurement of upstream model time to first token. These durations include the complete request path and its network and upstream behavior.

This is a two-request correctness check. It does not cover all 8 configured providers, all 99 routes, every account, sustained traffic, tool execution, or maximum concurrency. The acceptance record does not assert a hard generated-token cap or a hard network-byte cap. Application bytes read are the measured quantity above.

The legacy MCP metadata checks completed initialization, ping, and tools/list using protocol version 2025-06-18. The directory returned **0 tools** at that observation. No production tools/call operation was performed. Separate installed-package fixtures cover the MCP implementation with controlled inputs.

## Resource observations

The following values are aggregate process-memory observations, in **KiB**. The gateway rows cover the sampled gateway process tree, including the child processes present at that time. The GUI row covers one existing GUI process separately.

| Observed scope | Processes | RSS | PSS | SwapPss | PSS + SwapPss |
|---|---:|---:|---:|---:|---:|
| Magpie application tree before handover | 5 | 252,868 | 209,245 | 281,888 | 491,133 |
| Configured gateway tree before the two production requests | 2 | 82,888 | 76,392 | 0 | 76,392 |
| Configured gateway tree after the two production requests | 3 | 182,828 | 173,599 | 0 | 173,599 |
| Existing optional GUI process at final observation | 1 | 23,664 | 16,938 | 10,832 | 27,770 |

**RSS** counts pages currently resident in memory. Summing RSS across processes can count shared pages more than once. **PSS** divides shared resident pages proportionally between processes. **SwapPss** accounts proportionally for pages in swap. A resident PSS value alone therefore does not describe all of a process's accounted memory.

The final column is the arithmetic sum of PSS and SwapPss for each individual observation. It is not a measurement of total system memory, kernel memory, graphics allocations, or other applications.

The three process-tree sampling windows lasted 4.0290, 4.0174, and 4.0329 seconds respectively. At 100 scheduler ticks per second, the recorded CPU increments for processes present throughout each window were 0, 2, and 2 ticks. No process appeared or disappeared within those individual sampling windows. These short observations do not establish a steady-state CPU utilization or capacity result.

The observations were taken at different times and in different states. The pre-request gateway had already received metadata checks; it is not an untouched cold-start measurement. The Magpie application tree and the gateway tree also have different frontend scopes. The GUI was sampled separately and excludes the daemon, compositor, and separately accounted GPU memory.

These were **not controlled load tests**. The rows are not combined into a simultaneous system total, and no performance-improvement percentage is derived from them. They describe the configured deployment and its active optional components, separately from earlier minimal-core measurements.

## Release and installed-package validation

The [formal 0.2.4 release run](https://github.com/LIghtJUNction/zcalld/actions/runs/44674414875) completed its five native platform jobs, publication, and AUR publication. The native targets were Linux x86_64, Linux aarch64, Windows x86_64, macOS aarch64, and macOS x86_64.

The Arch source package was built from the published source archive and the actual AUR recipe with dependency checking enabled and the complete makepkg check phase. That check passed **109 Rust tests and 19 HTTP integration tests**. Package inspection verified the installation layout, file modes, configuration backup entry, manual page, MIT license, system and user systemd units, and all five native plugin ABI entry points. The daemon and five plugin libraries had complete shared-library resolution and no RPATH or RUNPATH.

Acceptance against the installed packages passed **35 of 35 fixtures, with 0 skipped**:

| Installed suite | Passed |
|---|---:|
| Privacy | 3 |
| Routing | 9 |
| MCP | 14 |
| Builtin provider fetch | 9 |

Core namcap checks reported **0 errors and 0 warnings** for both the recipe and package. The compatibility recipe reported 0 errors and 0 warnings; its package reported **0 errors and 1 warning**. That warning says the zcalld dependency may be unnecessary. The dependency is intentionally retained: zcalld >= 0.2.3 is required to understand the compatibility helpers' Arch installation path. Static ELF dependency analysis cannot infer that configuration and upgrade requirement.

Compatibility executables and resources use **/usr/lib/zcalld-compat/**, with CLI symlinks under **/usr/bin/**. The installed core also recognizes the exact previous default helper paths when those old files are absent. This preserves existing bindings without replacing arbitrary custom helper paths.

## Libraries, optional components, and support boundaries

The pinned core dependency graph includes **zcall 0.3.0**, **zconv 0.4.0**, and **zplugin 0.3.1**, plus the project's plugin API and compatibility client. The pinned native systems support library is **zsys 0.1.1**. Their exact source revisions are recorded in the recursive source archive and the [release manifests](https://github.com/LIghtJUNction/zcalld/blob/b9bef29e56897cd05ce2b9587aedb51fefb00842/Cargo.toml).

On this Arch deployment, the core package requires glibc, libgcc, and curl. Optional compatibility features use separate Go helpers and, where enabled, a Bun-based community/provider host and local model assets. Enabling those features can add processes and memory; the resource table includes the child processes present in each gateway sample.

The GUI and Omarchy menu are separate frontends that call the existing CLI. The gateway does not require either frontend to serve requests. The GUI uses the project's pinned native UI library.

Live migration, systemd operation, Arch packaging, and the GUI observation described here were checked on one Linux installation. The other operating systems were checked in native CI. Those CI results do not substitute for interactive keychain, login, account-refresh, or real-provider testing on every platform. The preserved externally managed account remains subject to its explicit owner and handoff contract.

## Public supply-chain identities

The source-archive hashes identify official release assets. The Arch package hashes identify the packages built from the published AUR recipes during this acceptance.

| Artifact | SHA256 |
|---|---|
| zcalld-0.2.4-source.tar.gz | be26976f7cb84e18dc584dc25a9959fa9d97473862bfef03f41092029c10f524 |
| zcalld-0.2.4-1-x86_64.pkg.tar.zst | 818b686851d20b0cda5ccc16606af637b099bf96c5c2513606679b0d9d215728 |
| zcalld-compat-0.1.5-source.tar.gz | 7b7972a79e4abdeb7e54590bead662032840fcb11b355ae1c60ea406ceff14cc |
| zcalld-compat-0.1.5-2-x86_64.pkg.tar.zst | 8053759d52c83c3afe08677940498667bab3076ac72f8563780d460b1d1b6729 |

| Published identity | Commit |
|---|---|
| Core v0.2.4 source | b9bef29e56897cd05ce2b9587aedb51fefb00842 |
| [Core AUR source recipe](https://aur.archlinux.org/packages/zcalld) | 434c41380fcf76a673d0af8c10559a3cb86afb9f |
| Compatibility v0.1.5 runtime source | a1ad1577d22d67b7557dda0a5be1270cc92af86a |
| Compatibility packaging revision | fefb7a4ced278498524658f1953140e94e6c72a0 |
| [Compatibility AUR recipe](https://aur.archlinux.org/packages/zcalld-compat) | aba7fb8e04a46bb215192faf001c2c20d663f6b2 |

The compatibility package release changed the Arch installation layout while retaining the v0.1.5 runtime source and immutable runtime release assets. The core source archive records all eight recursive submodule revisions, including that compatibility packaging revision.

The accompanying [JSON record](benchmarks/deployment-2026-11-20.json) publishes aggregate outcomes and measured values. Raw machine records, private screenshots, credentials, and configuration fingerprints remain outside the public report.
