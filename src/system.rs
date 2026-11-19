//! Minimal Linux process integration; no background service manager.
use std::{
    io,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixDatagram,
    },
    sync::atomic::{AtomicBool, Ordering},
};

pub static STOPPING: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
}
extern "C" fn terminate(_: i32) {
    STOPPING.store(true, Ordering::Relaxed);
}

pub fn setup_signals() -> io::Result<()> {
    // SAFETY: handlers perform only an atomic store and have C signal ABI.
    unsafe {
        if signal(2, terminate) == usize::MAX || signal(15, terminate) == usize::MAX {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn readable(fd: RawFd) -> io::Result<bool> {
    let mut descriptor = PollFd {
        fd,
        events: 1,
        revents: 0,
    };
    // A bounded poll also observes process shutdown if a signal reached a worker.
    // SAFETY: descriptor is live for this synchronous call, nfds is one.
    let result = unsafe { poll(&mut descriptor, 1, 500) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(result > 0)
}

pub fn notify(message: &str) -> io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    use std::os::unix::ffi::OsStrExt;
    let path = path.as_bytes();
    if let Some(name) = path.strip_prefix(b"@") {
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(message.as_bytes(), &address)?;
    } else {
        socket.send_to(
            message.as_bytes(),
            std::path::Path::new(std::ffi::OsStr::from_bytes(path)),
        )?;
    }
    Ok(())
}

pub fn wait_listener(listener: &std::net::TcpListener) -> io::Result<bool> {
    readable(listener.as_raw_fd())
}
