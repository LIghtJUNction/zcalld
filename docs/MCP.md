# Community plugin tools over MCP

zcalld can expose the tools of explicitly enabled community plugins through its
existing HTTP listener. The optional `zcalld-community` native plugin implements
this endpoint. It shares the same Bun host as community providers and middleware;
there is no second gateway or MCP daemon.

The canonical endpoint is `/mcp/zcalld/tools`. `/mcp/magpie/tools` remains an alias
so existing agent configurations can keep working after an authorized migration.
An individual plugin is available at either endpoint followed by `/<prefix>`.
This preserves the behavior of the pinned Magpie
[plugin-tools endpoint](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/gateway/tools_mcp.go).

## Install and configure

Install the optional `zcalld-compat` package and Bun, then install a plugin from an
explicit local source or a pinned Git/npm source:

```sh
zcalld plugins install /absolute/path/tools-weather.mjs --name weather
zcalld plugins enable weather
zcalld plugins runtime configure
zcalld plugins mcp enable
zcalld check
zcalld service restart
```

For a Git source, provide its full immutable commit with `--revision`. For an npm
source, use `npm:package@exact-version`. The plugin manager records the installed
content digest. Installation alone leaves a new plugin disabled. Community
plugins are trusted local code with the user's filesystem and network access;
their tool definitions are not a sandbox.

`runtime configure` materializes the helper's embedded host bundle and updates
the private `extensions/host.json` binding. It does not start Bun, load a plugin,
or refresh an account. Run it after a compatibility-package upgrade so an existing
binding does not continue pointing at an older cached host script. Restart the
gateway after changing the loaded native library or host bundle.

```sh
zcalld plugins mcp show --json
zcalld plugins mcp disable
```

The tool switch is separate from plugin provider/auth functionality. A disabled
tool switch returns an empty tool list and refuses execution while leaving those
other capabilities available. The running adapter reads the switch for each
request. `initialize`, `ping`, notifications, and the empty-list/disabled cases
do not start Bun. Tool discovery and execution start the shared host on demand.

## Protocol and naming

The adapter supports MCP `initialize`, `ping`, `tools/list`, and `tools/call` over
HTTP POST. It negotiates `2025-06-18`, `2025-03-26`, or `2024-11-05`, following
the pinned upstream
[transport contract](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/gateway/search_mcp.go).
GET and DELETE return 405 with `Allow: POST`; there is no server-originated SSE
stream. Notifications return 202 with an empty body and never execute tools.
An initialization response can assign an opaque `Mcp-Session-Id`; this label is
not an authentication credential or a stateful server session.

Aggregate tool names use `<plugin-prefix>_<tool>`. The prefix removes the common
`middleware-`, `opencode-`, `tools-`, `page-`, or `agent-` prefix and `-auth`
suffix, normalizes unsupported characters, and is capped at 24 ASCII characters.
Aggregate names are capped at 64 characters, with `_2`, `_3`, and subsequent
suffixes resolving collisions in plugin order. An individual-plugin endpoint
uses the original tool name. Disabled or failed plugins contribute no tools.

Tools follow the existing OpenCode hook shape:

```js
export default async () => ({
  tool: {
    echo: {
      description: "Repeat a supplied string",
      args: {type: "object", properties: {text: {type: "string"}}},
      async execute(args, context) {
        context.abort.throwIfAborted()
        return args.text
      },
    },
  },
})
```

Tool arguments, descriptions, and results are not written into gateway access
logs. A tool's result is returned to the calling agent without applying the AI
request privacy transform, matching upstream. If the agent subsequently includes
that result in a model request, the ordinary gateway privacy pipeline applies.

## Authorization and limits

The core validates Host, Origin, and caller authentication before invoking the
native HTTP capability. It forwards only selected MCP headers, the verified
caller identifier, peer-loopback status, and configured bounds. Authorization,
Cookie, query parameters, and gateway credentials are not sent to the tool host.

The migrated private registry stores `mcp_permissions` by caller identifier,
without gateway secrets. An empty permission list allows every tool; `-` denies
all; `magpie-tools` allows this server; `magpie-tools/<name>` allows a specific
tool. The legacy server permission name is retained for both URL aliases.
Issued keys inherit the intersection of their own and their issuer's permissions.
Read-only, browser-only, disabled, expired, or unmapped verified keys are refused.
These rules follow upstream's
[MCP access implementation](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/access/mcp.go).

A loopback request without a verified key can use the endpoint only after the
core's configured local-access policy has accepted it. A verified restricted key
remains restricted even on loopback. Remote requests need an accepted key with
matching private MCP permission metadata. Creating or changing caller keys must
also update this derived permission state; an unknown identifier does not become
an unrestricted caller.

The request body is capped at 4 MiB and a batch at 128 messages. The response is
bounded by the gateway's configured response limit, at most 16 MiB. HTTP body
delivery uses the existing 64 KiB frames and 512 KiB credit window. Tool calls
have a maximum execution timeout of 55 seconds, bounded further by the gateway
deadline. Caller and issuer concurrency limits use a cancellable FIFO queue.
Cancelled or oversized tool operations are not replayed.

Token and currency budgets are not implemented for MCP tools. Migration and
plugin configuration reject those nonempty restrictions with an explicit error.
They are never silently converted into unlimited tool access. This limitation
does not prevent importing unrestricted keys or tool/concurrency restrictions.

## Migration and upgrade

`import-magpie` now carries over the effective built-in tool switch and permission
metadata. It reads `plugins.yml`, the persisted `plugin-tools.json` note, and
`caller-keys.json` without starting community code. In upstream, an enabled plugin
known to supply tools keeps the built-in tool capability in use; this effective
state is retained. YAML input, nesting, alias expansion, and issuer chains have
explicit bounds.

To update an already staged private configuration after installing the new
helper, use its actual extension directory:

```sh
zcalld-pluginctl --root /absolute/staged/extensions import-mcp \
  --source /absolute/original/magpie-config
zcalld-pluginctl --root /absolute/staged/extensions runtime configure
```

Both commands are filesystem operations. They do not run a live account host or
switch a service. The original configuration is read only. Keep the original MCP
URL until the normal migration transaction takes over its listening address;
copying a URL alone does not preserve a service supplied by the old process.

## Verification

The portable integration suite installs and enables an isolated local `.mjs`
fixture and exercises the real native HTTP gateway:

```sh
python3 scripts/test-mcp.py \
  --binary /absolute/zcalld \
  --community-plugin /absolute/libzcalld_community.so \
  --pluginctl /absolute/zcalld-pluginctl \
  --bun /absolute/bun
```

Use the platform's `.dylib` or `.dll` and executable names on macOS or Windows.
The suite covers initialization without a host, real tool results, tool errors,
notifications, batches, aliases, permission filtering, disabled plugins,
multiframe output, response bounds without replay, a single shared host across
workers, and cancellation/drain at gateway shutdown. Owned Bun exit is checked
with process identities on POSIX and an opened process handle plus
`WaitForSingleObject` on Windows; an unrelated reused PID is not a shutdown
signal.
No real provider, credential, model call, or paid service is used.
