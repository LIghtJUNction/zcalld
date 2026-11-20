//! Local transport without an async runtime. One accept loop serves TCP and Unix sockets.
use crate::{config::Config, system};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

pub enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Stream {
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(socket) => socket.set_read_timeout(timeout),
            #[cfg(unix)]
            Self::Unix(socket) => socket.set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(socket) => socket.set_write_timeout(timeout),
            #[cfg(unix)]
            Self::Unix(socket) => socket.set_write_timeout(timeout),
        }
    }

    /// Detect confirmed disconnects without mistaking write half-close for cancellation.
    /// TCP FIN alone cannot distinguish a closed client from one awaiting a response.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Tcp(socket) => socket.take_error().is_ok_and(|error| error.is_some()),
            #[cfg(unix)]
            Self::Unix(socket) => {
                use std::os::fd::AsRawFd;
                system::peer_hung_up(socket.as_raw_fd()).unwrap_or(false)
            }
        }
    }

    pub fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(socket) => socket.shutdown(std::net::Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(socket) => socket.shutdown(std::net::Shutdown::Write),
        }
    }

    pub fn set_nodelay(&self, enabled: bool) -> io::Result<()> {
        match self {
            Self::Tcp(socket) => socket.set_nodelay(enabled),
            #[cfg(unix)]
            Self::Unix(_) => Ok(()),
        }
    }

    /// Kernel-derived connection metadata for optional routing. Caller headers
    /// cannot supply or replace it. Unsupported peer identity stays unknown.
    pub fn routing_peer(&self) -> String {
        match self {
            Self::Tcp(socket) => match (socket.local_addr(), socket.peer_addr()) {
                (Ok(local), Ok(remote)) => format!(
                    "{{\"kind\":\"tcp\",\"local\":{},\"remote\":{}}}",
                    crate::http::quote(&local.to_string()),
                    crate::http::quote(&remote.to_string())
                ),
                _ => "{}".into(),
            },
            #[cfg(target_os = "linux")]
            Self::Unix(socket) => linux_unix_peer(socket),
            #[cfg(all(unix, not(target_os = "linux")))]
            Self::Unix(_) => "{}".into(),
        }
    }

    pub fn requires_loopback_host(&self) -> bool {
        match self {
            Self::Tcp(socket) => socket.local_addr().is_ok_and(|a| a.ip().is_loopback()),
            // A private filesystem socket cannot be reached by DNS rebinding.
            #[cfg(unix)]
            Self::Unix(_) => false,
        }
    }

    pub fn is_loopback_peer(&self) -> bool {
        match self {
            Self::Tcp(socket) => socket.peer_addr().is_ok_and(|address| match address.ip() {
                IpAddr::V4(address) => address.is_loopback(),
                IpAddr::V6(address) => {
                    address.is_loopback()
                        || address
                            .to_ipv4_mapped()
                            .is_some_and(|address| address.is_loopback())
                }
            }),
            #[cfg(unix)]
            Self::Unix(_) => true,
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_unix_peer(socket: &std::os::unix::net::UnixStream) -> String {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Credentials {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    unsafe extern "C" {
        fn getsockopt(
            fd: i32,
            level: i32,
            option: i32,
            value: *mut std::ffi::c_void,
            length: *mut u32,
        ) -> i32;
    }
    let mut credentials = Credentials {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<Credentials>() as u32;
    // Linux SOL_SOCKET/SO_PEERCRED. The kernel fills the fixed-layout ucred.
    let result = unsafe {
        getsockopt(
            socket.as_raw_fd(),
            1,
            17,
            (&mut credentials as *mut Credentials).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<Credentials>() || credentials.pid <= 0
    {
        return "{}".into();
    }
    let stat =
        std::fs::read_to_string(format!("/proc/{}/stat", credentials.pid)).unwrap_or_default();
    let start = stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(19));
    match start.filter(|value| !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit())) {
        Some(start) => format!(
            "{{\"kind\":\"unix\",\"pid\":{},\"uid\":{},\"start\":{}}}",
            credentials.pid,
            credentials.uid,
            crate::http::quote(start)
        ),
        None => "{}".into(),
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&*self).read(bytes)
    }
}

impl Read for &Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(socket) => {
                let mut view = socket;
                view.read(bytes)
            }
            #[cfg(unix)]
            Stream::Unix(socket) => {
                let mut view = socket;
                view.read(bytes)
            }
        }
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&*self).write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Write for &Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(socket) => {
                let mut view = socket;
                view.write(bytes)
            }
            #[cfg(unix)]
            Stream::Unix(socket) => {
                let mut view = socket;
                view.write(bytes)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(socket) => {
                let mut view = socket;
                view.flush()
            }
            #[cfg(unix)]
            Stream::Unix(socket) => {
                let mut view = socket;
                view.flush()
            }
        }
    }
}

