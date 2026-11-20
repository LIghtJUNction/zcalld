# Changelog

## 0.4.0 — 2026-11-20

- Install and update complete native bundles through a verified private prefix, with per-file manifests, a durable recovery journal, runtime leases and complete rollback. Package-managed installations keep using their package manager.
- Add explicit first-use configuration and Linux user-service registration for private installs. Preserve existing files, reject conflicting service definitions, and keep installation, configuration and service startup as separate confirmed actions.
- Recover eligible explicit-account routes after bounded, fully received upstream refusals. Healthy fallbacks run first; a proven weekly reset allows one final request within the original deadline. Prompts and credentials never enter recovery metadata, and cancellation drains owned workers.
- Pin compatibility 0.2 with native quota reads, safe builtin upgrades, inherited warmup/reset policies, persistent maintenance ledgers and fresh quota checks after a confirmed reset. Optional maintenance remains off until configured or imported.
- Add optional native completion, quota and privacy notifications with conversation-safe filtering and keep-awake lifecycle handling. Preserve model-group references in provider and route edits.
- Run recovery, maintenance, full-archive installer and service-registration acceptance with isolated local fixtures. Retain five native release targets, checksums, source archives and AUR publishing in the formal release workflow.

## 0.3.1 — 2026-11-20

- test: exercise the real Codex ChatGPT path and quota parsing
- fix: accept native Codex paths and preserve quota metadata
- build: pin the verified native GUI and Ink panel updates

## 0.3.0 — 2026-11-20

- feat: share gateway primitives and manage caller keys
- fix: normalize the pinned Windows zlib static archive name

## 0.2.5 — 2026-11-20

- fix: pin the released GUI after the zgpui rename
- fix: restore Codex HTTP fallback and compressed requests
- docs: record verified 0.2.4 migration and compatibility resource use

## 0.2.4 — 2026-11-20

- fix: release socket locks explicitly and check Windows helper paths

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
