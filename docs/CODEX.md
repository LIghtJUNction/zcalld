# Native Codex metadata

The optional community extension can serve the read-only metadata requests used by Codex 0.170 alongside the existing Responses gateway. Model instructions, account information and quota windows come from the explicitly bound Codex account. The extension does not manufacture a catalog, select the first available account or refresh native Codex credentials.

This feature requires **zcalld 0.4.0 and zcalld-compat 0.2.0 or newer**. It is disabled by default. It reuses the community extension's lazy Bun host, with no additional listener, polling task or daemon.

## Supported requests

All entries accept GET with an empty body and retain the gateway's normal authentication and Origin checks.

| Gateway path | Fixed upstream path on `https://chatgpt.com` | Query |
| --- | --- | --- |
| `/backend-api/codex/models` | `/backend-api/codex/models` | Exactly one `client_version` |
| `/backend-api/wham/usage`, `/api/codex/usage` | `/backend-api/wham/usage` | None |
| `/backend-api/wham/accounts/check`, `/api/codex/accounts/check` | `/backend-api/wham/accounts/check` | None |
| `/backend-api/wham/config/bundle`, `/api/codex/config/bundle`, `/codex/config/bundle` | `/backend-api/wham/config/bundle` | None |

`client_version` is limited to 64 ASCII letters, digits, dots, plus signs, hyphens or underscores. Missing, duplicate, escaped or extra query parameters are rejected. Requests cannot supply an upstream URL. Redirects are rejected.

