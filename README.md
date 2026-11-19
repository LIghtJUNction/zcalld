# zcalld

A compact Linux AI gateway built on [zcall](https://github.com/LIghtJUNction/zcall), with optional native [zplugin](https://github.com/LIghtJUNction/zplugin) extensions.

One CLI and one foreground service connect AI clients to explicitly configured upstreams. The core handles HTTP, routing, credentials and protocol conversion. Model aliases and access policy are separate `.so` plugins. There is no GUI, embedded browser, JavaScript runtime, database, billing table or hardcoded paid feature gate.

## Install

On Arch Linux, choose the source package or the prebuilt package:

```sh
yay -S zcalld
# or
yay -S zcalld-bin
```

Native x86_64 and aarch64 archives are available in [Releases](https://github.com/LIghtJUNction/zcalld/releases). They contain an `install.sh`, the executable, optional plugins, systemd units, configuration, man page and license. The runtime uses the distribution's libcurl and CA certificates.

## Start

```sh
zcalld init
# Edit ~/.config/zcalld/config.conf for your upstream and installed model.
zcalld check
zcalld serve
```

The starter config uses an Ollama-compatible upstream at `http://127.0.0.1:11434/v1`, with the public route `local`. Change its upstream model to one actually installed on your machine. zcalld does not download or run a model.

In another terminal:

```sh
eval "$(zcalld env openai)"
curl "$OPENAI_BASE_URL/chat/completions" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"local","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Configuration resolves from `--config PATH`, then `ZCALLD_CONFIG`, then `$XDG_CONFIG_HOME/zcalld/config.conf`, or `$HOME/.config/zcalld/config.conf`. `init` creates a mode-0600 file and never overwrites an existing one.

## Configure providers and routes

```ini
[server]
listen = 127.0.0.1:9415
workers = 4
queue = 16
token_env = ZCALLD_TOKEN

[provider cloud]
base_url = https://api.example.com/v1
protocol = chat
api_key_env = CLOUD_API_KEY

[route coding]
provider = cloud
model = your-upstream-model
```

Set the two environment variables before running the service. `ZCALLD_TOKEN` authenticates clients to this gateway; `CLOUD_API_KEY` authenticates the gateway to its upstream. The gateway builds upstream authorization from provider configuration and does not forward the caller's credentials, cookies or query-string keys.

Protocol names are `chat`, `responses`, `anthropic`, `gemini`, `completions` and `interactions`. The provider's authentication header defaults to Bearer for OpenAI formats, `x-api-key` for Anthropic, and `x-goog-api-key` for Gemini. `auth = bearer|x-api-key|x-goog-api-key|none` overrides this choice.

The configuration format is deliberately small: named sections, `key=value`, optional surrounding quotes, and full-line `#` or `;` comments. Duplicate keys, unknown options, missing providers, invalid URLs and missing credential variables fail validation. Plugin sections run in declaration order.

## Gateway API

| Endpoint | Protocol |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/responses` | OpenAI Responses |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1beta/models/{model}:generateContent` | Gemini GenerateContent |
| `POST /v1beta/models/{model}:streamGenerateContent` | Gemini SSE |
| `POST /v1/completions` | OpenAI Completions |
| `POST /v1beta/interactions` | Gemini Interactions |
| `GET /v1/models` | Configured public route names |
| `GET /healthz` | Process readiness and version |

Same-protocol requests retain provider-specific fields and exact JSON values, with only the selected model and streaming fields adjusted. Responses stream incrementally through libcurl; the daemon does not collect a same-protocol SSE response before forwarding it.

Cross-protocol requests, complete responses and SSE use zcall's semantic interfaces. Conversion is strict: unsupported metadata or reasoning semantics produce an explicit error rather than disappearing. For example, some Chat-to-Anthropic streams need input-token usage before Chat provides it. See [protocol and resource limits](docs/ARCHITECTURE.md) for the supported contract.

The transport reuses connections per worker. A non-rewindable upload callback prevents automatic replay after generation upload begins. Redirects are not followed; interrupted responses are terminated without a successful final chunk. This avoids silently replaying a generation after an ambiguous failure.

## CLI

```sh
zcalld check --config ./gateway.conf --json
zcalld models --json
zcalld providers --json
zcalld plugins --json
zcalld env openai
zcalld env anthropic
zcalld env gemini
zcalld health
zcalld --version
```

`env` emits shell exports and references the configured token variable by name. It does not print the token value. `check` validates credentials and actually loads configured plugins so an incompatible ABI is caught before serving.

## Optional native plugins

```ini
[plugin aliases]
path = /usr/lib/zcalld/plugins/libzcalld_aliases.so
config = fast=coding

[plugin policy]
path = /usr/lib/zcalld/plugins/libzcalld_policy.so
config = allow=coding
```

The aliases plugin changes `fast` into the `coding` route. The policy plugin then permits `coding` and rejects other names. Multiline plugin configuration uses `config_file = ./policy.conf`.

Each worker creates its own zplugin instances. Trusted libraries are loaded only from configured paths and remain pinned until process exit. Disabled plugins are not loaded. The versioned SDK offers routing, request transformation and observation hooks using caller-owned buffers; [PLUGINS.md](docs/PLUGINS.md) describes the ABI and a real dynamic-loading example.

## systemd

Use the packaged user service after configuring the CLI:

```sh
systemctl --user enable --now zcalld.service
journalctl --user -u zcalld.service
```

For a machine-wide service, edit `/etc/zcalld/config.conf`. Put required environment variables in `/etc/zcalld/credentials`, restrict that file to the administrator, then start the service:

```sh
sudo systemctl enable --now zcalld.service
sudo systemctl status zcalld.service
```

Both units use `Type=notify`; readiness is sent after the listener and all workers initialize. SIGTERM stops admission and cancels outstanding upstream transfers. The system unit uses a dynamic unprivileged identity and filesystem restrictions. Configuration and plugin upgrades take effect with `systemctl restart`.

## Resource model

The defaults are 4 workers and 16 queued sockets. One active generation occupies a worker; excess accepted connections receive HTTP 503 when the queue is full. The daemon creates no model subprocesses and no thread per request. Upstream TLS and HTTP live in the shared system libcurl.

Headers are limited to 16 KiB; request bodies default to 4 MiB and responses to 16 MiB. Both body limits can be configured up to 16 MiB. Input, conversion and plugin workspaces have explicit caps. Cross-protocol SSE has a 64 KiB framing/payload limit and 256 KiB aggregate/pending regions. [The benchmark script](https://github.com/LIghtJUNction/zcalld/blob/main/scripts/benchmark.py) measures the actual daemon and uses a deterministic loopback upstream.

Pricing, subscriptions and payment decisions belong to the upstream service. The client has no hardcoded prices, plans, license checks or paid routing gates.

See [measured performance](docs/PERFORMANCE.md) for three runs with and without native plugins, including raw results and limits.

## Build and release

Use Rust 1.100.0, Python 3.11+, Make, and libcurl development headers. On Arch, install `rust`, `curl`, `base-devel`, `python` and `shellcheck`; on Debian/Ubuntu install the Rust toolchain plus `libcurl4-openssl-dev`, `pkg-config`, `build-essential`, `python3` and `shellcheck`.

```sh
make check
make package
```

`make check` runs formatting, Clippy, Rust tests, real gateway integration and release-transaction regression tests. The tests use local mock upstreams and consume no provider credits. Day-to-day push and PR events do not trigger remote CI; the same checks run for formal release builds.

For a version bump and release:

```sh
make bump BUMP=patch
make release
# Or let the workflow make the version commit and release:
gh workflow run bump.yml -f bump=patch
```

The workflow updates workspace/package versions, SDK dependency pins, lockfile and changelog; builds native x86_64/aarch64 archives; publishes a GitHub release and checksums; then regenerates and pushes both AUR package recipes. [RELEASING.md](docs/RELEASING.md) explains dry runs, exact-tag validation and recovery.

## Scope

The local-gateway role is inspired by [Magpie](https://github.com/yetone/magpie). zcalld 0.1 provides explicit routes, native plugins and protocol conversion. OAuth account discovery, agent configuration editing, persistent usage storage, quota-aware failover and graphical management are not included in this release.

MIT licensed. The protocol and plugin libraries are developed independently in Project Z.
