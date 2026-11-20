#![no_std]
//! Version 1 of the zcalld native plugin boundary.
//!
//! All inputs are borrowed for one call. The host owns every output buffer.
//! Native plugins execute with the host's privileges and must be trusted.
//! Change the interface version when changing method order, arguments or semantics.

/// Stable protocol numbers at the C ABI boundary; independent of Rust enum layout.
pub mod protocol {
    pub const OPENAI_CHAT: u32 = 1;
    pub const OPENAI_RESPONSES: u32 = 2;
    pub const ANTHROPIC_MESSAGES: u32 = 3;
    pub const GEMINI_GENERATE_CONTENT: u32 = 4;
    pub const OPENAI_COMPLETIONS: u32 = 5;
    pub const GEMINI_INTERACTIONS: u32 = 6;

    pub const fn is_supported(value: u32) -> bool {
        matches!(value, 1..=6)
    }
}

/// Positive application statuses preserve their meaning across plugin languages.
pub mod status {
    /// The plugin's model access policy rejected the selected route.
    pub const MODEL_DENIED: i32 = 1;
    /// The plugin's configured request-body limit was exceeded.
    pub const REQUEST_TOO_LARGE: i32 = 2;

    // Optional application statuses for dev.zcalld.provider-stream resources.
    // These expose static failure categories, never upstream text or secrets.
    /// The provider RPC reported a request failure.
    pub const PROVIDER_REQUEST_FAILED: i32 = 9;
    /// The worker exited or its transport/protocol became unavailable.
    pub const PROVIDER_TRANSPORT_FAILED: i32 = 10;
    /// The overall provider request deadline expired.
    pub const PROVIDER_TIMED_OUT: i32 = 11;
    /// Response metadata was malformed, oversized, or arrived out of order.
    pub const PROVIDER_HEAD_INVALID: i32 = 12;
}

/// Maximum UTF-8 model or route-name length accepted by the host.
pub const MAX_MODEL_BYTES: usize = 4096;

/// Ordered hooks for one worker's independent plugin instance.
///
/// A successful `route` or `request` returning zero leaves its input unchanged.
/// A positive result is the exact initialized byte count in `output`. Returning
/// a count larger than the supplied buffer is an interface violation.
///
/// `BufferTooSmall` must leave plugin state unchanged: the host may repeat that
/// hook with a larger bounded buffer. Other errors reject the current operation.
/// Hooks must not retain borrowed inputs or output buffers beyond the call.
#[zplugin::interface(id = "dev.zcalld.gateway", version = 1)]
pub trait GatewayPlugin {
    /// Called once before this instance serves requests. Configuration is explicit.
    fn configure(&mut self, config: &str) -> Result<(), zplugin::Error>;

    /// Return a replacement route/model name as UTF-8, or zero to keep `model`.
    fn route(&mut self, model: &str, output: &mut [u8]) -> Result<u64, zplugin::Error>;

