# Codex transport negotiation

Codex 0.170.0 can start a Responses request by negotiating WebSocket transport. A gateway that serves Responses over ordinary HTTP must return the client's supported negotiation result. Returning HTTP 400 ends the request before Codex can use the otherwise working HTTP/SSE endpoint.

zcalld returns **426 Upgrade Required** for a complete, body-free HTTP/1.1 WebSocket handshake. Codex then submits its Responses request over HTTP and reads the streamed SSE response. This follows the explicit fallback branch in [Codex 0.170.0's client](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/core/src/client.rs).

## Scope

The gateway does not return 101 or establish a WebSocket session. This is transport negotiation for clients that implement HTTP fallback. A client that requires a persistent WebSocket session still needs a WebSocket-capable adapter.

The check belongs to HTTP transport and does not hard-code an AI endpoint. It requires GET, HTTP/1.1, Host, the WebSocket upgrade and connection tokens, version 13, a valid 16-byte nonce, and no request body or transfer encoding. Malformed upgrades remain rejected. Browser Origin restrictions, duplicate-header rejection and Expect handling retain their existing precedence.

The subsequent HTTP request uses the same authentication, route selection, privacy plugins, provider handling and response conversion as other requests. No provider or Agent configuration needs to be rewritten to negotiate this fallback.

## Validation

The transport regressions exercise TCP negotiation followed by a real local HTTP/SSE generation fixture, the private Unix socket, and malformed upgrades that must not become successful negotiations. An additional installed-Codex fixture uses a temporary home and a local synthetic provider; it does not use real accounts or contact a paid model. Its result is recorded separately from ordinary HTTP integration checks.

## ChatGPT endpoint path

The built-in OpenAI provider chooses different default URL paths according to its authentication mode. In [Codex 0.170.0 provider selection](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/model-provider-info/src/lib.rs), API-key requests use `/v1`, while ChatGPT requests use `/backend-api/codex`. The Responses client appends `/responses` to that base.

zcalld accepts the exact `/backend-api/codex/responses` path as a Responses ingress alias. It uses the same authentication, body limits, decoding, privacy, routing and upstream protocol path as `/v1/responses`. This does not expose arbitrary `/backend-api` paths or add account, model-catalog or quota endpoints.

The installed-client fixture preserves each built-in provider's default path and replaces only its scheme and authority with a server inside the isolated network namespace. It asserts that both the ChatGPT WebSocket handshake and its subsequent zstd POST use `/backend-api/codex/responses`. Its recording proxy forwards that path unchanged; the actual gateway must map it to the configured upstream Responses endpoint. The API-key case separately verifies `/v1/responses`.

The earlier transport fixture explicitly overrode every built-in provider to `/v1`. It proved negotiation and decoding but missed the ChatGPT default-path failure. The updated regression demonstrates the installed pre-alias gateway returning 404 before testing the fixed gateway with the same client and fake credentials. No real default external URL is contacted.

## Quota response metadata

Codex can report both its default allowance and separate model allowances in the same upstream response. zcalld preserves the default `x-codex` family and named families such as `x-codex-bengalfox` and `x-codex-other-v2`. The [Codex 0.170.0 header parser](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/codex-api/src/rate_limits.rs) discovers a named family from its primary-used-percent header and normalizes its ID by replacing hyphens with underscores.

| Field | Forwarding rule |
| --- | --- |
| Primary and secondary used-percent, window-minutes, reset-at | Default family and named Codex families |
| Limit name | Default family and named Codex families |
| Credits has-credits, unlimited, balance | The three shared `x-codex-credits-*` fields |
| Rate-limit-reached type | The fixed `x-codex-rate-limit-reached-type` field |

A named family consists of nonempty ASCII letter/digit segments separated by single hyphens. Matching ignores header-name case and preserves the upstream values. Unrecognized suffixes, empty family segments, private headers and non-Codex families remain excluded. The filter does not allocate a lowercase copy of each name; the existing 16 KiB upstream-header bound applies to all accepted and rejected fields together.

The same response path preserves these fields for JSON, SSE and upstream 429/503 responses. The HTTP regression checks two named families, mixed-case names, exact numeric strings, shared credits and rejected private fields. The installed-client fixture uses separate default and named-family cases under both API-key and ChatGPT authentication. Codex records the last rate-limit update in each completed TokenCount rollout event, so each family is asserted independently against exact synthetic values.

These fixtures use disposable private homes and local synthetic providers. They do not read the user's Codex settings or contact a real account. The interactive status bar also uses the separate account-usage API; response-header forwarding does not add that endpoint or change its display rules.

## Compressed HTTP requests

The handshake and the request body are separate compatibility requirements. Codex with ChatGPT authentication sends zstd after falling back to HTTP. The gateway accepts identity, gzip and zstd, decodes before routing and privacy, and normalizes the downstream request metadata to the decoded body. It removes the old content-encoding and transfer-encoding fields and sets the decoded content length.

Both the received bytes and decoded bytes must fit max_body_bytes. The zstd history window is capped at 8 MiB; gzip uses a 32 KiB window. Native decoding produces at most 16 KiB per output step, checks the original receive deadline and shutdown flag between steps, rejects incomplete or corrupt frames, and frees the decoder on every exit. These are cooperative checks between native calls, not hard real-time preemption.

Linux links distribution zstd and zlib packages. macOS links zstd statically and uses system zlib; Windows links both codecs statically with the same dynamic CRT as the Rust program. No Rust crate dependency is added, and standalone updates on macOS/Windows do not gain a new codec DLL or dylib requirement. Distributed notices accompany the binaries.

The [installed-client fixture record](evidence/codex-transport-fixture.json) covers the actual Codex 0.170.0 binary with temporary credentials and no external network. Its ChatGPT case requires zstd at ingress, ordinary JSON at the fake provider, no forwarded caller Authorization, and one completed HTTP/SSE response. Separate native privacy acceptance checks masking and restoration for both encodings.
