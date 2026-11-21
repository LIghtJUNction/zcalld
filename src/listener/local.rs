//! One live lease, one generation at a time, no automatic reconnection or replay.
mod generation;

use crate::{
    config::Config,
    state_json::{Value, object},
    system::STOPPING,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::{Component, Path, PathBuf},
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_FRAME: usize = 256 * 1024;
const MAX_REQUEST: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(6);

struct Args {
    socket: PathBuf,
    config: PathBuf,
    model: String,
    token_env: Option<String>,
    timeout_ms: Option<u64>,
    once: bool,
    json: bool,
}

impl Args {
    fn parse(args: &[String]) -> Result<Self, String> {
        let (mut socket, mut config, mut model, mut token_env, mut timeout_ms) =
            (None, None, None, None, None);
        let (mut once, mut json) = (false, false);
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--ssmcp-socket" if socket.is_none() => {
                    socket = Some(PathBuf::from(
                        args.next().ok_or("--ssmcp-socket requires a path")?,
                    ))
                }
                "--config" | "-c" if config.is_none() => {
                    config = Some(PathBuf::from(
                        args.next().ok_or("--config requires a path")?,
                    ))
                }
                "--model" if model.is_none() => {
                    model = Some(args.next().ok_or("--model requires a route ID")?.clone())
                }
                "--token-env" if token_env.is_none() => {
                    token_env = Some(
                        args.next()
                            .ok_or("--token-env requires a variable name")?
                            .clone(),
                    )
                }
                "--timeout-ms" if timeout_ms.is_none() => {
                    let value: u64 = args
                        .next()
                        .ok_or("--timeout-ms requires a value")?
                        .parse()
                        .map_err(|_| "invalid listener timeout")?;
                    if !(1000..=900_000).contains(&value) {
                        return Err("listener timeout must be 1000..=900000 ms".into());
                    }
                    timeout_ms = Some(value);
                }
                "--once" if !once => once = true,
                "--json" if !json => json = true,
                _ => return Err("unknown or repeated listen option; use --help".into()),
            }
        }
        let socket = socket.ok_or("listen requires --ssmcp-socket PATH")?;
        let model: String = model.ok_or("listen requires --model ROUTE")?;
        if !socket.is_absolute()
            || socket
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err("SSMCP socket must be an absolute path without parent traversal".into());
        }
        if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
            return Err("listener model must be an exact route ID of 1..=256 bytes".into());
        }
        if token_env.as_ref().is_some_and(|name: &String| {
            name.is_empty()
                || name.len() > 128
                || !name.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                })
        }) {
            return Err("invalid caller token environment variable name".into());
        }
        Ok(Self {
            socket,
            config: config.map(Ok).unwrap_or_else(crate::config::default_path)?,
            model,
            token_env,
            timeout_ms,
            once,
            json,
        })
    }
}

/// Protect the same-UID local capability before connecting. A different user
/// cannot replace a socket inside the required private owner directory.
fn private_socket(path: &Path) -> Result<(), String> {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments and no ownership or lifetime requirements.
    let uid = unsafe { geteuid() };
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("socket path contains parent traversal".into());
        }
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| "local socket path is unavailable")?;
        if metadata.file_type().is_symlink() {
            return Err("local socket path must not contain symlinks".into());
        }
    }
    let parent = path.parent().ok_or("socket has no private parent")?;
    let directory = fs::symlink_metadata(parent).map_err(|_| "socket parent is unavailable")?;
    let socket = fs::symlink_metadata(path).map_err(|_| "socket is unavailable")?;
    if !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o077 != 0
        || !socket.file_type().is_socket()
        || socket.uid() != uid
        || socket.mode() & 0o077 != 0
    {
        return Err(
            "local socket and parent must be private and owned by this effective UID".into(),
        );
    }
    Ok(())
}

