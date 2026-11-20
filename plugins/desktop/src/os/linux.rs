//! Local D-Bus only. The optional libsystemd handle is owned by this worker.
use super::{Backend, Capability, Level};
use crate::logic::Notice;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::fd::{BorrowedFd, OwnedFd},
    ptr,
};
use zsys::dylib::Library;

type BusNew = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type Unref = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type Address = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type Flag = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type Timeout = unsafe extern "C" fn(*mut c_void, u64) -> c_int;
type Start = unsafe extern "C" fn(*mut c_void) -> c_int;
type Call = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
    *mut *mut c_void,
    *const c_char,
    ...
) -> c_int;
type Read = unsafe extern "C" fn(*mut c_void, *const c_char, ...) -> c_int;
struct Api {
    new: BusNew,
    unref: Unref,
    message_unref: Unref,
    address: Address,
    client: Flag,
    timeout: Timeout,
    start: Start,
    call: Call,
    read: Read,
    _library: Library,
}
macro_rules! symbol {
    ($lib:expr,$name:literal,$ty:ty) => {{
        let pointer = $lib
            .symbol(CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap())
            .map_err(|_| Capability::Unavailable)?;
        // SAFETY: the symbol names and exact signatures are libsystemd's published C ABI.
        unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
    }};
}
impl Api {
    fn load() -> Result<Self, Capability> {
        // SAFETY: the library is retained until all bus/message objects are gone.
        let library =
            unsafe { Library::open(c"libsystemd.so.0") }.map_err(|_| Capability::Unavailable)?;
        Ok(Self {
            new: symbol!(library, "sd_bus_new", BusNew),
            unref: symbol!(library, "sd_bus_unref", Unref),
            message_unref: symbol!(library, "sd_bus_message_unref", Unref),
            address: symbol!(library, "sd_bus_set_address", Address),
            client: symbol!(library, "sd_bus_set_bus_client", Flag),
            timeout: symbol!(library, "sd_bus_set_method_call_timeout", Timeout),
            start: symbol!(library, "sd_bus_start", Start),
            call: symbol!(library, "sd_bus_call_method", Call),
            read: symbol!(library, "sd_bus_message_read", Read),
            _library: library,
        })
    }
    fn open(&self, address: &str) -> Result<Bus, Capability> {
        // Never accept a tcp/nonce-tcp or multi-address fallback from the environment.
        if !(address.starts_with("unix:path=") || address.starts_with("unix:abstract="))
            || address.contains(';')
            || address.len() > 4096
        {
            return Err(Capability::Unavailable);
        }
        let address = CString::new(address).map_err(|_| Capability::Unavailable)?;
        let mut pointer = ptr::null_mut();
        // SAFETY: outputs are live and sd_bus owns its copied address.
        unsafe {
            if (self.new)(&mut pointer) < 0 || pointer.is_null() {
                return Err(Capability::Unavailable);
            }
            let bus = Bus {
                pointer,
                unref: self.unref,
            };
            for rc in [
                (self.address)(pointer, address.as_ptr()),
                (self.client)(pointer, 1),
                (self.timeout)(pointer, 500_000),
            ] {
                if rc < 0 {
                    return Err(classify(rc));
                }
            }
            let rc = (self.start)(pointer);
            if rc < 0 {
                return Err(classify(rc));
            }
            Ok(bus)
        }
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        unsafe {
            let _ = self._library.close();
        }
    }
}
struct Bus {
    pointer: *mut c_void,
    unref: Unref,
}
impl Drop for Bus {
    fn drop(&mut self) {
        unsafe {
            (self.unref)(self.pointer);
        }
    }
}
struct Message {
    pointer: *mut c_void,
    unref: Unref,
}
impl Drop for Message {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            unsafe {
                (self.unref)(self.pointer);
            }
        }
    }
}
fn classify(code: c_int) -> Capability {
    if code == -13 || code == -1 {
        Capability::Denied
    } else {
        Capability::Unavailable
    }
}
pub struct Linux {
    inhibitor: Option<OwnedFd>,
    system: Option<Bus>,
    session: Option<Bus>,
    api: Option<Api>,
}
impl Linux {
    pub fn new() -> Self {
        Self {
            inhibitor: None,
            system: None,
            session: None,
            api: None,
        }
    }
    fn prepare(&mut self) -> Result<(), Capability> {
        if self.api.is_none() {
            self.api = Some(Api::load()?);
        }
        Ok(())
    }
    fn hold(&mut self) -> Result<(), Capability> {
        if self.inhibitor.is_some() {
            return Ok(());
        }
        self.prepare()?;
        let api = self.api.as_ref().unwrap();
        if self.system.is_none() {
            let address = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
                .unwrap_or_else(|_| "unix:path=/run/dbus/system_bus_socket".into());
            self.system = Some(api.open(&address)?);
        }
        let mut reply = ptr::null_mut();
        // SAFETY: the fixed D-Bus signature is four strings and returns a borrowed fd.
        let rc = unsafe {
            (api.call)(
                self.system.as_ref().unwrap().pointer,
                c"org.freedesktop.login1".as_ptr(),
                c"/org/freedesktop/login1".as_ptr(),
                c"org.freedesktop.login1.Manager".as_ptr(),
                c"Inhibit".as_ptr(),
                ptr::null_mut(),
                &mut reply,
                c"ssss".as_ptr(),
                c"idle:sleep".as_ptr(),
                c"zcalld".as_ptr(),
                c"An agent request is active or recently completed".as_ptr(),
                c"block".as_ptr(),
            )
        };
        let reply = Message {
            pointer: reply,
            unref: api.message_unref,
        };
        if rc < 0 {
            return Err(classify(rc));
        }
        let mut fd: c_int = -1;
        if unsafe { (api.read)(reply.pointer, c"h".as_ptr(), &mut fd) } < 0 || fd < 0 {
            return Err(Capability::Failed);
        }
        // The reply owns fd. Duplicate with CLOEXEC before releasing the reply.
        self.inhibitor = Some(
            unsafe { BorrowedFd::borrow_raw(fd) }
                .try_clone_to_owned()
                .map_err(|_| Capability::Failed)?,
        );
        Ok(())
    }
    fn send(&mut self, notice: &Notice) -> Result<(), Capability> {
        self.prepare()?;
        let api = self.api.as_ref().unwrap();
        if self.session.is_none() {
            let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
                .ok()
                .or_else(|| {
                    std::env::var("XDG_RUNTIME_DIR")
                        .ok()
                        .map(|p| format!("unix:path={p}/bus"))
                })
                .ok_or(Capability::Unavailable)?;
            self.session = Some(api.open(&address)?);
        }
        let title = CString::new(escape(&notice.title)).map_err(|_| Capability::Failed)?;
        let body = CString::new(escape(&notice.body)).map_err(|_| Capability::Failed)?;
        let mut reply = ptr::null_mut();
        // No actions, markup, icon path, credentials, shell or browser activation.
        let rc = unsafe {
            (api.call)(
                self.session.as_ref().unwrap().pointer,
                c"org.freedesktop.Notifications".as_ptr(),
                c"/org/freedesktop/Notifications".as_ptr(),
                c"org.freedesktop.Notifications".as_ptr(),
                c"Notify".as_ptr(),
                ptr::null_mut(),
                &mut reply,
                c"susssasa{sv}i".as_ptr(),
                c"zcalld".as_ptr(),
                0u32,
                c"".as_ptr(),
                title.as_ptr(),
                body.as_ptr(),
                0u32,
                0u32,
                10_000i32,
            )
        };
        let _reply = Message {
            pointer: reply,
            unref: api.message_unref,
        };
        if rc < 0 {
            self.session = None;
            return Err(classify(rc));
        }
        Ok(())
    }
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
impl Backend for Linux {
    fn inhibit(&mut self, level: Level) -> Capability {
        if level == Level::Off {
            self.inhibitor = None;
            return Capability::Available;
        }
        self.hold().map_or_else(|e| e, |()| Capability::Available)
    }
    fn notify(&mut self, notice: &Notice) -> Capability {
        self.send(notice)
            .map_or_else(|e| e, |()| Capability::Available)
    }
}
