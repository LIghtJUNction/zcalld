//! Small binding to the system libcurl. Each worker owns one connection cache.
use crate::system::STOPPING;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_long, c_void},
    ptr::NonNull,
    sync::{OnceLock, atomic::Ordering},
};

#[repr(C)]
struct Slist {
    data: *mut c_char,
    next: *mut Slist,
}
#[cfg_attr(all(windows, target_env = "msvc"), link(name = "libcurl"))]
#[cfg_attr(not(all(windows, target_env = "msvc")), link(name = "curl"))]
unsafe extern "C" {
    fn curl_global_init(flags: c_long) -> c_int;
    fn curl_easy_init() -> *mut c_void;
    fn curl_easy_cleanup(handle: *mut c_void);
    fn curl_easy_reset(handle: *mut c_void);
    fn curl_easy_setopt(handle: *mut c_void, option: c_int, ...) -> c_int;
    fn curl_easy_perform(handle: *mut c_void) -> c_int;
    fn curl_easy_strerror(code: c_int) -> *const c_char;
    fn curl_slist_append(list: *mut Slist, text: *const c_char) -> *mut Slist;
    fn curl_slist_free_all(list: *mut Slist);
    fn curl_version() -> *const c_char;
}

pub trait Sink {
    fn header(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn body(&mut self, bytes: &[u8]) -> Result<(), String>;
}

pub struct Options<'a> {
    pub url: &'a str,
    pub body: &'a [u8],
    pub headers: &'a [String],
    pub timeout_ms: u64,
    pub connect_timeout_ms: u64,
}

pub fn init() -> Result<(), String> {
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| {
            // SAFETY: exactly once before worker creation; libcurl global state lives
            // until process exit, after all handles have been dropped.
            let result = unsafe { curl_global_init(3) };
            if result == 0 {
                Ok(())
            } else {
                Err(code_message(result))
            }
        })
        .clone()
}

pub fn version() -> String {
    // SAFETY: libcurl returns a process-lifetime NUL-terminated static string.
    unsafe {
        CStr::from_ptr(curl_version())
            .to_string_lossy()
            .into_owned()
    }
}

pub struct Client(NonNull<c_void>);
impl Client {
    pub fn new() -> Result<Self, String> {
        init()?;
        // SAFETY: library initialization succeeded; each handle has one owner.
        NonNull::new(unsafe { curl_easy_init() })
            .map(Self)
            .ok_or_else(|| "libcurl handle allocation failed".into())
    }

    pub fn perform(&mut self, options: Options<'_>, sink: &mut dyn Sink) -> Result<(), String> {
        let url = CString::new(options.url).map_err(|_| "invalid upstream URL")?;
        let mut headers = HeaderList(std::ptr::null_mut());
        for value in options.headers {
            headers.push(value)?;
        }
        let mut transfer = Transfer {
            sink,
            body: options.body,
            uploaded: 0,
            error: None,
        };
        let context = &mut transfer as *mut Transfer<'_> as *mut c_void;
        let handle = self.0.as_ptr();
        // SAFETY: reset preserves the easy handle's connection cache and removes
        // the previous request's credentials and callback pointers.
        unsafe {
            curl_easy_reset(handle);
        }
        let result = (|| -> Result<(), String> {
            macro_rules! option {
                ($name:expr, $value:expr) => {{
                    // SAFETY: option IDs/types match system curl.h. Pointers
                    // remain valid until synchronous curl_easy_perform returns.
                    let code = unsafe { curl_easy_setopt(handle, $name, $value) };
                    if code != 0 {
                        return Err(code_message(code));
                    }
                }};
            }
            option!(10002, url.as_ptr()); // CURLOPT_URL
            // A read/seek pair refuses rewinds after upload starts. POSTFIELDS
            // would let libcurl silently replay POST on a stale pooled socket.
            option!(10015, std::ptr::null::<c_char>()); // CURLOPT_POSTFIELDS
            option!(47, 1 as c_long); // CURLOPT_POST
            option!(60, options.body.len() as c_long); // CURLOPT_POSTFIELDSIZE
            option!(
                20012,
                read_callback as extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize
            );
            option!(10009, context); // CURLOPT_READDATA
            option!(
                20167,
                seek_callback as extern "C" fn(*mut c_void, i64, c_int) -> c_int
            );
            option!(10168, context); // CURLOPT_SEEKDATA
            option!(10023, headers.0); // CURLOPT_HTTPHEADER
            option!(
                20011,
                write_callback as extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize
            );
            option!(10001, context); // CURLOPT_WRITEDATA
            option!(
                20079,
                header_callback as extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize
            );
            option!(10029, context); // CURLOPT_HEADERDATA
            option!(155, options.timeout_ms as c_long);
            option!(156, options.connect_timeout_ms as c_long);
            option!(99, 1 as c_long); // CURLOPT_NOSIGNAL
            option!(52, 0 as c_long); // No redirect can forward credentials elsewhere.
            option!(10102, c"".as_ptr()); // Decode supported response encodings.
            Ok(())
        })();
        let result = result
            .and_then(|()| self.finish_setup(handle, context))
            .and_then(|code| {
                if let Some(error) = transfer.error.take() {
                    return Err(error);
                }
                if STOPPING.load(Ordering::Relaxed) {
                    return Err("gateway is stopping".into());
                }
                if code == 0 {
                    Ok(())
                } else {
                    Err(code_message(code))
                }
            });
        // SAFETY: no transfer is active; do not retain stack or header pointers.
        unsafe {
            curl_easy_reset(handle);
        }
        result
    }

