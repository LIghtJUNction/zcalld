use crate::{
    config::Config,
    event::{self, Event},
    logic::{self, Ledger, PendingMask, Turns},
    os::{self, Capability, Level},
    private,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zcalld_compat_client::json::{Value, object};

const MAX_QUEUE: usize = 128;
static HUBS: OnceLock<Mutex<BTreeMap<std::path::PathBuf, Weak<Hub>>>> = OnceLock::new();
pub struct State {
    pub active: AtomicU64,
    pub last_end: AtomicU64,
    pub had_activity: AtomicBool,
    pub turns: Mutex<Turns>,
    pub dropped: AtomicU64,
    pub invalid: AtomicU64,
    pub suppressed: AtomicU64,
    pub completed: AtomicU64,
    queue: Mutex<VecDeque<Event>>,
    wake: Condvar,
    stop: AtomicBool,
    started: Instant,
    pub(crate) activation: AtomicU32,
    notification: AtomicU32,
    inhibition: AtomicU32,
    display: AtomicBool,
    pub(crate) delivered: AtomicU64,
    error: AtomicU32,
}
impl State {
    fn new() -> Self {
        Self {
            active: 0.into(),
            last_end: 0.into(),
            had_activity: false.into(),
            turns: Mutex::new(Turns::default()),
            dropped: 0.into(),
            invalid: 0.into(),
            suppressed: 0.into(),
            completed: 0.into(),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            stop: false.into(),
            started: Instant::now(),
            activation: 0.into(),
            notification: (Capability::Unknown as u32).into(),
            inhibition: (Capability::Unknown as u32).into(),
            display: false.into(),
            delivered: 0.into(),
            error: 0.into(),
        }
    }
    pub fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}
pub struct Hub {
    pub config: Config,
    pub state: Arc<State>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
impl Hub {
    pub fn shared(config: Config) -> Result<Arc<Self>, String> {
        let mut all = HUBS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .map_err(|_| "desktop registry lock poisoned")?;
        all.retain(|_, v| v.strong_count() > 0);
        if let Some(h) = all.get(&config.state_dir).and_then(Weak::upgrade) {
            if h.config != config {
                return Err("desktop configuration conflicts with active instance".into());
            }
            return Ok(h);
        }
        if all.len() >= 8 {
            return Err("too many desktop instances".into());
        }
        let h = Arc::new(Self {
            config,
            state: Arc::new(State::new()),
            thread: Mutex::new(None),
        });
        all.insert(h.config.state_dir.clone(), Arc::downgrade(&h));
        Ok(h)
    }
    pub fn activate(&self) -> u32 {
        if !self.config.enabled {
            return 2;
        }
        if self
            .state
            .activation
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return 0;
        }
        let state = self.state.clone();
        let config = self.config.clone();
        match thread::Builder::new()
            .name("zcalld-desktop".into())
            .stack_size(512 * 1024)
            .spawn(move || run(config, state))
        {
            Ok(handle) => {
                if let Ok(mut h) = self.thread.lock() {
                    *h = Some(handle);
                }
            }
            Err(_) => {
                self.state.error.store(1, Ordering::Relaxed);
                self.state.activation.store(3, Ordering::Release);
            }
        }
        0
    }
    pub fn emit(&self, event: Event) -> u32 {
        if !self.config.enabled {
            return 2;
        }
        self.activate();
        let Ok(mut queue) = self.state.queue.try_lock() else {
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
            return 1;
        };
        if queue.len() >= MAX_QUEUE {
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        queue.push_back(event);
        drop(queue);
        self.state.wake();
        0
    }
    pub fn status(&self) -> Value {
        let s = &self.state;
        object([
            ("enabled", self.config.enabled.into()),
            ("active_requests", s.active.load(Ordering::Relaxed).into()),
            (
                "running",
                (s.activation.load(Ordering::Acquire) == 2).into(),
            ),
            (
                "notifications",
                Capability::name(s.notification.load(Ordering::Relaxed)).into(),
            ),
            (
                "inhibition",
                Capability::name(s.inhibition.load(Ordering::Relaxed)).into(),
            ),
            (
                "display_inhibition",
                s.display.load(Ordering::Relaxed).into(),
            ),
            (
                "notifications_accepted",
                s.delivered.load(Ordering::Relaxed).into(),
            ),
            ("dropped", s.dropped.load(Ordering::Relaxed).into()),
            ("invalid", s.invalid.load(Ordering::Relaxed).into()),
            ("suppressed", s.suppressed.load(Ordering::Relaxed).into()),
            (
                "error",
                match s.error.load(Ordering::Relaxed) {
                    0 => Value::Null,
                    1 => "worker_start_failed".into(),
                    2 => "private_state_unavailable".into(),
                    3 => "ledger_commit_failed".into(),
                    4 => "inbox_unavailable".into(),
                    _ => "desktop_unavailable".into(),
                },
            ),
        ])
    }
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.state.wake();
        if let Ok(mut handle) = self.thread.lock()
            && let Some(handle) = handle.take()
        {
            let _ = handle.join();
        }
    }
}
pub fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn run(config: Config, s: Arc<State>) {
    let locked = private::lock(&config.state_dir).and_then(|lock| {
        private::directory(&config.inbox)?;
        Ok(lock)
    });
    let Ok(_lock) = locked else {
        s.error.store(2, Ordering::Relaxed);
        s.activation.store(3, Ordering::Release);
        return;
    };
    let mut ledger = match private::load_ledger(&config.state_dir)
        .and_then(|v| v.as_ref().map(Ledger::parse).transpose())
    {
        Ok(v) => v.unwrap_or_default(),
        Err(_) => {
            s.error.store(2, Ordering::Relaxed);
            s.activation.store(3, Ordering::Release);
            return;
        }
    };
    let mut pending = PendingMask::default();
    let mut backend = os::create(&config);
    s.display
        .store(backend.display_supported(), Ordering::Relaxed);
    s.activation.store(2, Ordering::Release);
    let mut previous = Level::Off;
    let mut inbox_at = 0u64;
    let mut recorded_state = None;
    let mut inhibit_attempt = 0u64;
    while !s.stop.load(Ordering::Acquire) {
        let now = s.now();
        let wants = config.keep_awake
            && (s.active.load(Ordering::Acquire) > 0
                || s.had_activity.load(Ordering::Acquire)
                    && now.saturating_sub(s.last_end.load(Ordering::Acquire)) < logic::GRACE_MS);
        let level = if !wants {
            Level::Off
        } else if config.keep_display {
            Level::Display
        } else {
            Level::System
        };
        let capability = s.inhibition.load(Ordering::Relaxed);
        if level != previous
            || level != Level::Off
                && !matches!(capability, 1 | 6)
                && now.saturating_sub(inhibit_attempt) >= 30_000
        {
            s.inhibition
                .store(backend.inhibit(level) as u32, Ordering::Relaxed);
            previous = level;
            inhibit_attempt = now;
        }
        let mut batch = Vec::new();
        if let Ok(mut queue) = s.queue.lock() {
            for _ in 0..32 {
                if let Some(event) = queue.pop_front() {
                    batch.push((event, None))
                } else {
                    break;
                }
            }
        }
        if now >= inbox_at {
            inbox_at = now.saturating_add(2000);
            match private::inbox(&config.inbox) {
                Ok(entries) => {
                    for (path, bytes) in entries {
                        match bytes.and_then(|b| event::envelope(&b)) {
                            Ok(event) => batch.push((event, Some(path))),
                            Err(_) => {
                                s.invalid.fetch_add(1, Ordering::Relaxed);
                                let _ = std::fs::remove_file(path);
                            }
                        }
                    }
                }
                Err(_) => {
                    s.error.store(4, Ordering::Relaxed);
                }
            }
        }
        for (event, claimed) in batch {
            let mut next = ledger.clone();
            let mut next_pending = pending.clone();
            let notices =
                logic::process(&config, &mut next, &mut next_pending, event, wall_ms(), now);
            if private::atomic(
                &config.state_dir.join("ledger.json"),
                next.json().stringify().as_bytes(),
            )
            .is_err()
            {
                s.error.store(3, Ordering::Relaxed);
                continue;
            }
            ledger = next;
            pending = next_pending;
            // A crash before this removal replays the claimed event against the durable ledger.
            if let Some(path) = claimed
                && std::fs::remove_file(path).is_err()
            {
                s.invalid.fetch_add(1, Ordering::Relaxed);
            }
            for notice in notices {
                deliver(&mut *backend, &s, &notice);
            }
        }
        if let Some(notice) = pending.take_due(now, &config) {
            deliver(&mut *backend, &s, &notice);
        }
        let due = if let Ok(mut turns) = s.turns.try_lock() {
            turns.due(now, u64::from(config.turn_done_seconds) * 1000)
        } else {
            Vec::new()
        };
        for elapsed in due {
            deliver(
                &mut *backend,
                &s,
                &logic::notice(
                    &config,
                    "turn",
                    format!("An agent turn finished after {} seconds.", elapsed / 1000),
                ),
            );
        }
        if config.record_only {
            let snapshot = (
                s.active.load(Ordering::Acquire),
                s.completed.load(Ordering::Relaxed),
                s.suppressed.load(Ordering::Relaxed),
            );
            if recorded_state != Some(snapshot) {
                let row = object([
                    ("kind", "state".into()),
                    ("active_requests", snapshot.0.into()),
                    ("completed", snapshot.1.into()),
                    ("suppressed", snapshot.2.into()),
                ]);
                if private::record(&config.state_dir, &row).is_ok() {
                    recorded_state = Some(snapshot);
                }
            }
        }
        let delay = pending
            .due
            .map_or(2000, |t| t.saturating_sub(s.now()).min(2000))
            .max(1);
        if let Ok(queue) = s.queue.lock()
            && queue.is_empty()
            && !s.stop.load(Ordering::Acquire)
        {
            let _ = s.wake.wait_timeout(queue, Duration::from_millis(delay));
        }
    }
    // Closing must release the OS inhibitor, even when notification delivery failed.
    s.inhibition
        .store(backend.inhibit(Level::Off) as u32, Ordering::Relaxed);
    s.activation.store(0, Ordering::Release);
}
fn deliver(backend: &mut dyn os::Backend, s: &State, notice: &logic::Notice) {
    if s.stop.load(Ordering::Acquire) {
        return;
    }
    let result = backend.notify(notice);
    s.notification.store(result as u32, Ordering::Relaxed);
    if matches!(result, Capability::Available | Capability::RecordOnly) {
        s.delivered.fetch_add(1, Ordering::Relaxed);
    }
}