These contracts follow the official [Codex 0.170 model endpoint](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/codex-api/src/endpoint/models.rs), [model schema](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/protocol/src/openai_models.rs) and [backend client](https://github.com/openai/codex/tree/rust-v0.170.0/codex-rs/backend-client). Other account, task and mutation endpoints are not claimed by this extension. Changing where a client sends requests remains a separate, explicit client configuration decision.

## Bind an existing provider and its routes

The account must already be configured for an enabled `builtin:codex` provider. For example:

```ini
[provider codex-local]
base_url = https://chatgpt.com/backend-api/codex
protocol = responses
plugin = builtin:codex
plugin_account = builtin:codex#work
plugin_npm = @ai-sdk/openai

[route my-codex-model]
provider = codex-local
model = upstream-model-slug
```

These identifiers are examples. Use the actual local provider, account key and upstream model slug. A missing `plugin_account` is accepted only when the registry has an explicit `account_defaults["builtin:codex"]`. Another enabled account is never an implicit substitute.

Create a binding file containing the routes that this account may describe:

```json
{
  "enabled": true,
  "loopback": {
    "provider": "codex-local",
    "routes": ["my-codex-model"]
  },
  "callers": {
    "office-client-key-id": {
      "provider": "codex-local",
      "routes": ["my-codex-model"]
    }
  }
}
```

Omit `callers` when remote access is unnecessary. Each entry names a caller-key **ID**, not its secret. A remote caller must authenticate with that key and have its own binding. The global gateway token alone does not choose an account for remote metadata. The `loopback` entry applies only to an actual loopback or Unix peer.

Apply the binding and materialize the installed helper's embedded runtime:

```sh
zcalld plugins codex configure --file /absolute/codex-binding.json
zcalld plugins runtime configure
zcalld plugins codex show
zcalld check
```

For a nondefault configuration, add `--config /absolute/config.conf` to each command. Configuration commands do not start a host, contact providers, refresh credentials or restart the gateway. Review the result, then restart through the normal service manager. For a Linux user service, use `systemctl --user restart zcalld.service`.

Upgrading the helper alone does not replace script paths held by an existing private registry or running host. `runtime configure` materializes the new embedded runtime while preserving accounts, selections, options and enabled state. A normal restart loads it. Install compatible core and helper packages before enabling this feature.

`zcalld plugins codex disable` disables these metadata routes while retaining their bindings. Ordinary Responses generation and community MCP remain independently configured.

## Identity and model boundaries

The core supplies actual peer classification, the verified caller-key ID and loaded provider/route relationships. Host, X-Loopback and X-Caller-Key-Id headers cannot create those relationships. The optional HTTP interface requests this context with flags 3; existing flags 1 handlers, including MCP, retain their previous context. Configured provider credentials, base URLs and headers are excluded.

An incoming `ChatGPT-Account-Id` is an untrusted consistency hint. If present, it must match the account selected by the explicit binding. Duplicate, empty, malformed or mismatched values are rejected. The hint never selects an account or grants access. Incoming Authorization is never used as upstream authentication.

A model is returned only when a configured static route maps to a slug in that account's official response. The exposed slug becomes the public route ID; the upstream entry's instructions and capabilities are retained. An upgrade target uses its advertised canonical slug or its unique local alias. The upgrade is omitted when the target is absent from the filtered catalog or has multiple aliases without a canonical route. Missing instructions are rejected, and unbound upstream models are omitted. Dynamic groups and routes belonging to another provider cannot borrow a member's metadata. The filtered catalog gets its own content-derived ETag.

Usage, account-bootstrap and config-bundle responses preserve validated upstream JSON bytes. No quota percentage, account or instruction text is synthesized. These are request-time upstream reads; the local quota-cache CLI is a separate interface.

Externally owned native credentials are reread from their bound source without writing them. Expiry or an account change fails explicitly. The native owner remains responsible for refresh; metadata does not call loaders, refresh hooks or generation APIs.

## Bounds and failures

Each metadata request uses the smaller of the gateway's overall timeout and 30 seconds, including waiting for cold shared-host initialization. Response data is limited to the smaller of the configured response limit and 4 MiB. Streaming retains the existing byte-credit and cancellation protocol.

Timeout, a TCP reset or a full Unix-socket disconnection releases the request's own calls and cancels its pending stream. The worker and shared host remain available for later requests, and another request's initialization wait is preserved. A client that closes only its write direction can continue reading its response. Ordinary TCP FIN alone is treated as a possible write half-close; its request retains the normal response and deadline. An enabled community plugin that fails initialization makes that native host fail closed for subsequent calls. The management CLI retains partial-load diagnostics.

Redirects, invalid JSON, incompatible content types, missing model instructions and oversized bodies fail explicitly. Returned errors use static categories; arbitrary upstream error text, credentials and response headers are not reflected. Successful metadata responses expose only JSON content type, cache policy and an applicable ETag.

## Compaction uses the Responses pipeline

Codex 0.170 requests remote compaction through an ordinary Responses stream containing a `compaction_trigger` input item. Existing decompression, authentication, privacy, group routing and protocol handling apply. This feature does not alias an old `/responses/compact` path to generation.

The builtin adapter preserves opaque IDs and `encrypted_content` in `compaction`, `compaction_summary` and `context_compaction` items. Ordinary message normalization remains unchanged, and plaintext still passes through privacy masking and restoration. See the official [remote compaction implementation](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/core/src/compact_remote_v2.rs) and [wire item types](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/protocol/src/models.rs).

## Subscription-credit policy

The optional Codex adapter can refuse generation when a subscription window is
exhausted. The policy belongs to the selected account, independently of access
tokens, refresh ownership, and migration rollback fingerprints.

For a previously imported Magpie account, inspect the current source policy:

```sh
zcalld plugins codex policy plan --source /absolute/path/to/magpie --json
```

The result contains aggregate counts and a `plan_hash`. After reviewing them,
synchronize exactly that plan:

```sh
zcalld plugins codex policy sync --source /absolute/path/to/magpie \
  --expect-plan PLAN_HASH --json
```

The source account must match the existing private migration map by login ID
and its recorded user fingerprint. The policy's user comparison follows
Magpie's lowercase, trimmed identity rule. An unknown or changed identity blocks
the synchronization. This command does not import, open, replace, or refresh
target credentials; a native client's intervening token rotation does not make
the policy operation overwrite its credentials. Source settings can change
after a migration, so policy synchronization is explicit.

The stored shape is
`account_policies[OPAQUE_ACCOUNT_KEY] = {"codex_no_credits": true}` in the private
extension registry. Absence means unrestricted; a malformed present value is an
error. A running compatible host reads policy for its final account selection
on every request. It rechecks the policy, account enablement, and identity
immediately before generation, including when it reuses a cached loader.
Requests and client headers cannot supply policy. After the upgrade has materialized and started this runtime, ordinary policy
changes need no further host restart.

For restricted accounts, the adapter reads the fixed official usage endpoint
with existing credentials before generation, using an eight-second ceiling,
a 64 KiB response bound, the request's cancellation, and no redirects.
It checks the account-wide primary and secondary subscription windows from
that response. A known exhausted window returns HTTP 429 before generation;
missing, malformed, or expired availability returns HTTP 503. Existing group
fallback may then select another member. Neither outcome starts a paid retry
on the blocked account. Unlimited credits or a positive credit balance do not
establish subscription availability.

A one-second cache and shared concurrent lookup reduce duplicate usage reads.
Cancelling one caller does not cancel another caller's shared lookup; cancelling
the last waiter aborts that lookup. The cache is bounded to 64 credential
identities. Its in-process key is a digest of the selected workspace and the
resolved access token, so different members of one workspace cannot share quota
state. Cache entries, pending lookups and observed response holds use that same key.

Usage checking and generation use one resolved credential snapshot. Immediately
before generation, the adapter rereads the current authentication and verifies
that its workspace and token still match. A change during the check stops the
request before generation; the next request checks the new credentials. Token
renewal therefore invalidates the short quota cache without changing the stored
account policy. No automatic retry or background token refresh is added.

Fixed account-wide full-window response headers can tighten a cached hold; named
header families do not establish a model mapping. Unrestricted accounts add no
usage request. Metadata reads remain available independently of the generation
guard. See the [credential-isolation validation](validation/codex-quota-identity-20261120.md)
for the shared-workspace and credential-switch regressions.

This is a client-side spending rule, not a server billing guarantee. Another
client, or a single request that crosses the remaining window, can change
availability after the check. No official server switch preventing all credit
charges is claimed. The behavior follows the account-window hold in Magpie's
[pinned credit policy](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/provider/codex_credits.go).

The local acceptance suite uses generated embedded modules, a native gateway,
fake credentials and a network guard. It covers live policy changes on a
cached loader, malformed policies, final account selection, concurrent changes,
cancellation, metadata reads while generation is held, and a group fallback
that retains request redaction. It makes no real provider request.

## Verification and limits

| Evidence | Coverage |
| --- | --- |
| [Native metadata fixture](../scripts/test-codex-metadata.py) | Generated bundle and native HTTP plugin; explicit binding; actual peer classification; forged headers; query/method rejection; bounds; cold-init cancellation; shared failures; no host when disabled |
| [Installed Codex client fixture](../scripts/test-codex-client-metadata.py) | Codex 0.170.0 app-server parses account windows and bound model aliases; malformed usage produces a client error; no model turn is started |
| [HTTP cancellation fixture](../scripts/test-http-cancellation.py) | TCP reset and full Unix disconnect release pending calls; the same worker and shared host remain usable; TCP and Unix write half-closes retain responses |
| [Native compaction fixture](../scripts/test-codex-compaction.py) | Plain, gzip and zstd through the actual privacy worker and routing plugin; opaque IDs and ciphertext in JSON/SSE; plaintext masked upstream and restored downstream |
| [Native policy fixture](../scripts/test-codex-policy.py) | Per-account policy updates, final-account selection, fail-closed quota checks, cancellation and privacy-preserving group fallback |
| [MCP fixture](../scripts/test-mcp.py) | Existing tools, sessions and cancellation |
| [Builtin hook tests](https://github.com/LIghtJUNction/zcalld-compat/tree/main/community/builtin/tests) | Fixed origin, redirects, malformed metadata, external ownership, bounded reads and compaction preparation |

These fixtures use synthetic credentials and local network substitutes. They do not exercise a real subscription, spend credits, refresh an account or change a user's Codex configuration. Passing these contracts does not establish that every Codex feature or every provider account has been tested.

### Installed-client acceptance

With Codex 0.170.0 installed and the native build artifacts available, run on Linux:

```sh
python3 scripts/test-codex-client-metadata.py \
  --binary target/release/zcalld \
  --community-plugin target/release/libzcalld_community.so \
  --pluginctl _release/native-compat/zcalld-pluginctl \
  --redact-plugin target/release/libzcalld_redact.so \
  --privacy-worker _release/native-compat/zcalld-privacy \
  --routing-plugin target/release/libzcalld_plugin_routing.so
```

This separate acceptance target requires the installed client. It sends only initialization,
account-rate-limit and model-list RPCs through an isolated gateway. The fixture uses disposable
credentials and local metadata, verifies the parsed client values, and rejects attempted
external HTTP connections through a local proxy. The portable native suite includes HTTP
cancellation checks without requiring Codex.
