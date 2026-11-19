# Native plugins

zcalld loads optional native plugins through the pinned `zplugin` ABI. The gateway uses
`zcall` for protocol handling; plugins add explicit routing and request policy
without a second runtime for those native hooks. Optional compatibility plugins
start their own helper processes only when that capability is used.

## Enable the included plugins

Build the workspace or install the packaged shared libraries:

```sh
cargo build --release --workspace --locked
```

Add the following sections to the gateway configuration. Their order matters:
the aliases plugin selects a route before the policy plugin checks that route.

```ini
[plugin aliases]
path=/usr/lib/zcalld/plugins/libzcalld_aliases.so
config_file=aliases.conf

[plugin policy]
path=/usr/lib/zcalld/plugins/libzcalld_policy.so
config_file=policy.conf
```

The paths above are the system-package installation paths. For a source checkout,
use absolute paths to `target/release/libzcalld_aliases.so` and
`target/release/libzcalld_policy.so`. Configuration files resolve relative to the
gateway configuration directory.

`aliases.conf`:

```ini
# Public model name = configured gateway route
fast=local
think=reasoning
```

`policy.conf`:

```ini
allow=local,reasoning
max_bytes=262144
```

An alias performs one exact mapping. It does not recursively resolve other
entries. Unknown model names pass through to later hooks and normal routing.
Duplicate aliases, empty names and control characters are configuration errors.

The policy plugin checks exact, case-sensitive route names and an optional
request-body size limit. Omitting `allow` permits all route names; omitting
`max_bytes` leaves the gateway's body limit in charge. Unknown settings,
duplicate settings, duplicate allowed models and a zero size limit are errors.
The included configuration contains no provider credentials.

## Interface and ordering

The shared crate is `zcalld-plugin-api`. Its interface ID is
`dev.zcalld.gateway`, version `1`.

| Hook | Input | Successful result |
| --- | --- | --- |
| `configure` | Explicit UTF-8 configuration | Initializes one worker's instance |
| `route` | Selected model/route name | Zero keeps the input; a byte count selects a UTF-8 replacement |
| `request` | Protocol ID and JSON request bytes | Zero keeps the input; a byte count replaces the body |
| `observe` | HTTP status, elapsed milliseconds, input/output body bytes | Records a completed call without receiving its content |

Request hooks run before routing, once per plugin in configuration order. A request
hook runs before the gateway's normal protocol conversion. The gateway validates
rewritten requests and keeps its own authentication and resource limits.
Plugins cannot replace these core checks through this interface.

Protocol numbers are frozen independently of Rust enum layout:

| Value | SDK constant | Protocol |
| --- | --- | --- |
| 1 | `protocol::OPENAI_CHAT` | OpenAI Chat Completions |
| 2 | `protocol::OPENAI_RESPONSES` | OpenAI Responses |
| 3 | `protocol::ANTHROPIC_MESSAGES` | Anthropic Messages |
| 4 | `protocol::GEMINI_GENERATE_CONTENT` | Gemini GenerateContent |
| 5 | `protocol::OPENAI_COMPLETIONS` | OpenAI Completions |
| 6 | `protocol::GEMINI_INTERACTIONS` | Gemini Interactions |

Plugin methods return `zplugin::Result<T>`. Positive application status
`1` means a policy denied the model; `2` means a plugin's request-size limit
was exceeded. The SDK exports these as `status::MODEL_DENIED` and
`status::REQUEST_TOO_LARGE`. Other failures retain their zplugin diagnostic.

A load, ABI, configuration, routing or request error stops the affected operation.
Every observer is called even when an earlier observer fails; the first observer
failure is returned to the host for logging. Observation happens after the
request, so it cannot replace response bytes already delivered.

## Memory and lifetimes

Each worker loads and creates its own plugin instances on that worker thread.
The host stores owning proxies backed by an `Rc<Plugin>` library lease.
Proxies are neither `Send` nor `Sync`; zcalld adds no unsafe thread-sharing
implementation. Each instance keeps its own counters and configuration.
Aggregating state across workers belongs to a plugin designed to do so.

