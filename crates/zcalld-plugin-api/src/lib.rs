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