fn exchange(path: &Path, request: &Value) -> Result<Value, String> {
    let body = request.stringify();
    if body.is_empty() || body.len() > MAX_REQUEST {
        return Err("listener request exceeds 64 KiB".into());
    }
    private_socket(path)?;
    let mut socket = UnixStream::connect(path)
        .map_err(|_| "SSMCP listener disconnected; no automatic reconnect or replay")?;
    socket
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| socket.set_write_timeout(Some(IO_TIMEOUT)))
        .map_err(|_| "cannot set listener I/O timeout")?;
    socket
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|()| socket.write_all(body.as_bytes()))
        .map_err(|_| "listener request failed; delivery outcome may be unknown")?;
    // Do not half-close: the listener treats EOF as cancellation.
    let mut length = [0u8; 4];
    socket
        .read_exact(&mut length)
        .map_err(|_| "listener response missing; outcome is unknown")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("listener response exceeds bounded frame".into());
    }
    let mut bytes = vec![0; length];
    socket
        .read_exact(&mut bytes)
        .map_err(|_| "incomplete listener response; outcome is unknown")?;
    let response =
        crate::state_json::parse(&bytes).map_err(|_| "invalid listener JSON response")?;
    if response.get("ok").as_bool() != Some(true) {
        let error = response
            .get("error")
            .as_str()
            .unwrap_or("listener rejected request");
        if error.len() > 1024 || error.chars().any(char::is_control) {
            return Err("listener rejected request".into());
        }
        return Err(format!("listener: {error}"));
    }
    Ok(response.get("result").clone())
}

// Never Debug/Serialize: the token remains only in this process's memory.
struct Lease {
    socket: PathBuf,
    id: String,
    token: String,
    connection: String,
}

impl Lease {
    fn open(socket: PathBuf, model: &str) -> Result<Self, String> {
        let mut label = format!("zcalld / {model}");
        while label.len() > 96 {
            label.pop();
        }
        let key = format!(
            "zcalld_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock is invalid")?
                .as_nanos()
        );
        let value = exchange(
            &socket,
            &object([
                ("op", "open".into()),
                ("label", label.into()),
                ("idempotency_key", key.into()),
                ("enable_reply", true.into()),
            ]),
        )?;
        let identity = |field: &str, length: usize| -> Result<String, String> {
            let value = value
                .get(field)
                .as_str()
                .ok_or("listener open response is incomplete")?;
            if value.len() != length || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("listener open response has an invalid capability identity".into());
            }
            Ok(value.into())
        };
        let id = identity("listener_id", 32)?;
        let token = identity("token", 64)?;
        let connection = value
            .get("connection_id")
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .ok_or("listener open response has no connection identity")?
            .to_owned();
        let lease = Self {
            socket,
            id,
            token,
            connection,
        };
        if value.get("schema_version").as_u64() != Some(1)
            || value.get("reply_enabled").as_bool() != Some(true)
        {
            let _ = lease.close();
            return Err("SSMCP client does not support local execution results".into());
        }
        Ok(lease)
    }

    fn request(&self, op: &str) -> Value {
        object([
            ("op", op.into()),
            ("listener_id", self.id.clone().into()),
            ("token", self.token.clone().into()),
        ])
    }

    fn read(&self, wait_ms: u64) -> Result<Option<Delivery>, String> {
        let mut request = self.request("read");
        request.set("wait_ms", wait_ms.into());
        let value = exchange(&self.socket, &request)?;
        let delivery = value.get("delivery");
        if matches!(delivery, Value::Null) {
            return Ok(None);
        }
        Delivery::parse(delivery, &self.id, &self.connection).map(Some)
    }

    fn ack(&self, delivery: &Delivery, accepted: bool) -> Result<Value, String> {
        let mut request = self.request("ack");
        request.set("delivery_id", delivery.id.clone().into());
        request.set(
            "status",
            if accepted { "accepted" } else { "rejected" }.into(),
        );
        if !accepted {
            request.set("reason", "zcalld 正在生成上一条结果，请稍后重新发送".into());
        }
        let value = exchange(&self.socket, &request)?;
        let receipt = value.get("receipt");
        if receipt.get("delivery_id").as_str() != Some(delivery.id.as_str())
            || receipt.get("connection_id").as_str() != Some(self.connection.as_str())
            || receipt.get("listener_id").as_str() != Some(self.id.as_str())
            || receipt.get("status").as_str()
                != Some(if accepted { "accepted" } else { "rejected" })
        {
            return Err("listener ACK did not confirm this exact delivery".into());
        }
        Ok(receipt.clone())
    }

    fn reply(&self, delivery: &Delivery, result: Result<String, String>) -> Result<Value, String> {
        let (state, text) = match result {
            Ok(text) => ("completed", text),
            Err(error) => ("failed", format!("zcalld generation failed: {error}")),
        };
        let mut request = self.request("reply");
        request.set("delivery_id", delivery.id.clone().into());
        request.set("execution_state", state.into());
        request.set("text", text.into());
        let value = exchange(&self.socket, &request)?;
        let reply = value.get("reply");
        if reply.get("delivery_id").as_str() != Some(delivery.id.as_str())
            || reply.get("connection_id").as_str() != Some(self.connection.as_str())
            || reply.get("listener_id").as_str() != Some(self.id.as_str())
            || reply.get("conversation_id").as_str() != Some(delivery.conversation.as_str())
            || reply.get("execution_state").as_str() != Some(state)
            || reply.get("history_saved").as_bool() != Some(true)
        {
            return Err("listener did not confirm the result in its original conversation".into());
        }
        Ok(reply.clone())
    }

    fn close(&self) -> Result<(), String> {
        exchange(&self.socket, &self.request("close")).map(|_| ())
    }
}

