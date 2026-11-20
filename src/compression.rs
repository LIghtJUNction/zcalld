//! Bounded native decoding for incoming compressed HTTP requests.
//! Runs before JSON parsing, authentication dispatch, privacy and provider routing.
use crate::{http::HttpError, system::STOPPING};
use std::{
    ffi::{c_char, c_int, c_uint, c_ulong, c_void},
    ptr,
    sync::atomic::Ordering,
    time::Instant,
};

const BLOCK: usize = 16 * 1024;
const ZSTD_WINDOW_LOG_MAX: c_int = 23; // At most 8 MiB of history per decoder.

fn error(status: u16, message: &'static str) -> HttpError {
    HttpError { status, message }
}
fn budget(deadline: Instant) -> Result<(), HttpError> {
    if STOPPING.load(Ordering::Relaxed) {
        Err(error(503, "gateway is stopping"))
    } else if Instant::now() >= deadline {
        Err(error(408, "request timeout"))
    } else {
        Ok(())
    }
}
fn append(output: &mut Vec<u8>, bytes: &[u8], maximum: usize) -> Result<(), HttpError> {
    if bytes.len() > maximum.saturating_sub(output.len()) {
        return Err(error(413, "decoded request exceeds max_body_bytes"));
    }
    let needed = output.len() + bytes.len();
    if needed > output.capacity() {
        let capacity = output.capacity().saturating_mul(2).max(needed).min(maximum);
        output
            .try_reserve_exact(capacity - output.len())
            .map_err(|_| error(503, "decoded body allocation failed"))?;
    }
    output.extend_from_slice(bytes);
    Ok(())
}

pub fn decode(
    encoding: &str,
    body: &[u8],
    maximum: usize,
    deadline: Instant,
) -> Result<Vec<u8>, HttpError> {
    budget(deadline)?;
    if body.len() > maximum {
        return Err(error(413, "encoded request exceeds max_body_bytes"));
    }
    match encoding {
        "zstd" => zstd(body, maximum, deadline),
        "gzip" => gzip(body, maximum, deadline),
        _ => Err(error(415, "unsupported request content encoding")),
    }
}

#[repr(C)]
struct ZstdInput {
    src: *const c_void,
    size: usize,
    pos: usize,
}
#[repr(C)]
struct ZstdOutput {
    dst: *mut c_void,
    size: usize,
    pos: usize,
}
#[cfg_attr(target_os = "macos", link(name = "zstd", kind = "static"))]
#[cfg_attr(windows, link(name = "zstd", kind = "static"))]
#[cfg_attr(not(any(target_os = "macos", windows)), link(name = "zstd"))]
unsafe extern "C" {
    fn ZSTD_createDCtx() -> *mut c_void;
    fn ZSTD_freeDCtx(context: *mut c_void) -> usize;
    fn ZSTD_DCtx_setParameter(context: *mut c_void, parameter: c_int, value: c_int) -> usize;
    fn ZSTD_decompressStream(
        context: *mut c_void,
        output: *mut ZstdOutput,
        input: *mut ZstdInput,
    ) -> usize;
    fn ZSTD_isError(result: usize) -> c_uint;
    fn ZSTD_getErrorCode(result: usize) -> c_int;
}
struct ZstdContext(*mut c_void);
impl Drop for ZstdContext {
    fn drop(&mut self) {
        // SAFETY: this non-null context is owned here and freed once.
        unsafe { ZSTD_freeDCtx(self.0) };
    }
}
fn zstd(body: &[u8], maximum: usize, deadline: Instant) -> Result<Vec<u8>, HttpError> {
    // SAFETY: allocation has no Rust pointer arguments.
    let raw = unsafe { ZSTD_createDCtx() };
    if raw.is_null() {
        return Err(error(503, "cannot allocate zstd decoder"));
    }
    let context = ZstdContext(raw);
    // SAFETY: parameter 100 is the stable ZSTD_d_windowLogMax enum value.
    let configured = unsafe { ZSTD_DCtx_setParameter(context.0, 100, ZSTD_WINDOW_LOG_MAX) };
    // SAFETY: the result is an opaque zstd status value.
    if unsafe { ZSTD_isError(configured) } != 0 {
        return Err(error(503, "cannot bound the zstd decoder window"));
    }
    let mut input = ZstdInput {
        src: body.as_ptr().cast(),
        size: body.len(),
        pos: 0,
    };
    let mut decoded = Vec::new();
    let mut bytes = [0u8; BLOCK];
    loop {
        budget(deadline)?;
        let before = input.pos;
        let capacity = BLOCK.min(maximum.saturating_sub(decoded.len()).saturating_add(1));
        let mut output = ZstdOutput {
            dst: bytes.as_mut_ptr().cast(),
            size: capacity,
            pos: 0,
        };
        // SAFETY: both buffers remain live for this call; sizes bound their
        // initialized allocations. The exclusive context is never shared.
        let result = unsafe { ZSTD_decompressStream(context.0, &mut output, &mut input) };
        // SAFETY: zstd status helpers inspect only the returned integer.
        if unsafe { ZSTD_isError(result) } != 0 {
            return Err(match unsafe { ZSTD_getErrorCode(result) } {
                16 => error(413, "zstd request window exceeds 8 MiB"),
                64 => error(503, "zstd decoder allocation failed"),
                _ => error(400, "invalid or incomplete zstd request body"),
            });
        }
        append(&mut decoded, &bytes[..output.pos], maximum)?;
        if result == 0 && input.pos == input.size {
            budget(deadline)?;
            return Ok(decoded);
        }
        if input.pos == before && output.pos == 0 {
            return Err(error(400, "invalid or incomplete zstd request body"));
        }
    }
}