Inputs and outputs are borrowed for one method call. A plugin must not retain
their pointers. Route names are limited to 4,096 UTF-8 bytes. The host accepts
at most 32 configured plugins and 65,536 configuration bytes per plugin.

Unchanged request hooks use an empty output slice, so aliases and policy do not
allocate a request-output buffer in the host. A transforming plugin returns
`Error::BufferTooSmall` until the output slice fits. The host grows that slice
up to the configured request-body limit. On `BufferTooSmall`, plugin state must
remain unchanged because the host retries the hook. A successful positive byte
count must describe initialized output and fit inside the supplied slice.

Native code runs inside the daemon with its privileges. Only explicitly
configured, trusted absolute library paths are loaded. ABI checks catch
interface incompatibility; they do not sandbox native machine code. Libraries
use zplugin's pinned policy and remain resident until the process exits.
Restart the service after upgrading an installed plugin.

## Write a plugin

Use a separate `cdylib` crate and the shared SDK:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
zcalld-plugin-api = "0.1"
zplugin = { version = "=0.3.0", default-features = false, features = ["std", "macros"] }
```

Implement all four `GatewayPlugin` methods, then export the generated adapter:

```rust,ignore
zplugin::export!(
    GatewayPluginExport<MyPlugin>,
    constructor = create,
    version = (1, 0, 0),
    unload = pinned
);
```

The [aliases implementation](../plugins/aliases/src/lib.rs) and
[policy implementation](../plugins/policy/src/lib.rs) are complete examples.
The SDK also exposes `GatewayPluginApi::write_c_header` for C/C++ declarations
usable from Zig. No Rust `String`, `Vec`, trait object or future layout crosses
the interface.

Changing method order, arguments or semantics requires a new interface version.
A plugin package version is separate from the ABI version. The included plugins
derive their package descriptors from Cargo version variables, so the release
bump command updates their reported version automatically.

## Verification

Run the plugin unit tests and exercise the actual shared-library ABI:

```sh
cargo test --locked -p zcalld-plugin-api -p zcalld-plugin-aliases -p zcalld-plugin-policy
cargo build --release --locked -p zcalld-plugin-aliases -p zcalld-plugin-policy
cargo run --locked -p zcalld-plugin-api --example check_plugins -- \
  target/release/libzcalld_aliases.so target/release/libzcalld_policy.so