struct Delivery {
    id: String,
    conversation: String,
    text: String,
}
impl Delivery {
    fn parse(value: &Value, listener: &str, connection: &str) -> Result<Self, String> {
        if value.get("listener_id").as_str() != Some(listener)
            || value.get("connection_id").as_str() != Some(connection)
        {
            return Err("listener delivery belongs to another lease or connection".into());
        }
        let id = value
            .get("delivery_id")
            .as_str()
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("listener delivery has no valid identity")?;
        let conversation = value
            .get("conversation_id")
            .as_str()
            .filter(|s| {
                s.starts_with("conversation_")
                    && s.len() == 45
                    && s[13..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or("listener delivery has no bound conversation")?;
        let text = value
            .get("text")
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 192 * 1024)
            .ok_or("listener delivery text is missing or too large")?;
        Ok(Self {
            id: id.into(),
            conversation: conversation.into(),
            text: text.into(),
        })
    }
}

fn emit(json: bool, event: &str, data: Value) {
    if json {
        println!(
            "{}",
            object([("event", event.into()), ("data", data)]).stringify()
        );
    } else {
        match event {
            "bound" => eprintln!(
                "zcalld listening on one SSMCP conversation; input receipt and generation result are separate"
            ),
            "accepted" => eprintln!("zcalld accepted input; generating"),
            "result" => eprintln!(
                "zcalld result: {}",
                data.get("execution_state").as_str().unwrap_or("unknown")
            ),
            "busy" => {
                eprintln!("zcalld rejected new input while the previous generation is running")
            }
            _ => {}
        }
    }
}

fn generate(
    lease: &Lease,
    gateway: &generation::Gateway,
    delivery: &Delivery,
    json: bool,
) -> Result<Result<String, String>, String> {
    std::thread::scope(|threads| {
        let (send, receive) = mpsc::sync_channel(1);
        threads.spawn(move || {
            let _ =
                send.send(gateway.generate(&delivery.text, &delivery.conversation, &delivery.id));
        });
        let mut heartbeat = Instant::now();
        loop {
            if STOPPING.load(Ordering::Relaxed) {
                return Err("listener stopped; unfinished generation is not replayed".into());
            }
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Ok(result),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(Err("generation worker stopped".into()));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if heartbeat.elapsed() >= Duration::from_secs(1) {
                let checked = (|| {
                    if let Some(extra) = lease.read(0)? {
                        emit(json, "busy", lease.ack(&extra, false)?);
                    }
                    Ok::<(), String>(())
                })();
                if let Err(error) = checked {
                    STOPPING.store(true, Ordering::Relaxed);
                    return Err(error);
                }
                heartbeat = Instant::now();
            }
        }
    })
}

pub(super) fn run(args: &[String]) -> Result<(), String> {
    let args = Args::parse(args)?;
    let config = Config::load(&args.config)?;
    let gateway = generation::Gateway::new(
        &config,
        &args.model,
        args.token_env.as_deref(),
        args.timeout_ms,
    )?;
    crate::system::setup_signals().map_err(|_| "cannot install listener shutdown handlers")?;
    let lease = Lease::open(args.socket, &args.model)?;
    emit(
        args.json,
        "bound",
        object([
            ("listener_id", lease.id.clone().into()),
            ("connection_id", lease.connection.clone().into()),
            ("model", args.model.clone().into()),
        ]),
    );
    let result = (|| {
        while !STOPPING.load(Ordering::Relaxed) {
            let Some(delivery) = lease.read(1000)? else {
                continue;
            };
            emit(args.json, "accepted", lease.ack(&delivery, true)?);
            let result = generate(&lease, &gateway, &delivery, args.json)?;
            // No retry: a missing reply receipt is ambiguous and never starts another model call.
            emit(args.json, "result", lease.reply(&delivery, result)?);
            if args.once {
                break;
            }
        }
        Ok(())
    })();
    let closed = lease.close();
    result.and(closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn native_listener_requires_an_explicit_socket_and_route_and_rejects_duplicate_options() {
        let good = words(&[
            "--ssmcp-socket",
            "/tmp/private/receiver.sock",
            "--model",
            "exact-route",
            "--config",
            "/tmp/config",
            "--once",
            "--json",
        ]);
        let args = Args::parse(&good).unwrap();
        assert!(args.once && args.json);
        assert_eq!(args.model, "exact-route");
        for invalid in [
            words(&["--model", "exact-route"]),
            words(&["--ssmcp-socket", "relative.sock", "--model", "exact-route"]),
            words(&[
                "--ssmcp-socket",
                "/tmp/../bad.sock",
                "--model",
                "exact-route",
            ]),
            words(&[
                "--ssmcp-socket",
                "/tmp/socket",
                "--model",
                "route",
                "--model",
                "other",
            ]),
            words(&[
                "--ssmcp-socket",
                "/tmp/socket",
                "--model",
                "route",
                "--timeout-ms",
                "0",
            ]),
        ] {
            assert!(Args::parse(&invalid).is_err());
        }
    }

    fn delivery() -> Value {
        object([
            ("delivery_id", "a".repeat(32).into()),
            ("listener_id", "b".repeat(32).into()),
            ("connection_id", "connection-a".into()),
            (
                "conversation_id",
                format!("conversation_{}", "c".repeat(32)).into(),
            ),
            ("text", "  原文 @literal $skill\nsecond line  ".into()),
        ])
    }

    #[test]
    fn native_listener_delivery_keeps_literal_text_and_requires_exact_scope() {
        let value = delivery();
        let parsed = Delivery::parse(&value, &"b".repeat(32), "connection-a").unwrap();
        assert_eq!(parsed.text, "  原文 @literal $skill\nsecond line  ");
        assert!(Delivery::parse(&value, &"b".repeat(32), "connection-b").is_err());
        let mut value = value;
        value.remove("conversation_id");
        assert!(Delivery::parse(&value, &"b".repeat(32), "connection-a").is_err());
    }

    struct SocketFixture {
        directory: PathBuf,
        path: PathBuf,
        listener: UnixListener,
    }
    impl SocketFixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "zcalld-listener-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("receiver.sock");
            let listener = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                directory,
                path,
                listener,
            }
        }
    }
    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn native_listener_private_socket_uses_one_bounded_frame_and_no_half_close() {
        let fixture = SocketFixture::new();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let (mut socket, _) = fixture.listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut length = [0; 4];
                socket.read_exact(&mut length).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
                socket.read_exact(&mut bytes).unwrap();
                assert_eq!(
                    crate::state_json::parse(&bytes).unwrap().get("op").as_str(),
                    Some("status")
                );
                let response = br#"{"ok":true,"result":{"schema_version":1}}"#;
                socket
                    .write_all(&(response.len() as u32).to_be_bytes())
                    .unwrap();
                socket.write_all(response).unwrap();
            });
            assert_eq!(
                exchange(&fixture.path, &object([("op", "status".into())]))
                    .unwrap()
                    .get("schema_version")
                    .as_u64(),
                Some(1)
            );
        });
        fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_socket(&fixture.path).is_err());
    }

    #[test]
    fn native_listener_rejects_oversized_response_before_allocating_body() {
        let fixture = SocketFixture::new();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let (mut socket, _) = fixture.listener.accept().unwrap();
                let mut length = [0; 4];
                socket.read_exact(&mut length).unwrap();
                let mut request = vec![0; u32::from_be_bytes(length) as usize];
                socket.read_exact(&mut request).unwrap();
                socket
                    .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
                    .unwrap();
            });
            assert!(
                exchange(&fixture.path, &object([("op", "status".into())]))
                    .unwrap_err()
                    .contains("bounded")
            );
        });
    }
}