    /// Transform the JSON request before protocol conversion, or return zero.
    ///
    /// The host validates rewritten requests through its normal protocol path.
    /// `protocol` is one of the constants in [`protocol`]. HTTP headers, provider
    /// credentials and streaming response fragments are outside this hook.
    fn request(
        &mut self,
        protocol: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<u64, zplugin::Error>;

    /// Observe one completed call without receiving prompts, tokens or credentials.
    ///
    /// Duration is monotonic milliseconds. Byte counts describe the host's wire
    /// body accounting. An observer failure is surfaced by the host; a response
    /// already written to the client cannot be changed retrospectively.
    fn observe(
        &mut self,
        status: u32,
        elapsed_ms: u64,
        input_bytes: u64,
        output_bytes: u64,
    ) -> Result<(), zplugin::Error>;
}

/// Parse a Cargo-provided numeric version component at compile time.
///
/// Use `env!("CARGO_PKG_VERSION_MAJOR")`, `MINOR` and `PATCH` in a plugin's
/// zplugin export descriptor so release metadata follows the Cargo version.
pub const fn version_component(value: &str) -> u32 {
    let bytes = value.as_bytes();
    assert!(!bytes.is_empty(), "empty plugin version component");
    let mut number = 0_u32;
    let mut index = 0;
    while index < bytes.len() {
        let digit = bytes[index];
        assert!(
            digit >= b'0' && digit <= b'9',
            "invalid plugin version component"
        );
        number = match number.checked_mul(10) {
            Some(value) => value,
            None => panic!("plugin version component overflow"),
        };
        number = match number.checked_add((digit - b'0') as u32) {
            Some(value) => value,
            None => panic!("plugin version component overflow"),
        };
        index += 1;
    }
    number
}

/// Optional response transformer, bound separately from GatewayPlugin v1.
/// begin resets request-local state; write receives the caller's protocol after
/// conversion. A zero result means output is held, not passthrough. final_chunk
/// is 0 or 1. BufferTooSmall retries must replay the same input exactly.
#[zplugin::interface(id = "dev.zcalld.response", version = 1)]
pub trait ResponsePlugin {
    fn configure(&mut self, config: &str) -> Result<(), zplugin::Error>;
    fn begin(&mut self, status: u32, content_type: &str) -> Result<(), zplugin::Error>;
    fn write(
        &mut self,
        input: &[u8],
        final_chunk: u32,
        output: &mut [u8],
    ) -> Result<u64, zplugin::Error>;
}

/// Optional custom provider transport. The request is the documented JSON
/// envelope, never a gateway credential. The returned resource has type
/// dev.zcalld.provider-stream v1. Operation 1 reads response metadata JSON,
/// operation 2 reads a body fragment (empty means EOF), and 3 cancels.
/// Resource calls obey replay-safe BufferTooSmall and own cancellation on drop.
#[zplugin::interface(id = "dev.zcalld.provider", version = 1)]
pub trait ProviderPlugin {
    fn configure(&mut self, config: &str) -> Result<(), zplugin::Error>;
    fn fetch(&mut self, request: &[u8]) -> Result<zplugin::OwnedResource, zplugin::Error>;
}

/// Optional bounded local request context, including the original JSON request.
/// The host never adds HTTP authentication or cookie headers to this envelope.
#[zplugin::interface(id = "dev.zcalld.context", version = 1)]
pub trait ContextPlugin {
    fn configure(&mut self, config: &str) -> Result<(), zplugin::Error>;
    fn context(&mut self, metadata: &str) -> Result<(), zplugin::Error>;
}

/// Optional routing retry after a complete retryable upstream error head.
/// The host decides whether a request is eligible; transport failures are never
/// permission to replay. A BufferTooSmall retry must not advance selection or
/// repeat external decisions. Bodies have already passed privacy filtering.
#[zplugin::interface(id = "dev.zcalld.retry", version = 1)]
pub trait RetryPlugin {
    fn configure(&mut self, config: &str) -> zplugin::Result<()>;
    /// Return a public route name, or zero to decline this retry.
    fn retry(
        &mut self,
        status: u32,
        previous_model: &str,
        output: &mut [u8],
    ) -> zplugin::Result<u64>;
    /// Return a replacement filtered body, or zero to keep the input.
    fn request(
        &mut self,
        protocol: u32,
        filtered_body: &[u8],
        output: &mut [u8],
    ) -> zplugin::Result<u64>;
}

/// Optional application HTTP endpoint, separate from AI request middleware.
/// The core authenticates the connection and validates HTTP framing first.
/// accepts returns 0 (decline), 1 (owned), or 3 (owned + trusted route context).
/// Flag 3 requires a host supporting route context; older hosts reject it. All
/// other flag combinations are invalid. It must not execute a tool or allocate
/// request-specific state. Conflicting endpoint owners are rejected.
///
/// metadata is UTF-8 JSON with method, path (without query), a bounded allowlist
/// of protocol headers, loopback, verified caller_key_id or null, timeout_ms,
/// and max_response_bytes. Flag 3 also supplies raw query (the plugin MUST
/// validate its own allowlist) and routing.providers/routes from loaded config.
/// Providers expose id, plugin, account only. Routes expose id, dynamic, provider,
/// model; dynamic groups have null provider/model. Caller headers cannot override
/// any identity. untrusted_headers contains all ChatGPT-Account-Id header values
/// solely for a plugin's reject-only consistency check; duplicates/empty values
/// must be rejected, never used to select an account or grant permission.
/// Configured provider credentials/URLs, Authorization and cookies are omitted.
/// Query and consistency hints remain untrusted input and must not be logged.
/// body is borrowed for this call and is not implicitly AI-redacted.
///
/// handle returns the existing dev.zcalld.provider-stream v1 HTTP resource:
/// operation 1 yields status/headers JSON; 2 yields body bytes (zero is EOF);
/// 3 cancels. PluginStatus(3) means pending. Output retries must be replay-safe.
/// Tools must not be executed again to satisfy a response-buffer retry.
#[zplugin::interface(id = "dev.zcalld.http", version = 1)]
pub trait HttpPlugin {
    fn configure(&mut self, config: &str) -> zplugin::Result<()>;
    fn accepts(&mut self, path: &str) -> zplugin::Result<u32>;
    fn handle(&mut self, metadata: &str, body: &[u8]) -> zplugin::Result<zplugin::OwnedResource>;
}
