//! Shared HTTP framing with the host-owned bounded request decoder.
use std::time::Duration;
pub use zcall::server::http::*;

pub fn read_request(
    socket: &mut crate::transport::Stream,
    maximum: usize,
    timeout: Duration,
) -> Result<Request, HttpError> {
    zcall::server::http::read_request_with_decoder(
        socket,
        maximum,
        timeout,
        RequestPolicy::default(),
        Some(&crate::compression::decode),
    )
}