```

The unit tests cover explicit alias selection, UTF-8 names, duplicate and malformed
configuration, replay-safe buffer growth, policy denial, exact body-size limits,
atomic reconfiguration and saturating observation counters. The executable example
loads both real libraries, applies alias selection followed by policy, rejects a
blocked model and an oversized request, and invokes the observation hook.

The gateway integration tests exercise plugins through its HTTP ingress. Memory
and request timings belong to measured gateway workloads, not the interface's
type definitions.


## Optional privacy and community interfaces

The gateway keeps `dev.zcalld.gateway` version 1 unchanged. Additional capabilities
use separate version 1 interfaces. A library can export several interfaces with
`zplugin::export_interfaces!`; each interface binding constructs an instance.
Plugins that share request state between bindings must arrange that sharing
explicitly on the worker thread. The included adapters use worker-local shared
state and never make native proxies `Send` or `Sync`.

| Interface ID | Purpose and contract |
| --- | --- |
| `dev.zcalld.context` | Receives bounded request metadata. `phase=request` precedes request hooks; `phase=route` contains the filtered request after those hooks. HTTP credentials and cookies are excluded. |
| `dev.zcalld.response` | Begins after final upstream headers, then transforms chunks after protocol conversion. A zero result holds output; it does not mean passthrough. The final flush is delivered once. |
| `dev.zcalld.provider` | Returns an owned provider-stream resource for explicitly selected community providers. The resource retains its native library and cancels unfinished work when dropped. |
| `dev.zcalld.retry` | Offers a next public route and an optional patch to an already filtered body. The server independently limits retry eligibility; zero declines. |

Request hooks run in configuration order before route selection. Managed
community hooks are followed by privacy and then group routing. The gateway
validates the modified request before choosing the upstream model. Context
consumers choose the phase they need: community middleware starts one transaction
during `request`; routing can inspect the filtered `route` phase. Request text
must not be persisted by a routing classifier.

Response transforms run in reverse plugin order. Privacy restores placeholders
before community middleware receives the reply, including JSON and placeholders
split across SSE events. The community adapter reconstructs supported reply
objects and usage for the upstream `onResponse` contract. It bounds capture at
16 MiB and opens the Bun process only when enabled provider or middleware code
needs it.

The provider-stream resource is `dev.zcalld.provider-stream`, version 1. Its
operations are head (1), body (2), and cancel (3). Head metadata is bounded at
16 KiB; status 3 means pending. Both head and body polling return promptly so
the gateway can enforce request deadlines and shutdown. Raw body bytes use a
512 KiB credit window in the Bun host; cancellation does not replay a POST.
The native bridge translates the core's UTF-8 JSON body string to the host's
base64 wire representation. Plugin-specific wire formats stay outside the core.

| Application status | Meaning |
| --- | --- |
| 3 | Provider head or body is pending |
| 4 | Privacy policy blocked the request |
| 5 | Required privacy filtering is unavailable |
| 6 | Community plugin operation is unavailable |
| 7 | All members of the requested routing group are unavailable |
| 8 | Routing control or observation failed |

Response buffer retries must preserve exactly one logical write. A plugin may
cache the pending transformed bytes and copy them on retry; it must not advance
its stream twice. The host rejects lengths larger than the supplied buffer.
The native trust boundary still applies: bounds checks do not make untrusted
machine code safe.

## Install community packages

The optional [zcalld-compat component](https://github.com/LIghtJUNction/zcalld-compat)
contains the private privacy worker and pinned Magpie-compatible Bun host. It
opens no separate gateway listener. Neither runtime is started by an unconfigured
core. The native adapters are supplied with zcalld; the helper package and Bun
are needed only for the capabilities that use them.

```sh
zcalld plugins install /absolute/plugin.mjs --name sample --json
zcalld plugins install npm:package-name@1.2.3 --json
zcalld plugins install github:owner/repository --revision FULL_COMMIT_HASH --json
zcalld plugins inspect sample --json
zcalld plugins config sample --file /absolute/private-options.json --json
zcalld plugins enable sample --json
zcalld plugins disable sample --json
zcalld plugins update sample --revision NEW_FULL_COMMIT_HASH --json
zcalld plugins remove sample --json
```

New packages start disabled. Git revisions are immutable full hashes, npm
versions are exact, and copied local packages have recorded SHA-256 digests.
An optional supplied digest must match. Native packages are verified before
activation and loading. Package lifecycle scripts are disabled. These controls
identify what is installed; they do not sandbox code or establish its publisher's
trustworthiness.

The registry, options and copied authentication live beside the selected
configuration, inside its private `extensions` directory. Public list and
inspect results omit credentials and options. Importing an existing Magpie setup
preserves its enabled package choices, custom privacy rules, keyed placeholders,
installed model cache and historical masking records in a separate candidate.
Import never refreshes live credentials.

See the compatibility component's README for model availability, migration
boundaries, public plugin RPC and the persistent authorization session helper.
The local AstrLink Guard path is tested with actual pinned model files; other
explicit model selections fail closed until their adapters are installed.

## Sources

- [zplugin native ABI and lifetime contract](https://github.com/LIghtJUNction/zplugin/blob/main/README.md)
- [zcall semantic and transport boundary](https://github.com/LIghtJUNction/zcall/blob/main/README.md)