pub fn client_address(address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        SocketAddr::new(
            if address.is_ipv4() {
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            },
            address.port(),
        )
    } else {
        address
    }
}

/// Prefer the private local socket; use TCP when a configured socket is absent.
/// Permission/other socket errors remain visible instead of silently changing trust boundaries.
pub fn connect(config: &Config) -> io::Result<Stream> {
    #[cfg(unix)]
    if let Some(path) = &config.unix_socket {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(socket) => return Ok(Stream::Unix(socket)),
            Err(error)
                if config.listen.is_some()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) => {}
            Err(error) => return Err(error),
        }
    }
    let address = config.listen.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "no available local listener")
    })?;
    TcpStream::connect_timeout(&client_address(address), Duration::from_secs(3)).map(Stream::Tcp)
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(unix::Socket),
}

impl Listener {
    fn handle(&self) -> system::SocketHandle {
        #[cfg(unix)]
        use std::os::fd::AsRawFd;
        #[cfg(windows)]
        use std::os::windows::io::AsRawSocket;
        match self {
            #[cfg(unix)]
            Self::Tcp(socket) => socket.as_raw_fd(),
            #[cfg(windows)]
            Self::Tcp(socket) => socket.as_raw_socket(),
            #[cfg(unix)]
            Self::Unix(socket) => socket.listener.as_raw_fd(),
        }
    }

    fn accept(&self) -> io::Result<Stream> {
        match self {
            Self::Tcp(socket) => {
                let (stream, _) = socket.accept()?;
                stream.set_nonblocking(false)?;
                Ok(Stream::Tcp(stream))
            }
            #[cfg(unix)]
            Self::Unix(socket) => {
                let (stream, _) = socket.listener.accept()?;
                stream.set_nonblocking(false)?;
                Ok(Stream::Unix(stream))
            }
        }
    }
}

pub struct Listeners {
    listeners: Vec<Listener>,
    next: usize,
    description: String,
}