    fn finish_setup(&self, handle: *mut c_void, context: *mut c_void) -> Result<c_int, String> {
        // SAFETY: long values and callback prototypes match curl.h, and all
        // borrowed data is alive through the synchronous perform call.
        let codes = unsafe {
            [
                curl_easy_setopt(handle, 265, 1 as c_long), // SUPPRESS_CONNECT_HEADERS
                curl_easy_setopt(handle, 213, 1 as c_long), // TCP_KEEPALIVE
                curl_easy_setopt(handle, 10318, c"http,https".as_ptr()), // PROTOCOLS_STR
                curl_easy_setopt(handle, 10018, c"zcalld".as_ptr()),
                curl_easy_setopt(handle, 43, 0 as c_long), // NOPROGRESS
                curl_easy_setopt(handle, 10057, context),
                curl_easy_setopt(
                    handle,
                    20219,
                    progress_callback as extern "C" fn(*mut c_void, i64, i64, i64, i64) -> c_int,
                ),
            ]
        };
        for code in codes {
            if code != 0 {
                return Err(code_message(code));
            }
        }
        // SAFETY: handle is valid and callbacks cannot outlive this call.
        Ok(unsafe { curl_easy_perform(handle) })
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: sole owner, no concurrent or active transfer at drop.
        unsafe {
            curl_easy_cleanup(self.0.as_ptr());
        }
    }
}

struct HeaderList(*mut Slist);
impl HeaderList {
    fn push(&mut self, value: &str) -> Result<(), String> {
        if value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
            return Err("invalid upstream header".into());
        }
        let value = CString::new(value).map_err(|_| "invalid upstream header")?;
        // SAFETY: libcurl copies the string; self owns the linked list.
        let next = unsafe { curl_slist_append(self.0, value.as_ptr()) };
        if next.is_null() {
            return Err("upstream header allocation failed".into());
        }
        self.0 = next;
        Ok(())
    }
}
impl Drop for HeaderList {
    fn drop(&mut self) {
        // SAFETY: no easy handle retains this list after perform's reset.
        unsafe {
            curl_slist_free_all(self.0);
        }
    }
}

struct Transfer<'a> {
    sink: &'a mut dyn Sink,
    body: &'a [u8],
    uploaded: usize,
    error: Option<String>,
}
extern "C" fn read_callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
) -> usize {
    const ABORT: usize = 0x10000000;
    let Some(capacity) = size.checked_mul(count) else {
        return ABORT;
    };
    if capacity == 0 {
        return 0;
    }
    if data.is_null() || context.is_null() || STOPPING.load(Ordering::Relaxed) {
        return ABORT;
    }
    // SAFETY: curl supplies writable storage and our live exclusive transfer
    // context; callbacks are synchronous and never retained after perform.
    let transfer = unsafe { &mut *(context as *mut Transfer<'_>) };
    let count = capacity.min(transfer.body.len() - transfer.uploaded);
    unsafe {
        std::ptr::copy_nonoverlapping(
            transfer.body.as_ptr().add(transfer.uploaded),
            data.cast::<u8>(),
            count,
        );
    }
    transfer.uploaded += count;
    count
}
extern "C" fn seek_callback(context: *mut c_void, offset: i64, origin: c_int) -> c_int {
    if context.is_null() {
        return 1;
    }
    // SAFETY: the callback context is live for this synchronous request.
    let transfer = unsafe { &mut *(context as *mut Transfer<'_>) };
    if transfer.uploaded == 0 && offset == 0 && origin == 0 {
        return 0;
    }
    transfer.error = Some("automatic replay refused after generation upload began".into());
    1 // CURL_SEEKFUNC_FAIL aborts; CANTSEEK would permit alternative replay.
}
fn callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
    header: bool,
) -> usize {
    let Some(length) = size.checked_mul(count) else {
        return 0;
    };
    if length == 0 {
        return 0;
    }
    if data.is_null() || context.is_null() {
        return 0;
    }
    // SAFETY: libcurl supplies an initialized slice and the exclusive live
    // Transfer pointer registered immediately before synchronous perform.
    let transfer = unsafe { &mut *(context as *mut Transfer<'_>) };
    let bytes = unsafe { std::slice::from_raw_parts(data.cast_const().cast::<u8>(), length) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if header {
            transfer.sink.header(bytes)
        } else {
            transfer.sink.body(bytes)
        }
    }));
    match result {
        Ok(Ok(())) => length,
        Ok(Err(error)) => {
            transfer.error = Some(error);
            0
        }
        Err(_) => {
            transfer.error = Some("upstream callback failed".into());
            0
        }
    }
}
extern "C" fn write_callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
) -> usize {
    callback(data, size, count, context, false)
}
extern "C" fn header_callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
) -> usize {
    callback(data, size, count, context, true)
}
extern "C" fn progress_callback(_: *mut c_void, _: i64, _: i64, _: i64, _: i64) -> c_int {
    c_int::from(STOPPING.load(Ordering::Relaxed))
}
fn code_message(code: c_int) -> String {
    // SAFETY: curl_easy_strerror returns a static, NUL-terminated string.
    let message = unsafe { CStr::from_ptr(curl_easy_strerror(code)).to_string_lossy() };
    format!("upstream transport: {message} (curl {code})")
}
