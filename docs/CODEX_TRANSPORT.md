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

The gateway forwards eleven explicitly allowed response headers: the primary and secondary used-percent, window-minutes and reset-at fields; credits-has-credits, credits-unlimited and credits-balance; limit-name; and rate-limit-reached-type, each under the `x-codex-` prefix. Other private or unrelated response headers do not become allowed because of their prefix.

[Codex 0.170.0 parses these headers into rate-limit snapshots](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/codex-api/src/rate_limits.rs). The installed-client fixture sends synthetic window and credit values from the fake upstream and verifies their exact values in the real client's `TokenCount` rollout events after HTTP/SSE completion. Those rollout files exist only in its disposable private home. No WebSocket-only `codex.rate_limits` event is injected into the SSE body.

This proves response metadata reaches the client and is parsed. It does not prove that the interactive status bar shows account quota: that UI also uses the separate account-usage API. This change does not add account-usage or Codex model-catalog endpoints, and the fixture makes no real quota or account request.

## Compressed HTTP requests

The handshake and the request body are separate compatibility requirements. Codex with ChatGPT authentication sends zstd after falling back to HTTP. The gateway accepts identity, gzip and zstd, decodes before routing and privacy, and normalizes the downstream request metadata to the decoded body. It removes the old content-encoding and transfer-encoding fields and sets the decoded content length.

Both the received bytes and decoded bytes must fit max_body_bytes. The zstd history window is capped at 8 MiB; gzip uses a 32 KiB window. Native decoding produces at most 16 KiB per output step, checks the original receive deadline and shutdown flag between steps, rejects incomplete or corrupt frames, and frees the decoder on every exit. These are cooperative checks between native calls, not hard real-time preemption.

Linux links distribution zstd and zlib packages. macOS links zstd statically and uses system zlib; Windows links both codecs statically with the same dynamic CRT as the Rust program. No Rust crate dependency is added, and standalone updates on macOS/Windows do not gain a new codec DLL or dylib requirement. Distributed notices accompany the binaries.

The [installed-client fixture record](evidence/codex-transport-fixture.json) covers the actual Codex 0.170.0 binary with temporary credentials and no external network. Its ChatGPT case requires zstd at ingress, ordinary JSON at the fake provider, no forwarded caller Authorization, and one completed HTTP/SSE response. Separate native privacy acceptance checks masking and restoration for both encodings.
