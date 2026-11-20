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

    pub(super) fn peer_hung_up(handle: SocketHandle) -> io::Result<bool> {
        let mut descriptor = PollFd {
            fd: handle,
            // Darwin implements poll through kqueue and registers no socket
            // filter for events=0. Request only EVFILT_WRITE via POLLOUT:
            // its EOF proves that the peer cannot receive a response.
            // EVFILT_READ's EOF also occurs on a valid write half-close.
            events: 0x04, // POLLOUT
            revents: 0,
        };
        // Ignore ordinary writability; only error/hangup flags cancel.
        // SAFETY: one initialized pollfd and a nonblocking synchronous call.
        let result = unsafe { poll(&mut descriptor, 1, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            };
        }
        // POSIX POLLERR | POLLHUP | POLLNVAL on the supported Unix targets.
        Ok(descriptor.revents & (0x08 | 0x10 | 0x20) != 0)
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

    pub(super) fn tcp_reset(socket: &std::net::TcpStream) -> bool {
        use std::os::windows::io::AsRawSocket;

        let mut descriptor = PollFd {
            fd: socket.as_raw_socket(),
            events: 0x0100, // POLLRDNORM, not the Unix POLLIN value.
            revents: 0,
        };
        // SAFETY: std initialized Winsock; this is one live WSAPOLLFD and a
        // zero-timeout call. No socket mode, timeout or input bytes are changed.
        let ready = unsafe { WSAPoll(&mut descriptor, 1, 0) };
        if ready <= 0 || descriptor.revents & (0x0100 | 0x0001 | 0x0002) == 0 {
            return false;
        }
        // Only normal read readiness or an error/hangup can reach this peek.
        // The owning request worker has finished reading the request and no
        // other reader owns this client socket, so readiness cannot be consumed
        // between poll and peek. FIN (0 bytes) is not cancellation: a client may
        // still be waiting for our response after shutting down its write half.
        matches!(
            socket.peek(&mut [0u8; 1]),
            Err(error) if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::BrokenPipe
            )
        )
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

#[cfg(unix)]
pub fn peer_hung_up(handle: SocketHandle) -> io::Result<bool> {
    platform::peer_hung_up(handle)
}

#[cfg(windows)]
pub fn tcp_reset(socket: &std::net::TcpStream) -> bool {
    platform::tcp_reset(socket)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::Shutdown,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    #[test]
    fn unix_hangup_distinguishes_write_shutdown_from_full_close() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        assert!(!peer_hung_up(server.as_raw_fd()).unwrap());
        client.shutdown(Shutdown::Write).unwrap();
        assert!(!peer_hung_up(server.as_raw_fd()).unwrap());
        server.write_all(b"response").unwrap();
        let mut response = [0; 8];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"response");
        drop(client);
        assert!(peer_hung_up(server.as_raw_fd()).unwrap());
    }

    #[test]
    fn unix_hangup_probe_keeps_pending_request_bytes() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        for _ in 0..3 {
            assert!(!peer_hung_up(server.as_raw_fd()).unwrap());
        }
        let mut request = [0; 7];
        server.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"request");
        drop(client);
        assert!(peer_hung_up(server.as_raw_fd()).unwrap());
    }
}
