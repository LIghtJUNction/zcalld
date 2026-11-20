use super::*;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "zcalld-desktop-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        let p = p.canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(p)
    }
    fn config(&self) -> String {
        zcalld_compat_client::json::object([
            ("enabled", true.into()),
            ("keep_awake", true.into()),
            ("usage_alert", 90u64.into()),
            ("turn_done_seconds", 1u64.into()),
            ("mask_alert", true.into()),
            ("backend", "record".into()),
            (
                "state_dir",
                self.0.join("state").to_string_lossy().into_owned().into(),
            ),
            (
                "inbox",
                self.0.join("inbox").to_string_lossy().into_owned().into(),
            ),
        ])
        .stringify()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < deadline, "desktop worker did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn configure_and_status_do_not_start_worker_or_touch_files() {
    let temp = Temp::new();
    let mut p = Desktop::default();
    EventPlugin::configure(&mut p, &temp.config()).unwrap();
    let mut out = [0; 4096];
    let n = p.status(&mut out).unwrap() as usize;
    let v = json::parse(&out[..n]).unwrap();
    assert_eq!(v.get("running").as_bool(), Some(false));
    assert!(!temp.0.join("state").exists());
    assert!(!temp.0.join("inbox").exists());
}
#[test]
fn instances_share_worker_and_cancelled_lease_releases_activity() {
    let temp = Temp::new();
    let mut a = Desktop::default();
    let mut b = Desktop::default();
    EventPlugin::configure(&mut a, &temp.config()).unwrap();
    EventPlugin::configure(&mut b, &temp.config()).unwrap();
    assert!(Arc::ptr_eq(a.hub().unwrap(), b.hub().unwrap()));
    let lease=a.begin(r#"{"version":1,"protocol":1,"agent":"codex","session":"private-digest","kind":"generation","subagent":false}"#).unwrap();
    assert_eq!(a.hub().unwrap().state.active.load(Ordering::Acquire), 1);
    wait(|| a.hub().unwrap().state.activation.load(Ordering::Acquire) == 2);
    wait(|| temp.0.join("state/recorded.jsonl").is_file());
    drop(lease);
    assert_eq!(a.hub().unwrap().state.active.load(Ordering::Acquire), 0);
    assert_eq!(a.hub().unwrap().state.suppressed.load(Ordering::Relaxed), 1);
    drop(a);
    drop(b);
    let text = fs::read_to_string(temp.0.join("state/recorded.jsonl")).unwrap();
    assert!(text.contains("\"level\":\"off\""));
    assert!(!text.contains("private-digest"));
    assert!(!text.contains("\"kind\":\"turn\""));
}
#[test]
fn inbox_claim_replay_is_idempotent_and_bad_files_are_consumed_without_payload_logs() {
    let temp = Temp::new();
    let mut p = Desktop::default();
    EventPlugin::configure(&mut p, &temp.config()).unwrap();
    p.emit(events::CONTROL_ACTIVATE, "{}").unwrap();
    wait(|| p.hub().unwrap().state.activation.load(Ordering::Acquire) == 2);
    let payload = format!(
        r#"{{"kind":1,"event":{{"version":1,"source_id":"opaque","observation_id":"same","observed_at_ms":{},"fresh":true,"success":true,"windows":[{{"id":"week","used_percent":95,"span_seconds":604800}}]}}}}"#,
        hub::wall_ms()
    );
    private::atomic(&temp.0.join("inbox/one.json"), payload.as_bytes()).unwrap();
    private::atomic(&temp.0.join("inbox/two.json"), payload.as_bytes()).unwrap();
    private::atomic(&temp.0.join("inbox/bad.json"), b"PRIVATE_CANARY_NOT_JSON").unwrap();
    wait(|| p.hub().unwrap().state.delivered.load(Ordering::Relaxed) == 1);
    wait(|| p.hub().unwrap().state.invalid.load(Ordering::Relaxed) == 1);
    drop(p);
    let text = fs::read_to_string(temp.0.join("state/recorded.jsonl")).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("\"kind\":\"usage\""))
            .count(),
        1
    );
    assert!(!text.contains("PRIVATE_CANARY"));
    assert!(
        private::load_ledger(&temp.0.join("state"))
            .unwrap()
            .is_some()
    );
}
#[test]
fn private_state_is_exclusive_and_symlinks_do_not_get_claimed_as_payload() {
    let temp = Temp::new();
    let lock = private::lock(&temp.0.join("state")).unwrap();
    assert!(private::lock(&temp.0.join("state")).is_err());
    drop(lock);
    assert!(private::lock(&temp.0.join("state")).is_ok());
    private::atomic(&temp.0.join("state/value.json"), b"{}").unwrap();
    assert_eq!(
        private::read(&temp.0.join("state/value.json"), 10).unwrap(),
        b"{}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        assert_eq!(
            fs::metadata(temp.0.join("state/value.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        symlink(
            temp.0.join("state/value.json"),
            temp.0.join("state/link.json"),
        )
        .unwrap();
        assert!(private::read(&temp.0.join("state/link.json"), 10).is_err());
        fs::hard_link(
            temp.0.join("state/value.json"),
            temp.0.join("state/hard.json"),
        )
        .unwrap();
        assert!(private::read(&temp.0.join("state/value.json"), 10).is_err());
    }
}

#[test]
fn configuration_rejects_malformed_language_and_backend() {
    let temp = Temp::new();
    for field in ["language", "backend"] {
        for invalid in [
            json::Value::Bool(false),
            1u64.into(),
            "".into(),
            "x".repeat(256).into(),
            json::Value::Array(Vec::new()),
            json::parse(b"{}").unwrap(),
        ] {
            let mut value = json::parse(temp.config().as_bytes()).unwrap();
            value.set(field, invalid);
            assert!(
                config::Config::parse(&value.stringify()).is_err(),
                "{field}"
            );
        }
        let mut value = json::parse(temp.config().as_bytes()).unwrap();
        value.set(field, json::Value::Null);
        let parsed = config::Config::parse(&value.stringify()).unwrap();
        if field == "language" {
            assert_eq!(parsed.language, "en");
        } else {
            assert!(!parsed.record_only);
        }
    }
    for language in ["en", "zh", "zh-CN", "zh-TW", "ja", "de"] {
        let mut value = json::parse(temp.config().as_bytes()).unwrap();
        value.set("language", language.into());
        assert_eq!(
            config::Config::parse(&value.stringify()).unwrap().language,
            language
        );
    }
    assert!(!temp.0.join("state").exists());
    assert!(!temp.0.join("inbox").exists());
}
