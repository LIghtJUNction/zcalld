# Native plugins

zcalld loads optional native plugins through `zplugin 0.3.0`. The gateway uses
`zcall` for protocol handling; plugins add explicit routing and request policy
without a second runtime or background plugin process.

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

Routing and request hooks run once per plugin in configuration order. A request
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

## Sources

- [zplugin native ABI and lifetime contract](https://github.com/LIghtJUNction/zplugin/blob/main/README.md)
- [zcall semantic and transport boundary](https://github.com/LIghtJUNction/zcall/blob/main/README.md)
