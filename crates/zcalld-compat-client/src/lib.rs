//! Bounded stdio transport for optional local compatibility workers.
pub mod json;
use json::{Value, object};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
const MAX_LINE: usize = 24 * 1024 * 1024;
const MAX_CALLS: usize = 64;
static HOSTS: OnceLock<Mutex<HashMap<String, Weak<Inner>>>> = OnceLock::new();
struct Inner {
    input: Mutex<ChildStdin>,
    child: Mutex<Child>,
    calls: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    next: AtomicU64,
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
        if let Ok(c) = self.child.get_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
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
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|_| "optional compatibility worker is unavailable")?;
        let input = child.stdin.take().ok_or("worker stdin unavailable")?;
        let output = child.stdout.take().ok_or("worker stdout unavailable")?;
        let inner = Arc::new(Inner {
            input: Mutex::new(input),
            child: Mutex::new(child),
            calls: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        });
        hosts.insert(key.into(), Arc::downgrade(&inner));
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("zcalld-compat-read".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(65536, output);
                loop {
                    let mut line = Vec::new();
                    match reader
                        .by_ref()
                        .take((MAX_LINE + 1) as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if line.len() > MAX_LINE || line.last() != Some(&b'\n') {
                        break;
                    }
                    let Ok(value) = json::parse(&line) else { break };
                    let Some(inner) = weak.upgrade() else { break };
                    if let Some(id) = value.get("id").as_u64() {
                        if let Ok(calls) = inner.calls.lock() {
                            if let Some(tx) = calls.get(&id) {
                                let _ = tx.send(value);
                            }
                        }
                    }
                }
                if let Some(inner) = weak.upgrade() {
                    if let Ok(mut calls) = inner.calls.lock() {
                        calls.clear();
                    }
                    if let Ok(mut child) = inner.child.lock() {
                        let _ = child.kill();
                    }
                }
            })
            .map_err(|_| "worker reader unavailable")?;
        Ok((Self(inner), true))
    }
    pub fn start(&self, mut request: Value) -> Result<Call, String> {
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
        self.0
            .input
            .lock()
            .map_err(|_| "worker input poisoned")?
            .write_all(line.as_bytes())
            .map_err(|_| "compatibility worker ended".to_owned())
    }
    pub fn rpc(&self, request: Value) -> Result<Value, String> {
        let mut call = self.start(request)?;
        loop {
            let value = call.next()?;
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
    pub fn next(&mut self) -> Result<Value, String> {
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
