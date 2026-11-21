# Magpie alignment and remaining acceptance

Reviewed: 2026-11-22 (Asia/Taipei). This matrix describes the current implementation candidate.
It is a review map, not a declaration of complete Magpie parity or a release gate result.

## Reference and evidence boundaries

- Product reference: the official [Magpie README](https://github.com/yetone/magpie/blob/main/README.md)
  and [reference manual](https://github.com/yetone/magpie/blob/main/docs/reference.md).
- Frozen source reference: [`1e174f8f9f613a920203a4658d259456fcab5d0d`](https://github.com/yetone/magpie/tree/1e174f8f9f613a920203a4658d259456fcab5d0d).
  Existing integration contracts link specific files at that revision.
  The moving manual can describe newer behavior; it is not evidence that every feature existed at the frozen revision.
- The measured installation for this iteration is **Magpie 0.1.2512**.
  Its version and the frozen source reference are separate identifiers; no identical-build claim is made.
- Source paths establish implemented behavior. Named scripts establish relevant test coverage;
  their presence does not mean every platform, live provider, or configuration passed this iteration.
  Actual commands, revisions and outcomes are recorded in the [iteration validation report](validation/2026-11-22-magpie-iteration.md).
- **Native** means implemented in this repository. **Bridge** means an optional compatibility component is required.
  **Partial** and **remaining** identify a specific boundary, not a count of missing product features.
  Rows differ in scope and must not be summed into a feature score or a claim of more/fewer total features.

## Configuration, agents and access

| Capability family | zcalld implementation and evidence | Remaining acceptance / GUI boundary |
| --- | --- | --- |
| Agent discovery and model configuration | Native registry, atomic apply/revert and custom adapters in [adapters.rs](../src/adapters.rs); [test-control.py](../scripts/test-control.py). The built-in model registry has 16 entries, 10 writable and 6 detect-only. | Detection is not model configuration support. Verify each writable adapter against current native applications and add supported writers for the six detect-only entries. GUI Agents consumes these capabilities. |
| Providers and model catalog | Native provider CRUD/test/discovery and retained model metadata: [config.rs](../src/config.rs), [control.rs](../src/control.rs), [test-control.py](../scripts/test-control.py). GUI has provider management and search. | One native key or key-environment binding per provider; no native per-provider multi-key pool is demonstrated. Separate provider entries are not equivalent to key rotation, cooldown and quota aggregation. |
| Routes, groups and rules | Native aliases and seven group modes: smart, order, rotate, usage, pace, weight and drain. Advanced RE2/time-zone conditions use a bridge: [routing](../plugins/routing/src/lib.rs), [advanced.rs](../plugins/routing/src/advanced.rs), [test-routing.py](../scripts/test-routing.py), [test-routing-advanced.py](../scripts/test-routing-advanced.py). | Group editing rejects nested group members. Nested selection, cycle detection and recursive fallback remain a gap. No dedicated GUI route/group editor is present. |
| Profiles | CLI snapshots gateway configuration, credentials and privacy settings: [control.rs](../src/control.rs), [test-control.py](../scripts/test-control.py). | This is not Magpie's complete per-agent model/profile snapshot behavior. Agent-specific visible-model lists and complete group/library profile semantics need separate work; no dedicated GUI profile page. |
| Caller API keys | Native list/add/rotate/revoke, LAN permission and trusted workspace metadata: [keys.rs](../src/keys.rs), [caller_keys.rs](../src/config/caller_keys.rs), [test-keys.py](../scripts/test-keys.py). | Generation-specific model scopes, token/currency budgets and issuer policy enforcement are not established by these key-management tests. MCP tool permissions are a separate path. No dedicated GUI key editor. |
| Management interface | JSON CLI and service control: [main.rs](../src/main.rs), [control.rs](../src/control.rs), [test-control.py](../scripts/test-control.py), [test-service-install.py](../scripts/test-service-install.py). | The GUI invokes the CLI. A complete authenticated HTTP management API, Web management surface and interactive TUI are not supplied by the native CLI contract. |

The ten writable model adapters are Claude Code, Codex, Gemini CLI, OpenCode, Pi,
Qwen Code, Aider, Goose, Zed and VS Code. Cursor, Amp, Continue, Cline, Roo and Kilo
are detect-only in this registry. The Library has a separate target matrix;
support for writing an agent's MCP file does not imply support for changing its model.

## Generation transport and usage

| Capability family | zcalld implementation and evidence | Remaining acceptance / GUI boundary |
| --- | --- | --- |
| Protocol translation and streaming | zcall-backed Chat, Responses, Anthropic Messages, Gemini GenerateContent, Completions and Interactions; strict conversion through [protocol.rs](../src/protocol.rs) and [server.rs](../src/server.rs). Fixtures: [integration.py](../scripts/integration.py), [test-transport.py](../scripts/test-transport.py), [test-native-runtime.py](../scripts/test-native-runtime.py). | Six protocol names are not proof of every source/target feature combination. Keep tool calls, reasoning and unsupported-loss behavior tied to exact fixtures. See [ARCHITECTURE.md](ARCHITECTURE.md). |
| WebSocket generation | Documented HTTP/SSE fallback behavior and native compressed ingress: [CODEX_TRANSPORT.md](CODEX_TRANSPORT.md), [test-codex-transport.py](../scripts/test-codex-transport.py). | Accepting a client's fallback does not implement native Responses WebSocket sessions. Session reuse, cancellation and event semantics need their own implementation and acceptance. |
| Audio, token counting and Ollama API | Generic provider configuration can address an OpenAI-compatible upstream; see [config.rs](../src/config.rs) and [example configuration](../examples/config.conf). | No dedicated native audio/transcription, Anthropic `count_tokens`, or Ollama `/api/*` ingress contract was found in the reviewed routes. Ollama's `/v1` compatibility is not native Ollama API parity. |
| Retry, cancellation and recovery | Bounded explicit-refusal retry and recovery hooks: [server.rs](../src/server.rs), [recovery.rs](../src/recovery.rs), [test-recovery.py](../scripts/test-recovery.py), [test-generation-cancellation.py](../scripts/test-generation-cancellation.py). | Preserve the no-replay boundary for started streams and uncertain network failures. Expand acceptance only for newly introduced retry/account selection behavior. |
| Request and token records | Bounded JSON/SSE collector, five token counters, legacy history, model/time filters and export: [usage.rs](../plugins/routing/src/usage.rs), [maintenance.rs](../src/maintenance.rs), [test-usage.py](../scripts/test-usage.py), [test-control.py](../scripts/test-control.py). Native enable/disable works without groups and avoids duplicate explicit/automatic bindings. | The endpoint records upstream attempts, including retries. Detailed end-to-end usage fixtures cover same-protocol paths; preservation of all details through cross-protocol conversion remains unverified. |
| Cost and usage attribution | Counts retain provider-reported input/output/cache/reasoning semantics; see [USAGE.md](USAGE.md). GUI displays aggregate request/token history and recording settings. | No native price catalog, currency ledger, invoice reconciliation or complete provider/account/session cost attribution is established. Mixed providers' five counters must not simply be summed. GUI time/model filtering and cost drill-down are not implemented by the aggregate page. |

The Usage observer sees the response-plugin stream, which can follow protocol conversion.
A parser that recognizes an upstream usage shape cannot recover a field already removed by a converter.
Cross-protocol accounting therefore needs fixtures that compare the original upstream counters,
the delivered response and the persisted record, including cache creation and reasoning.

## Accounts, plugins and reusable agent configuration

| Capability family | zcalld implementation and evidence | Remaining acceptance / GUI boundary |
| --- | --- | --- |
| Subscription accounts and provider authentication | Account import/selection and optional provider runtime: [maintenance.rs](../src/maintenance.rs), [community plugin](../plugins/community/src/lib.rs), [test-account-runtime.py](../scripts/test-account-runtime.py), [test-native-compat.py](../scripts/test-native-compat.py). | These bridge paths are not a universal native OAuth implementation or complete GUI sign-in flow. Live login/refresh/revocation requires provider-specific acceptance; fixtures do not demonstrate every subscription provider. |
| Codex quota and policy | Account-bound quota/credit policy and explicit maintenance/recovery bridge: [CODEX.md](CODEX.md), [test-codex-policy.py](../scripts/test-codex-policy.py), [test-weekly-maintenance.py](../scripts/test-weekly-maintenance.py). | A specific Codex policy must not be generalized to all providers, all account quotas or general-purpose automation. |
| Plugin lifecycle and middleware | Native ABI plus optional managed/community host: [extensions.rs](../src/extensions.rs), [plugins.rs](../src/plugins.rs), [PLUGINS.md](PLUGINS.md), [test-native-compat.py](../scripts/test-native-compat.py). | Community behavior requires the helper/runtime. Native ABI support does not establish arbitrary Magpie plugin UI, marketplace, hot reload or every middleware hook's compatibility. |
| Privacy masking/restoration | Optional native redaction adapter with worker preflight: [redact plugin](../plugins/redact/src/lib.rs), [extensions.rs](../src/extensions.rs), [test-privacy-init.py](../scripts/test-privacy-init.py), [test-builtin-fetch.py](../scripts/test-builtin-fetch.py). GUI exposes privacy settings. | Local model availability, detection quality and complete masking-history UI require their own acceptance. A settings toggle alone is not evidence of redaction quality. |
| Library MCP/instructions/skills | Native CRUD, assignment, owned-file apply/revert, backups and conflict handling: [library.rs](../src/library.rs), [LIBRARY.md](LIBRARY.md), [test-library.py](../scripts/test-library.py). | File-format support follows `library targets`, not the model-adapter list. There is no dedicated GUI Library editor; WSL mappings and unsupported target formats remain explicit boundaries. |
| Git Skills | This iteration adds fetch/update from HTTPS or a local Git repository, selected subdirectory/ref, resolved commit provenance and local-edit conflict checks: [library_git.rs](../src/library_git.rs), [library.rs](../src/library.rs), [test-library.py](../scripts/test-library.py). | Repository files are read as objects; skill code is not executed by fetching. Marketplace discovery, scheduled updates, shared skill routing and GUI source management remain separate capabilities. |
| MCP tools and hub | Optional community-tool endpoint with permission filtering: [community MCP adapter](../plugins/community/src/mcp.rs), [plugin_http.rs](../src/plugin_http.rs), [MCP.md](MCP.md), [test-mcp.py](../scripts/test-mcp.py). | This endpoint is not an arbitrary Library MCP-server hub. Aggregating stdio/remote servers, per-server lifecycle and remote MCP OAuth sign-in are not implemented by Library metadata import or tool exposure. |
| Library market and automation | Metadata is retained and unsupported execution paths are identified in [LIBRARY.md](LIBRARY.md) and [library.rs](../src/library.rs). | Market discovery, hooks, commands, subagents, rule automation, automatic skill updates and skill routing are not completed merely by importing their flags. No executable parity claim is made. |
| Synchronization | Explicit migration, import/export and selected policy/desktop sync exist: [migration.rs](../src/migration.rs), [library.rs](../src/library.rs), [desktop.rs](../src/desktop.rs). | These operations do not supply general cloud/peer synchronization, conflict reconciliation or a remote-device management UI. |

MCP OAuth and provider OAuth are distinct acceptance targets. Archived credentials are data preservation,
not proof that a sign-in/refresh flow executes. Similarly, MCP-specific caller permissions do not prove
generation model scopes or token/currency budget enforcement.

## Desktop and operations

| Capability family | zcalld implementation and evidence | Remaining acceptance / GUI boundary |
| --- | --- | --- |
| Desktop activity and notifications | Optional native desktop plugin, activity leases and completion/quota events: [desktop.rs](../src/desktop.rs), [desktop plugin](../plugins/desktop/src/lib.rs), [test-desktop-host.py](../scripts/test-desktop-host.py), [test-desktop.py](../scripts/test-desktop.py). | Native OS notification/inhibitor behavior needs platform-specific evidence. This is not a general automation scheduler or a replacement for all Magpie tray interactions. |
| Migration and recovery | Private staging, verification and explicit cutover: [migration.rs](../src/migration.rs), [cutover.py](../scripts/cutover.py), [test-cutover.py](../scripts/test-cutover.py). | A successful import preserves supported state; it does not prove execution of every imported Magpie feature or authorize a live service switch. |
| Installation and updates | Native installer, package-aware updates and service installation: [installer.rs](../src/installer.rs), [test-installer.py](../scripts/test-installer.py), [test-service-install.py](../scripts/test-service-install.py). | Release/package/platform results must name exact artifacts. Dependency updates alone do not demonstrate faster execution or less memory. |
| GUI workflow and appearance | The companion [zcall-gui repository](https://github.com/LIghtJUNction/zcall-gui) contains `src/main.rs`, `src/pages.rs` and `src/usage.rs`; Usage adds explicit loading/error/empty states, refresh and enable/disable with a separate restart action. | Current navigation is Overview, Providers, Agents, Plugins, Privacy, Usage and Updates. Routing, Library, caller keys and profiles remain CLI workflows. GUI unit tests and `scripts/ui_state_smoke.py`/`scripts/ui_smoke.py` cover their stated scenarios, not every native desktop. |

## This iteration's concrete progress

1. **Git Skills:** native source fetch/update, recorded revision, resource handling and conflict refusal;
   the focused Git cases in `scripts/test-library.py` cover pinned revisions, failed fetches and links.
2. **Usage correctness:** bounded scanning survives long JSON, compact SSE `data:` fields and Anthropic
   `message_start`; cache reads, cache writes and reasoning are persisted without retaining response text.
3. **Usage lifecycle:** native enable/disable preflights the plugin and saved configuration, preserves
   groups/history/permissions, and deduplicates an explicit binding with managed loading.
4. **GUI:** Usage presents saved recording state and keeps service restart an explicit action;
   older CLI output without that field is treated as unknown. Provider search and refreshed layouts
   belong to the companion GUI changes, with native visual evidence recorded separately.
5. **Hook buffers:** request-hook buffer ownership and reuse are implemented in `src/plugins.rs`, with focused
   unit cases and `scripts/benchmark_rewrites.py`. Performance conclusions require the run report.

`recording_configured` reports saved settings, not an inspected running daemon.
An idempotent command returning `restart_required:false` only says that invocation changed no settings.
No performance figures or overall feature-completeness percentage are inferred from this matrix.

## Next priorities and completion criteria

1. Close the everyday management loop: route/group, Library and caller-key GUI workflows;
   acceptance must perform real CLI-backed mutations, reload their state and exercise failure recovery.
2. Add provider multi-key pools and generation access/budget policies with identity-specific fixtures;
   define cost attribution and provider token semantics before adding currency totals.
3. Verify detailed cross-protocol usage, then prioritize native WebSocket and additional endpoints
   from real client requirements; require streaming, tools, cancellation and accounting evidence together.
4. Extend Library execution deliberately: MCP hub/OAuth, market, synchronization and automation each
   need their own contract, lifecycle and conflict tests. Retained metadata is not completion evidence.
5. Reconcile the remaining agent writers, nested groups and profile semantics against the frozen
   source and installed version. Use requirement-specific acceptance instead of counting table rows.
