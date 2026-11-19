//! Native process integration: Unix signals, Windows console control, Linux systemd notify.
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

pub static STOPPING: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
pub type SocketHandle = std::os::fd::RawFd;
#[cfg(windows)]
pub type SocketHandle = std::os::windows::io::RawSocket;

#[cfg(unix)]
mod platform {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PollFd {
        fd: SocketHandle,
        events: i16,
        revents: i16,
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type Nfds = usize;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type Nfds = u32;

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: Nfds, timeout: i32) -> i32;
        fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
    }
    extern "C" fn terminate(_: i32) {
        STOPPING.store(true, Ordering::Relaxed);
    }

    pub(super) fn setup_signals() -> io::Result<()> {
        // SAFETY: handlers perform only an atomic store and have C signal ABI.
        unsafe {
            if signal(2, terminate) == usize::MAX || signal(15, terminate) == usize::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(super) fn wait_readable(handles: &[SocketHandle]) -> io::Result<u32> {
        let mut descriptors = [PollFd {
            fd: -1,
            events: 1,
            revents: 0,
        }; 2];
        for (descriptor, handle) in descriptors.iter_mut().zip(handles) {
            descriptor.fd = *handle;
        }
        // A bounded wait observes shutdown even if the signal reaches a worker.
        // SAFETY: at most two initialized pollfd entries, valid for the synchronous call.
        let result = unsafe { poll(descriptors.as_mut_ptr(), handles.len() as Nfds, 500) };
        if result < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(0)
            } else {
                Err(error)
            };
        }
        Ok(descriptors[..handles.len()]
            .iter()
            .enumerate()
            .fold(0, |mask, (index, descriptor)| {
                mask | (u32::from(descriptor.revents != 0) << index)
            }))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PollFd {
        fd: SocketHandle,
        events: i16,
        revents: i16,
    }
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAPoll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    extern "system" fn terminate(event: u32) -> i32 {
        if matches!(event, 0 | 1 | 2 | 5 | 6) {
            STOPPING.store(true, Ordering::Relaxed);
            1
        } else {
            0
        }
    }

    pub(super) fn setup_signals() -> io::Result<()> {
        // SAFETY: callback has Win32 ABI and remains valid until process exit.
        if unsafe { SetConsoleCtrlHandler(Some(terminate), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn wait_readable(handles: &[SocketHandle]) -> io::Result<u32> {
        // Windows POLLIN = POLLRDNORM | POLLRDBAND.
        let mut descriptors = [PollFd {
            fd: !0,
            events: 0x0300,
            revents: 0,
        }; 2];
        for (descriptor, handle) in descriptors.iter_mut().zip(handles) {
            descriptor.fd = *handle;
        }
        // SAFETY: std's TCP listener initializes Winsock; the array layout matches WSAPOLLFD.
        let result = unsafe { WSAPoll(descriptors.as_mut_ptr(), handles.len() as u32, 500) };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(descriptors[..handles.len()]
            .iter()
            .enumerate()
            .fold(0, |mask, (index, descriptor)| {
                mask | (u32::from(descriptor.revents != 0) << index)
            }))
    }
}

pub fn setup_signals() -> io::Result<()> {
    STOPPING.store(false, Ordering::Relaxed);
    platform::setup_signals()
}

pub fn wait_readable(handles: &[SocketHandle]) -> io::Result<u32> {
    if handles.is_empty() || handles.len() > 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one or two listener handles",
        ));
    }
    platform::wait_readable(handles)
}

#[cfg(target_os = "linux")]
pub fn notify(message: &str) -> io::Result<()> {
    use std::os::{
        linux::net::SocketAddrExt,
        unix::{ffi::OsStrExt, net::UnixDatagram},
    };
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
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

#[cfg(not(target_os = "linux"))]
pub fn notify(_message: &str) -> io::Result<()> {
    Ok(())
}
