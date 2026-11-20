# zcalld

A native local AI gateway for Linux, macOS and Windows, built on [zcall](https://github.com/LIghtJUNction/zcall), [zconv](https://github.com/LIghtJUNction/zconv), [zplugin](https://github.com/LIghtJUNction/zplugin) and [zsys](https://github.com/LIghtJUNction/zsys).

The CLI daemon owns transport, bounded queues, credentials and protocol conversion. Privacy, community providers and model-group decisions are optional plugins. [zcall-gui](https://github.com/LIghtJUNction/zcall-gui) is a separate native application built with [zui](https://github.com/LIghtJUNction/zui); closing it leaves the gateway running. There is no embedded browser in either application. The optional compatibility package supplies the Go privacy and advanced-rule workers and one lazy Bun host for existing JavaScript community plugins. [zcall-omarchy](https://github.com/LIghtJUNction/zcall-omarchy) is a second, independent frontend that uses Omarchy's native plugin and menu interfaces without another background gateway.

## Install

On Arch Linux, choose one core package and add the capabilities you use:

```sh
paru -S zcalld-bin
# Source build alternative: paru -S zcalld
paru -S zcalld-compat    # Privacy model, community plugins and subscription accounts
paru -S zcall-gui       # Optional native desktop application
paru -S zcall-omarchy   # Optional Omarchy bar and terminal menu frontend
```

[Releases](https://github.com/LIghtJUNction/zcalld/releases) provide Linux x86_64/aarch64, Windows x86_64 and macOS Apple Silicon/Intel archives with platform-native plugin libraries. Linux and macOS use the system libcurl; the Windows archive includes its TLS transport DLLs. These are real operating-system dependencies, not an embedded browser runtime.

```sh
zcalld init
# Edit the generated provider and route to match an available upstream.
zcalld check --json
zcalld serve
```

The default configuration uses an Ollama-compatible upstream. zcalld does not download or execute an LLM. Configuration follows `--config`, `ZCALLD_CONFIG`, then the platform user configuration directory. Run `zcalld --help` for the exact local path and command list.

## Preserve an existing Magpie setup

Migration first creates a private copy; it does not stop Magpie or change agent settings:

```sh
zcalld migrate magpie --json
zcalld migrate magpie --apply --json
zcalld migrate verify --json
zcalld check --json
```

The importer preserves public model IDs and catalog metadata, routes, accounts, enabled community packages, the reversible privacy key, custom rules and history. It reports unresolved routes or unsupported rules explicitly. The old TCP address can remain unchanged while zcalld's own CLI and GUI prefer a private Unix socket on Linux and macOS.

See [MIGRATION.md](docs/MIGRATION.md) for the service transaction and rollback. Credential refresh ownership matters: an active native Codex login remains externally managed and is reread for each request. The gateway does not race another process to rotate that account's refresh token. Explicit handoff requires the recorded original owners to have exited.

## Configure and use

```ini
[server]
listen = 127.0.0.1:9415
unix_socket = auto
workers = 4
queue = 16
token_env = ZCALLD_GATEWAY_TOKEN

[provider cloud]
base_url = https://api.example.com/v1
protocol = chat
api_key_env = CLOUD_API_KEY

[route coding]
provider = cloud
model = upstream-model
```

Put secrets in the private `credentials` file beside the configuration, as `NAME="value"` assignments. Environment values take precedence. Provider credentials and caller credentials are separate; incoming authorization, cookies and query keys are not forwarded upstream. A non-loopback listener requires a LAN-capable credential.

Protocol names are `chat`, `responses`, `anthropic`, `gemini`, `completions` and `interactions`. Native per-protocol endpoint overrides accept a base URL or a complete endpoint URL. Same-protocol forwarding retains provider extensions. Cross-protocol conversion uses strict zcall semantics and rejects unsupported information loss.

| Endpoint | Purpose |
| --- | --- |
| `/v1/chat/completions`, `/v1/responses` | OpenAI-style generation |
| `/v1/messages` | Anthropic Messages |
| `/v1beta/models/{model}:generateContent` | Gemini generation |
| `/v1beta/models/{model}:streamGenerateContent` | Gemini streaming |
| `/v1/completions`, `/v1beta/interactions` | Additional protocol ingress |
| `GET /v1/models` | Public routes with preserved catalog metadata |
| `GET /healthz` | Readiness |
| `POST /mcp/zcalld/tools`, `POST /mcp/magpie/tools` | Optional community-tool MCP; the legacy address remains valid |

## Management

```sh
zcalld status --json
zcalld providers list --json
zcalld providers add work --base-url https://api.example.com/v1 --api-key-stdin
zcalld routes list --json
zcalld groups set group/work --members work/model-a,work/model-b --mode rotate
zcalld agents detect --json
zcalld agents apply claude --route group/work --json
zcalld agents revert claude --json
zcalld plugins list --json
zcalld plugins mcp show --json
zcalld accounts list --json
zcalld privacy show --json
zcalld profiles save work --json
zcalld library list --json
zcalld library targets --json
zcalld usage summary --json
zcalld update check --json
```

Agent edits preserve unrelated fields and keep exact-byte backups. Revert refuses to overwrite later user edits unless explicitly forced. Account controls operate on the actual private compatibility-host store. Community packages must be deliberately enabled; trusted native libraries run inside the daemon process.

The [agent library](docs/LIBRARY.md) manages MCP definitions, instruction sets and skill files separately from gateway providers. Preview/apply/revert keeps exact owned bytes and refuses conflicting user edits. [Community MCP](docs/MCP.md) uses the optional HTTP plugin interface and the same shared Bun process as community providers; initialize and ping do not start that process. Caller-key permissions remain separate from upstream credentials.

Privacy runs before model-group classification. JSON and fragmented SSE responses restore masked values. If a required privacy worker is unavailable, requests fail closed before upstream generation. Group modes include order, smart, rotate, usage, pace, weight and drain; turn affinity is bounded. Explicit upstream 429/503 responses may select another member before downstream headers are committed, with at most four total attempts. An already-started stream or an ambiguous network failure is never replayed. [Advanced rules](docs/ROUTING.md) support IANA time zones, RE2 message conditions and trusted workspace conditions through a lazy optional worker; ordinary groups do not start it.

## Services

Linux user service, after configuration:

```sh
systemctl --user daemon-reload
systemctl --user enable --now zcalld.service
journalctl --user -u zcalld.service
```

The Linux system service runs as the dedicated `zcalld` user and reads `/var/lib/zcalld/config.conf`. Initialize that private state directory before starting it; `/etc/zcalld/config.conf` is a template. Both units use readiness notification, private socket permissions and a 60-second stop budget so an in-progress credential rotation can finish writing.

On macOS, `zcalld service start` manages a per-user launchd agent. On Windows it manages a per-user scheduled task. `service status` queries the actual platform manager and checks gateway health. Unix sockets are used where supported; Windows clients use loopback TCP.

## Build, submodules and release

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/zcalld.git
cd zcalld
make check
make package
make bump BUMP=patch
make release
# Or: gh workflow run bump.yml -f bump=patch
```

Use Rust 1.100.0, Python 3.11+, a native compiler and libcurl development libraries. Project Z libraries are actual Cargo patches pointing to pinned git submodules. The optional GUI, Omarchy frontend and compatibility repositories are also submodules; they keep independent releases.

Day-to-day checks run locally. Formal release workflows validate five native targets, including real privacy, routing and community MCP worker calls, publish versioned archives plus SHA256SUMS, and update source/binary AUR recipes. Linux also runs the migration transaction and rollback fixtures before publication. Source archives recursively include the exact tracked submodule commits; private and untracked development files are excluded. The bump command updates workspace versions, dependency pins, lockfile and changelog as one recoverable operation.

[RELEASING.md](docs/RELEASING.md) describes publishing and recovery. [PLUGINS.md](docs/PLUGINS.md) describes the SDK. [PERFORMANCE.md](docs/PERFORMANCE.md) contains measured daemon baselines, with versions and workload limitations. GUI renderer and native-platform evidence live in its own repository. No client pricing table, subscription tier or paid feature gate is built into these projects.

MIT licensed. Magpie compatibility code retains upstream notices in the optional compatibility repository.
