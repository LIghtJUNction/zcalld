# Changelog

## 0.2.3 — 2026-11-20

- fix(arch): resolve relocated compatibility helpers

## 0.2.2 — 2026-11-20

- fix: ship Codex stream compatibility and native release coverage
- fix(provider): honor overall deadlines and classify failures

## 0.2.1 — 2026-11-20

- fix: initialize generated builtin plugins without invoking helper exports

## 0.2.0 — 2026-11-20

- Add native Linux, macOS and Windows releases, local agent detection, and per-user systemd, launchd or scheduled-task controls. Linux and macOS management clients prefer a private Unix socket while existing TCP clients retain their address.
- Import Magpie providers, complete model catalog metadata, accounts, groups, community packages, usage and privacy state into a private migration. Preserve external ownership of active native Codex credentials.
- Add a recoverable Linux cutover transaction that verifies both TCP and Unix health, model catalogs and retained MCP metadata before activation. Rollback drains managed workers and preserves later credential and usage changes.
- Keep reversible privacy, advanced routing and JavaScript community support in optional plugins. The compatibility package supplies separate lazy Go workers and a shared lazy Bun host, without an embedded browser.
- Apply privacy before routing classification and restore masked values in JSON and fragmented SSE. Missing required privacy workers fail closed before upstream generation.
- Continue bounded private privacy audit events and counters through compatibility 0.1.3. Synchronize the final old history after drain and preserve new history during recoverable rollback under stable external locks.
- Expose community tools through the optional HTTP plugin API, including the legacy Magpie MCP address. Bound request framing, response chunks, cancellation and owned-worker shutdown.
- Add transactional provider, account, agent, profile and usage controls, plus separate MCP, instruction and skill libraries with conflict-aware preview, apply and revert.
- Add bounded model groups, IANA time-zone and RE2 conditions, trusted workspace matching, and limited retries of explicit upstream refusals before a response starts. Started streams and ambiguous failures are never replayed.
- Pin the separate zui-based zcall-gui and native zcall-omarchy frontends as submodules, with independent releases and AUR packages.
- Preserve early HTTP rejection responses with a bounded write half-close, and isolate Windows compatibility workers from console signals so IPC cancellation and credential writes can finish.
- Validate five native release targets with real privacy, routing, community MCP and library fixtures. Publish recursive pinned source archives, native packages, checksums and source/binary AUR recipes through the version-bump workflows.

## 0.1.1 — 2026-11-19

- fix: use portable byte pointers in native curl callbacks

## 0.1.0

- Add Linux-native local gateway and CLI using zcall 0.3.0 and zconv 0.4.0.
- Support six protocol ingress formats, strict conversion and incremental SSE.
- Add bounded workers, explicit credentials, request/response limits and non-replayable generation uploads.
- Add zplugin 0.3.0 SDK plus separately built alias and policy plugins.
- Add system/user systemd notification services and standard Linux installation paths.
- Add native x86_64/aarch64 release archives, source/binary AUR recipes and transactional version bump workflows.
- Add local integration, release regression and reproducible performance checks.