type Allocate = Option<unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void>;
type Free = Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>;
#[repr(C)]
struct ZStream {
    next_in: *mut u8,
    avail_in: c_uint,
    total_in: c_ulong,
    next_out: *mut u8,
    avail_out: c_uint,
    total_out: c_ulong,
    msg: *mut c_char,
    state: *mut c_void,
    zalloc: Allocate,
    zfree: Free,
    opaque: *mut c_void,
    data_type: c_int,
    adler: c_ulong,
    reserved: c_ulong,
}
#[cfg_attr(windows, link(name = "zlib", kind = "static"))]
#[cfg_attr(not(windows), link(name = "z"))]
unsafe extern "C" {
    fn zlibVersion() -> *const c_char;
    fn inflateInit2_(
        stream: *mut ZStream,
        window_bits: c_int,
        version: *const c_char,
        stream_size: c_int,
    ) -> c_int;
    fn inflate(stream: *mut ZStream, flush: c_int) -> c_int;
    fn inflateReset2(stream: *mut ZStream, window_bits: c_int) -> c_int;
    fn inflateEnd(stream: *mut ZStream) -> c_int;
}
struct GzipContext(Box<ZStream>);
impl Drop for GzipContext {
    fn drop(&mut self) {
        // SAFETY: inflateInit2_ succeeded on this same stable boxed address.
        unsafe { inflateEnd(&mut *self.0) };
    }
}
fn gzip(body: &[u8], maximum: usize, deadline: Instant) -> Result<Vec<u8>, HttpError> {
    let length = c_uint::try_from(body.len())
        .map_err(|_| error(413, "encoded request exceeds native decoder bounds"))?;
    // zlib retains a pointer to z_stream; the box keeps that address stable.
    let mut stream = Box::new(ZStream {
        next_in: body.as_ptr().cast_mut(),
        avail_in: length,
        total_in: 0,
        next_out: ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: ptr::null_mut(),
        state: ptr::null_mut(),
        zalloc: None,
        zfree: None,
        opaque: ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    });
    // SAFETY: the C-layout stream is initialized, input is live, and the zlib
    // version/structure-size handshake checks the ABI before any decoding.
    let initialized = unsafe {
        inflateInit2_(
            &mut *stream,
            31, // gzip only, with the fixed 32 KiB deflate window.
            zlibVersion(),
            std::mem::size_of::<ZStream>() as c_int,
        )
    };
    if initialized != 0 {
        return Err(error(503, "cannot initialize gzip decoder"));
    }
    let mut context = GzipContext(stream);
    let mut decoded = Vec::new();
    let mut bytes = [0u8; BLOCK];
    loop {
        budget(deadline)?;
        let capacity = BLOCK.min(maximum.saturating_sub(decoded.len()).saturating_add(1));
        let before = context.0.avail_in;
        context.0.next_out = bytes.as_mut_ptr();
        context.0.avail_out = capacity as c_uint;
        // SAFETY: the stable stream references live input and bounded output.
        let result = unsafe { inflate(&mut *context.0, 0) };
        let written = capacity - context.0.avail_out as usize;
        append(&mut decoded, &bytes[..written], maximum)?;
        if result == 1 {
            if context.0.avail_in == 0 {
                budget(deadline)?;
                return Ok(decoded);
            }
            // A gzip body may contain multiple members. The next member must
            // independently pass its header, checksum and length validation.
            let next = context.0.next_in;
            let remaining = context.0.avail_in;
            // SAFETY: reset retains the same initialized, exclusively owned stream.
            if unsafe { inflateReset2(&mut *context.0, 31) } != 0 {
                return Err(error(400, "invalid gzip request body"));
            }
            context.0.next_in = next;
            context.0.avail_in = remaining;
        } else if result != 0 || (context.0.avail_in == before && written == 0) {
            return Err(if result == -4 {
                error(503, "gzip decoder allocation failed")
            } else {
                error(400, "invalid or incomplete gzip request body")
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    const BODY: &[u8] = &[
        123, 34, 109, 111, 100, 101, 108, 34, 58, 34, 116, 101, 115, 116, 34, 44, 34, 109, 101,
        115, 115, 97, 103, 101, 115, 34, 58, 91, 123, 34, 114, 111, 108, 101, 34, 58, 34, 117, 115,
        101, 114, 34, 44, 34, 99, 111, 110, 116, 101, 110, 116, 34, 58, 34, 99, 111, 109, 112, 114,
        101, 115, 115, 101, 100, 32, 102, 105, 120, 116, 117, 114, 101, 34, 125, 93, 44, 34, 115,
        116, 114, 101, 97, 109, 34, 58, 102, 97, 108, 115, 101, 125,
    ];
    const ZSTD: &[u8] = &[
        40, 181, 47, 253, 32, 91, 93, 2, 0, 178, 197, 17, 22, 160, 167, 57, 216, 16, 77, 10, 253,
        178, 129, 233, 223, 76, 206, 86, 5, 105, 194, 36, 1, 60, 14, 135, 159, 10, 170, 90, 49,
        230, 75, 2, 71, 141, 69, 22, 6, 104, 56, 254, 141, 4, 59, 211, 106, 18, 78, 226, 76, 73,
        209, 47, 90, 117, 58, 172, 1, 168, 222, 130, 242, 219, 74, 154, 119, 182, 154, 124, 206,
        106, 4, 0,
    ];
    const GZIP: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 13, 202, 75, 10, 192, 32, 16, 4, 209, 171, 132, 94, 123,
        2, 175, 18, 178, 16, 109, 67, 192, 79, 152, 25, 33, 32, 222, 61, 110, 235, 213, 68, 237,
        137, 5, 30, 70, 53, 56, 84, 170, 134, 155, 10, 127, 78, 72, 47, 220, 52, 148, 178, 41, 246,
        102, 108, 182, 67, 236, 245, 149, 61, 50, 29, 249, 249, 108, 8, 177, 46, 7, 53, 97, 168,
        240, 57, 20, 229, 250, 1, 60, 160, 195, 53, 91, 0, 0, 0,
    ];
    const LARGE_ZSTD: &[u8] = &[
        40, 181, 47, 253, 96, 0, 255, 85, 0, 0, 16, 120, 120, 1, 0, 251, 127, 29, 96, 1,
    ];
    const LARGE_GZIP: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 237, 193, 1, 1, 0, 0, 0, 128, 144, 219, 205, 239, 8, 10,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 106, 154, 88, 180, 44, 0, 0, 1, 0,
    ];
    fn run(encoding: &str, bytes: &[u8], maximum: usize) -> Result<Vec<u8>, HttpError> {
        decode(
            encoding,
            bytes,
            maximum,
            Instant::now() + Duration::from_secs(2),
        )
    }
    fn status(result: Result<Vec<u8>, HttpError>) -> u16 {
        match result {
            Ok(_) => panic!("encoded fixture should be rejected"),
            Err(e) => e.status,
        }
    }
    #[test]
    fn native_gzip_and_zstd_decode_exact_json() {
        for (encoding, bytes) in [("gzip", GZIP), ("zstd", ZSTD)] {
            assert_eq!(
                run(encoding, bytes, 4096).unwrap_or_else(|e| panic!("{}", e.message)),
                BODY
            );
        }
    }
    #[test]
    fn decoded_limits_stop_compression_bombs() {
        assert_eq!(status(run("zstd", LARGE_ZSTD, 256)), 413);
        assert_eq!(status(run("gzip", LARGE_GZIP, 256)), 413);
    }
    #[test]
    fn incomplete_and_trailing_frames_are_rejected() {
        for (encoding, bytes) in [("gzip", GZIP), ("zstd", ZSTD)] {
            assert_eq!(status(run(encoding, &bytes[..bytes.len() - 1], 4096)), 400);
            let mut trailing = bytes.to_vec();
            trailing.extend_from_slice(b"junk");
            assert_eq!(status(run(encoding, &trailing, 4096)), 400);
        }
    }
    #[test]
    fn concatenated_members_keep_the_same_total_limit() {
        for (encoding, bytes) in [("gzip", GZIP), ("zstd", ZSTD)] {
            let mut members = bytes.to_vec();
            members.extend_from_slice(bytes);
            let mut expected = BODY.to_vec();
            expected.extend_from_slice(BODY);
            assert_eq!(
                run(encoding, &members, 4096).unwrap_or_else(|e| panic!("{}", e.message)),
                expected
            );
        }
    }
    #[test]
    fn oversized_zstd_window_is_refused_before_allocation() {
        // A valid empty frame advertises a 256 MiB history window.
        assert_eq!(
            status(run(
                "zstd",
                &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x90, 0x01, 0x00, 0x00],
                4096
            )),
            413
        );
    }
    #[test]
    fn expired_deadlines_do_not_restart_for_decoding() {
        assert_eq!(
            status(decode(
                "zstd",
                ZSTD,
                4096,
                Instant::now() - Duration::from_millis(1)
            )),
            408
        );
    }
}