impl Listeners {
    pub fn bind(config: &Config) -> io::Result<Self> {
        let mut listeners = Vec::with_capacity(2);
        let mut descriptions = Vec::with_capacity(2);
        if let Some(address) = config.listen {
            let socket = TcpListener::bind(address)?;
            socket.set_nonblocking(true)?;
            descriptions.push(format!("http://{}", socket.local_addr()?));
            listeners.push(Listener::Tcp(socket));
        }
        if let Some(path) = &config.unix_socket {
            #[cfg(unix)]
            {
                let socket = unix::Socket::bind(path)?;
                descriptions.insert(0, format!("unix:{}", path.display()));
                listeners.push(Listener::Unix(socket));
            }
            #[cfg(not(unix))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Unix socket {} is unavailable on this platform",
                    path.display()
                ),
            ));
        }
        if listeners.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configure at least one listener",
            ));
        }
        Ok(Self {
            listeners,
            next: 0,
            description: descriptions.join(", "),
        })
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Bounded native wait, round-robin fairness, and no idle polling thread.
    pub fn accept(&mut self) -> io::Result<Option<Stream>> {
        let mut handles = [0 as system::SocketHandle; 2];
        for (index, listener) in self.listeners.iter().enumerate() {
            handles[index] = listener.handle();
        }
        let ready = system::wait_readable(&handles[..self.listeners.len()])?;
        for offset in 0..self.listeners.len() {
            let index = (self.next + offset) % self.listeners.len();
            if ready & (1 << index) == 0 {
                continue;
            }
            self.next = (index + 1) % self.listeners.len();
            match self.listeners[index].accept() {
                Ok(socket) => return Ok(Some(socket)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsString,
        fs::{self, File, OpenOptions},
        io::{self, Read, Seek, SeekFrom, Write},
        os::unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        },
        path::{Path, PathBuf},
    };

    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    pub(super) struct Socket {
        pub(super) listener: UnixListener,
        path: PathBuf,
        identity: (u64, u64),
        // Never unlink the lock file: otherwise two daemons can lock different inodes.
        _lock: File,
    }

    impl Socket {
        pub(super) fn bind(path: &Path) -> io::Result<Self> {
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unix socket path must be absolute",
                ));
            }
            let parent = path.parent().expect("absolute socket has parent");
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            let metadata = fs::symlink_metadata(parent)?;
            // SAFETY: geteuid has no parameters or memory effects.
            let owner = unsafe { geteuid() };
            if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Unix socket directory {} must be owned by this user with mode 0700",
                        parent.display()
                    ),
                ));
            }

            let mut lock_name = OsString::from(path.file_name().expect("validated"));
            lock_name.push(".lock");
            let lock_path = parent.join(lock_name);
            if let Ok(metadata) = fs::symlink_metadata(&lock_path)
                && !metadata.is_file()
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "socket lock path is not a regular file",
                ));
            }
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true).mode(0o600);
            // Refuse symlinks even if changed between the metadata check and open.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            options.custom_flags(0x20000); // O_NOFOLLOW
            #[cfg(target_os = "macos")]
            options.custom_flags(0x100); // O_NOFOLLOW
            let (mut lock, created) = match options.open(&lock_path) {
                Ok(file) => (file, true),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    options.create_new(false);
                    (options.open(&lock_path)?, false)
                }
                Err(error) => return Err(error),
            };
            let metadata = lock.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != owner
                || metadata.nlink() != 1
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "socket lock must be a private file owned by this user",
                ));
            }
            lock.try_lock().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another zcalld owns this Unix socket",
                )
            })?;
            let mut record = String::new();
            (&mut lock).take(128).read_to_string(&mut record)?;
            if created {
                record = "zcalld-socket-v1 vacant\n".into();
                lock.write_all(record.as_bytes())?;
                lock.flush()?;
            } else {
                let fields: Vec<_> = record.split_whitespace().collect();
                let valid = record == "zcalld-socket-v1 vacant\n"
                    || (fields.len() == 3
                        && fields[0] == "zcalld-socket-v1"
                        && fields[1].parse::<u64>().is_ok()
                        && fields[2].parse::<u64>().is_ok());
                if !valid || metadata.len() > 128 {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "socket lock path is not a recognized zcalld record",
                    ));
                }
            }

            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    let identity = (metadata.dev(), metadata.ino());
                    if !metadata.file_type().is_socket()
                        || metadata.uid() != owner
                        || record != format!("zcalld-socket-v1 {} {}\n", identity.0, identity.1)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "socket path already exists and is not a recorded stale zcalld socket",
                        ));
                    }
                    // The held lock proves the recorded zcalld instance is gone.
                    // Unknown sockets and filesystem entries are never removed.
                    fs::remove_file(path)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(path)?;
            let metadata = fs::symlink_metadata(path)?;
            let socket = Self {
                listener,
                path: path.to_path_buf(),
                identity: (metadata.dev(), metadata.ino()),
                _lock: lock,
            };
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            socket.listener.set_nonblocking(true)?;
            let mut lock = &socket._lock;
            lock.seek(SeekFrom::Start(0))?;
            lock.set_len(0)?;
            writeln!(
                lock,
                "zcalld-socket-v1 {} {}",
                socket.identity.0, socket.identity.1
            )?;
            lock.flush()?;
            Ok(socket)
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            if let Ok(metadata) = fs::symlink_metadata(&self.path)
                && metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino()) == self.identity
            {
                let _ = fs::remove_file(&self.path);
            }
            // close alone retains flock while a dup or pre-exec child still
            // holds this open-file description. Our ownership ends here.
            let _ = self._lock.unlock();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Directory(PathBuf);
        impl Directory {
            fn new() -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "zcalld-socket-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
                Self(path)
            }
            fn socket(&self) -> PathBuf {
                self.0.join("gateway.sock")
            }
        }
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn private_socket_exchanges_bytes_and_cleans_up() {
            let directory = Directory::new();
            let socket = Socket::bind(&directory.socket()).unwrap();
            assert_eq!(
                fs::metadata(directory.socket()).unwrap().mode() & 0o777,
                0o600
            );
            let mut client = UnixStream::connect(directory.socket()).unwrap();
            let (mut server, _) = socket.listener.accept().unwrap();
            client.write_all(b"hello").unwrap();
            let mut buffer = [0; 5];
            server.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, b"hello");
            assert!(Socket::bind(&directory.socket()).is_err());
            drop(socket);
            assert!(!directory.socket().exists());
            assert!(Socket::bind(&directory.socket()).is_ok());
        }

        #[test]
        fn drop_releases_lock_while_an_inherited_descriptor_remains_open() {
            let directory = Directory::new();
            let socket = Socket::bind(&directory.socket()).unwrap();
            // dup and fork share the open-file description. A child can inherit
            // it until exec even when the descriptor is close-on-exec.
            let inherited = socket._lock.try_clone().unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
            drop(socket);
            let restarted = Socket::bind(&directory.socket()).unwrap();
            assert!(UnixStream::connect(directory.socket()).is_ok());
            drop(inherited);
            assert!(Socket::bind(&directory.socket()).is_err());
            drop(restarted);
            assert!(!directory.socket().exists());
        }

        #[test]
        fn does_not_replace_files_symlinks_or_unknown_sockets() {
            let directory = Directory::new();
            fs::write(directory.socket(), "keep").unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
            assert_eq!(fs::read_to_string(directory.socket()).unwrap(), "keep");
            fs::remove_file(directory.socket()).unwrap();
            std::os::unix::fs::symlink("missing", directory.socket()).unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
            assert!(
                fs::symlink_metadata(directory.socket())
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            fs::remove_file(directory.socket()).unwrap();
            let listener = UnixListener::bind(directory.socket()).unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
            drop(listener);
            assert!(directory.socket().exists());
        }

        #[test]
        fn reclaims_only_recorded_stale_socket() {
            let directory = Directory::new();
            let socket = Socket::bind(&directory.socket()).unwrap();
            // Leave the recorded inode on disk while closing both descriptors.
            let saved = directory.0.join("saved.sock");
            fs::rename(directory.socket(), &saved).unwrap();
            drop(socket);
            fs::rename(saved, directory.socket()).unwrap();
            let restarted = Socket::bind(&directory.socket()).unwrap();
            assert!(UnixStream::connect(directory.socket()).is_ok());
            drop(restarted);
            assert!(!directory.socket().exists());
        }

        #[test]
        fn cleanup_preserves_replaced_path_and_rejects_public_directory() {
            let directory = Directory::new();
            let socket = Socket::bind(&directory.socket()).unwrap();
            fs::remove_file(directory.socket()).unwrap();
            fs::write(directory.socket(), "replacement").unwrap();
            drop(socket);
            assert_eq!(
                fs::read_to_string(directory.socket()).unwrap(),
                "replacement"
            );
            fs::remove_file(directory.socket()).unwrap();
            fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
        }

        #[test]
        fn refuses_to_overwrite_an_unrelated_lock_file() {
            let directory = Directory::new();
            let lock = directory.0.join("gateway.sock.lock");
            fs::write(&lock, "keep unrelated data").unwrap();
            fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(Socket::bind(&directory.socket()).is_err());
            assert_eq!(fs::read_to_string(lock).unwrap(), "keep unrelated data");
            assert!(!directory.socket().exists());
        }
    }
}

// Keep native listener/peer identity policy here while sharing HTTP and protocol I/O.
impl zcall::server::Connection for Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Stream::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Stream::set_write_timeout(self, timeout)
    }
    fn shutdown_write(&self) -> io::Result<()> {
        Stream::shutdown_write(self)
    }
    fn cancelled(&self) -> bool {
        system::STOPPING.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn error_type(&self) -> &'static str {
        "zcalld_error"
    }
}

// A borrowed I/O view keeps socket observation available while a framing writer
// holds its Connection trait object. It does not duplicate or own the socket.
impl zcall::server::Connection for &Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Stream::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Stream::set_write_timeout(self, timeout)
    }
    fn shutdown_write(&self) -> io::Result<()> {
        Stream::shutdown_write(self)
    }
    fn cancelled(&self) -> bool {
        <Stream as zcall::server::Connection>::cancelled(self)
    }
    fn error_type(&self) -> &'static str {
        <Stream as zcall::server::Connection>::error_type(self)
    }
}
