//! Bounded stdio transport for optional local compatibility workers.
pub mod helpers;
pub mod json;
use json::{Value, object};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const MAX_LINE: usize = 24 * 1024 * 1024;
const MAX_CALLS: usize = 64;
static HOSTS: OnceLock<Mutex<HashMap<String, Weak<Inner>>>> = OnceLock::new();
struct Inner {
    input: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    calls: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    alive: Arc<AtomicBool>,
    next: AtomicU64,
    drain_deadline: Option<Instant>,
}
#[derive(Clone)]
pub struct Host(Arc<Inner>);
pub struct Call {
    host: Host,
    id: u64,
    receiver: mpsc::Receiver<Value>,
    done: bool,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut calls) = self.calls.lock() {
            calls.clear();
        }
        // EOF asks the pinned host to finish any rotating-token write before exit.
        self.input
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let usual = Instant::now() + Duration::from_secs(35);
        let deadline = self.drain_deadline.map_or(usual, |limit| limit.min(usual));
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Host {
    pub fn shared(
        key: &str,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<(Self, bool), String> {
        let mut hosts = HOSTS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| "worker registry poisoned")?;
        if let Some(inner) = hosts.get(key).and_then(Weak::upgrade) {
            return Ok((Self(inner), false));
        }
        let host = Self::isolated(program, args, cwd)?;
        hosts.insert(key.into(), Arc::downgrade(&host.0));
        Ok((host, true))
    }

    /// A one-shot worker owns its stdin, calls, reader and drain independently.
    /// It never enters the shared weak registry or borrows another invocation.
    pub fn isolated(program: &Path, args: &[String], cwd: Option<&Path>) -> Result<Self, String> {
        Self::spawn(program, args, cwd, None)
    }

    /// Keep a one-shot operation and its credential drain within one deadline.
    pub fn isolated_until(
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        deadline: Instant,
    ) -> Result<Self, String> {
        Self::spawn(program, args, cwd, Some(deadline))
    }

    fn spawn(
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        drain_deadline: Option<Instant>,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            // IPC workers do not own the console. Ctrl+Break must reach the
            // gateway first so it can cancel calls and drain pending auth writes
            // through stdin EOF, instead of terminating the worker mid-write.
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|_| "optional compatibility worker is unavailable")?;
        let input = child.stdin.take().ok_or("worker stdin unavailable")?;
        let output = child.stdout.take().ok_or("worker stdout unavailable")?;
        let inner = Arc::new(Inner {
            input: Mutex::new(Some(input)),
            child: Mutex::new(child),
            calls: Arc::new(Mutex::new(HashMap::new())),
            alive: Arc::new(AtomicBool::new(true)),
            next: AtomicU64::new(1),
            drain_deadline,
        });
        let calls = Arc::clone(&inner.calls);
        let alive = Arc::clone(&inner.alive);
        std::thread::Builder::new()
            .name("zcalld-compat-read".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(65536, output);
                loop {
                    if !alive.load(Ordering::Acquire) {
                        let _ = std::io::copy(&mut reader, &mut std::io::sink());
                        break;
                    }
                    let mut line = Vec::new();
                    match reader
                        .by_ref()
                        .take((MAX_LINE + 1) as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let value = if line.len() <= MAX_LINE && line.last() == Some(&b'\n') {
                        json::parse(&line).ok()
                    } else {
                        None
                    };
                    let Some(value) = value else {
                        alive.store(false, Ordering::Release);
                        if let Ok(mut calls) = calls.lock() {
                            calls.clear();
                        }
                        // Keep stdout open while EOF drains a pending credential refresh.
                        let _ = std::io::copy(&mut reader, &mut std::io::sink());
                        break;
                    };
                    if let Some(id) = value.get("id").as_u64()
                        && let Ok(calls) = calls.lock()
                        && let Some(tx) = calls.get(&id)
                    {
                        let _ = tx.send(value);
                    }
                }
                alive.store(false, Ordering::Release);
                if let Ok(mut calls) = calls.lock() {
                    calls.clear();
                }
            })
            .map_err(|_| "worker reader unavailable")?;
        Ok(Self(inner))
    }
    pub fn start(&self, mut request: Value) -> Result<Call, String> {
        if !self.0.alive.load(Ordering::Acquire) {
            return Err("compatibility worker ended".into());
        }
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut calls = self
                .0
                .calls
                .lock()
                .map_err(|_| "worker registry poisoned")?;
            if calls.len() >= MAX_CALLS {
                return Err("compatibility worker is busy".into());
            }
            calls.insert(id, tx);
        }
        request.set("id", id.into());
        if let Err(e) = self.send(request) {
            if let Ok(mut calls) = self.0.calls.lock() {
                calls.remove(&id);
            }
            return Err(e);
        }
        Ok(Call {
            host: self.clone(),
            id,
            receiver: rx,
            done: false,
        })
    }
    pub fn send(&self, value: Value) -> Result<(), String> {
        let mut line = value.stringify();
        if line.len() > MAX_LINE {
            return Err("compatibility request exceeds limit".into());
        }
        line.push('\n');
        if !self.0.alive.load(Ordering::Acquire) {
            return Err("compatibility worker ended".into());
        }
        self.0
            .input
            .lock()
            .map_err(|_| "worker input poisoned")?
            .as_mut()
            .ok_or("compatibility worker ended")?
            .write_all(line.as_bytes())
            .map_err(|_| "compatibility worker ended".to_owned())
    }
    pub fn rpc(&self, request: Value) -> Result<Value, String> {
        let mut call = self.start(request)?;
        loop {
            let value = call.recv()?;
            if !matches!(value.get("error"), Value::Null) {
                return Err("compatibility worker rejected the operation".into());
            }
            if value.as_object().is_some_and(|v| v.contains_key("result")) {
                call.done = true;
                return Ok(value.get("result").clone());
            }
        }
    }
}
impl Call {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn poll(&mut self) -> Result<Option<Value>, String> {
        match self.receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(v) => Ok(Some(v)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => Err("compatibility worker ended".into()),
        }
    }
    pub fn recv(&mut self) -> Result<Value, String> {
        self.receiver
            .recv_timeout(Duration::from_secs(300))
            .map_err(|_| "compatibility worker timed out or ended".into())
    }
    pub fn credit(&self, n: usize) -> Result<(), String> {
        self.host.send(object([
            ("method", "credit".into()),
            ("params", object([("id", self.id.into()), ("n", n.into())])),
        ]))
    }
    pub fn finish(&mut self) {
        self.done = true
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        if let Ok(mut calls) = self.host.0.calls.lock() {
            calls.remove(&self.id);
        }
        if !self.done {
            let _ = self.host.send(object([
                ("method", "abort".into()),
                ("params", object([("id", self.id.into())])),
            ]));
        }
    }
}
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let v = ((c[0] as u32) << 16)
            | ((c.get(1).copied().unwrap_or(0) as u32) << 8)
            | (c.get(2).copied().unwrap_or(0) as u32);
        for shift in [18, 12, 6, 0] {
            out.push(B64[((v >> shift) & 63) as usize] as char)
        }
        if c.len() < 3 {
            out.pop();
            out.push('=');
        }
        if c.len() == 1 {
            out.pop();
            out.pop();
            out.push('=');
            out.push('=');
        }
    }
    out
}
pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(4) || s.len() > MAX_LINE {
        return Err("invalid encoded worker output".into());
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (index, c) in s.as_bytes().chunks(4).enumerate() {
        let mut v = 0u32;
        let mut padding = 0;
        for (at, b) in c.iter().enumerate() {
            v <<= 6;
            if *b == b'=' {
                if at < 2 {
                    return Err("invalid base64 padding".into());
                }
                padding += 1;
            } else {
                if padding != 0 {
                    return Err("invalid base64 padding".into());
                }
                v |= B64
                    .iter()
                    .position(|x| x == b)
                    .ok_or("invalid base64 character")? as u32;
            }
        }
        if padding > 0 && index + 1 != s.len() / 4 {
            return Err("invalid base64 padding".into());
        }
        out.push((v >> 16) as u8);
        if padding < 2 {
            out.push((v >> 8) as u8)
        }
        if padding == 0 {
            out.push(v as u8)
        }
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn last_lease_drains_stdout_and_waits_for_private_write() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("zcalld-drain-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let checkpoint = dir.join("refresh-checkpoint");
        let args = vec![
            "-c".into(),
            "cat >/dev/null; sleep 0.1; head -c 2097152 /dev/zero; printf rotated > \"$1\"".into(),
            "fixture".into(),
            checkpoint.to_string_lossy().into_owned(),
        ];
        let (host, fresh) = Host::shared(
            &dir.to_string_lossy(),
            Path::new("/bin/sh"),
            &args,
            Some(&dir),
        )
        .unwrap();
        assert!(fresh);
        drop(host);
        assert_eq!(std::fs::read(&checkpoint).unwrap(), b"rotated");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn isolated_workers_do_not_share_calls_or_lifetimes() {
        let program = Path::new("/bin/sh");
        let script = "IFS= read -r line; printf '{\"id\":1,\"result\":{\"owner\":\"%s\"}}\\n' \"$1\"; cat >/dev/null";
        let first = Host::isolated(
            program,
            &["-c".into(), script.into(), "fixture".into(), "first".into()],
            None,
        )
        .unwrap();
        let second = Host::isolated(
            program,
            &[
                "-c".into(),
                script.into(),
                "fixture".into(),
                "second".into(),
            ],
            None,
        )
        .unwrap();
        let mut a = first.start(object([("method", "work".into())])).unwrap();
        let mut b = second.start(object([("method", "work".into())])).unwrap();
        assert_eq!(
            a.recv().unwrap().get("result").get("owner").as_str(),
            Some("first")
        );
        a.finish();
        drop(a);
        drop(first);
        assert_eq!(
            b.recv().unwrap().get("result").get("owner").as_str(),
            Some("second")
        );
        b.finish();
        drop(b);
        drop(second);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_drain_does_not_add_a_second_deadline() {
        let started = Instant::now();
        let host = Host::isolated_until(
            Path::new("/bin/sh"),
            &["-c".into(), "exec sleep 10".into()],
            None,
            started + Duration::from_millis(100),
        )
        .unwrap();
        drop(host);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn encodings_roundtrip() {
        for v in [b"".as_slice(), b"a", b"ab", b"abc", b"hello\x00world"] {
            assert_eq!(decode(&encode(v)).unwrap(), v)
        }
        assert_eq!(encode(b"a"), "YQ==");
        assert_eq!(decode("YWI=").unwrap(), b"ab");
        assert!(decode("AA=A").is_err());
    }
}
