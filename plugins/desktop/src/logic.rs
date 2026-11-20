use crate::{
    config::Config,
    event::{Event, Quota},
};
use std::collections::{BTreeMap, BTreeSet};
use zcalld_compat_client::json::{Value, object};

pub const QUIET_MS: u64 = 4_000;
pub const IDLE_MS: u64 = 30 * 60 * 1_000;
pub const GRACE_MS: u64 = 10 * 60 * 1_000;
const KEEP_MS: u64 = 40 * 24 * 60 * 60 * 1_000;
const MAX_MARKS: usize = 2048;
const MAX_SEEN: usize = 2048;
const MAX_SOURCES: usize = 512;

#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: &'static str,
    pub title: String,
    pub body: String,
}
impl Notice {
    pub fn json(&self) -> Value {
        object([
            ("kind", self.kind.into()),
            ("title", self.title.clone().into()),
            ("body", self.body.clone().into()),
        ])
    }
}
#[derive(Clone, Debug)]
struct Mark {
    at: u64,
    until: Option<u64>,
}
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    marks: BTreeMap<String, Mark>,
    seen: BTreeMap<String, u64>,
    sources: BTreeMap<String, u64>,
}
impl Ledger {
    pub fn parse(v: &Value) -> Result<Self, String> {
        if v.get("version").as_u64() != Some(1) {
            return Err("unsupported desktop ledger".into());
        }
        let mut l = Self::default();
        let marks = v.get("marks").as_object().ok_or("invalid desktop ledger")?;
        let seen = v.get("seen").as_object().ok_or("invalid desktop ledger")?;
        let sources = v
            .get("sources")
            .as_object()
            .ok_or("invalid desktop ledger")?;
        if marks.len() > MAX_MARKS || seen.len() > MAX_SEEN || sources.len() > MAX_SOURCES {
            return Err("desktop ledger exceeds limit".into());
        }
        for (key, m) in marks {
            if key.len() > 512 {
                return Err("desktop ledger key exceeds limit".into());
            }
            l.marks.insert(
                key.clone(),
                Mark {
                    at: m.get("at").as_u64().ok_or("invalid desktop mark")?,
                    until: match m.get("until") {
                        Value::Null => None,
                        n => Some(n.as_u64().ok_or("invalid desktop mark")?),
                    },
                },
            );
        }
        for (key, at) in seen {
            if key.len() > 256 {
                return Err("desktop ledger key exceeds limit".into());
            }
            l.seen
                .insert(key.clone(), at.as_u64().ok_or("invalid desktop seen mark")?);
        }
        for (key, at) in sources {
            if key.len() > 128 {
                return Err("desktop source key exceeds limit".into());
            }
            l.sources.insert(
                key.clone(),
                at.as_u64().ok_or("invalid desktop source mark")?,
            );
        }
        Ok(l)
    }
    pub fn json(&self) -> Value {
        object([
            ("version", 1u64.into()),
            (
                "marks",
                Value::Object(
                    self.marks
                        .iter()
                        .map(|(k, m)| {
                            (
                                k.clone(),
                                object([
                                    ("at", m.at.into()),
                                    ("until", m.until.map_or(Value::Null, Into::into)),
                                ]),
                            )
                        })
                        .collect(),
                ),
            ),
            (
                "seen",
                Value::Object(
                    self.seen
                        .iter()
                        .map(|(k, v)| (k.clone(), (*v).into()))
                        .collect(),
                ),
            ),
            (
                "sources",
                Value::Object(
                    self.sources
                        .iter()
                        .map(|(k, v)| (k.clone(), (*v).into()))
                        .collect(),
                ),
            ),
        ])
    }
    fn remember(&mut self, key: String, now: u64) -> bool {
        if self.seen.contains_key(&key) {
            return false;
        }
        self.seen.retain(|_, at| now.saturating_sub(*at) <= KEEP_MS);
        if self.seen.len() >= MAX_SEEN {
            return false;
        }
        self.seen.insert(key, now);
        true
    }
    fn mark(&mut self, key: String, until: Option<u64>, now: u64) -> bool {
        if self
            .marks
            .get(&key)
            .is_some_and(|m| same_run(m.until, until))
        {
            return false;
        }
        if !self.marks.contains_key(&key) && self.marks.len() >= MAX_MARKS {
            return false;
        }
        self.marks.insert(key, Mark { at: now, until });
        true
    }
}
fn same_run(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) < 600_000,
        (None, None) => true,
        _ => false,
    }
}

#[derive(Clone, Default)]
pub struct PendingMask {
    pub counts: BTreeMap<String, u64>,
    pub agents: BTreeSet<String>,
    pub due: Option<u64>,
    pub last: Option<u64>,
}
impl PendingMask {
    pub fn take_due(&mut self, now: u64, config: &Config) -> Option<Notice> {
        if self.due.is_none_or(|at| at > now) {
            return None;
        }
        self.due = None;
        if self.counts.is_empty() || !config.mask_alert {
            self.counts.clear();
            self.agents.clear();
            return None;
        }
        let n: u64 = self.counts.values().copied().sum();
        // Only approved kind labels are rendered; arbitrary identifiers never reach the desktop.
        let parts = self
            .counts
            .iter()
            .map(|(k, n)| format!("{}: {}", kind_label(k), n))
            .collect::<Vec<_>>()
            .join(", ");
        self.counts.clear();
        self.agents.clear();
        self.last = Some(now);
        Some(notice(
            config,
            "mask",
            format!("{n} new sensitive values were masked. {parts}"),
        ))
    }
}
pub fn process(
    config: &Config,
    ledger: &mut Ledger,
    pending: &mut PendingMask,
    event: Event,
    wall: u64,
    mono: u64,
) -> Vec<Notice> {
    match event {
        Event::Quota(q) => quota(config, ledger, q, wall),
        Event::Mask(m) => {
            if config.mask_alert
                && !m.counts.is_empty()
                && ledger.remember(format!("m:{}", m.id), wall)
            {
                for (k, n) in m.counts {
                    if pending.counts.len() < 32 || pending.counts.contains_key(&k) {
                        let v = pending.counts.entry(k).or_default();
                        *v = v.saturating_add(n).min(1_000_000_000);
                    }
                }
                if pending.agents.len() < 16 {
                    pending.agents.insert(m.agent);
                }
                if pending.due.is_none() {
                    pending.due = Some(
                        mono.saturating_add(3_000)
                            .max(pending.last.map_or(0, |n| n.saturating_add(60_000))),
                    );
                }
            }
            Vec::new()
        }
    }
}
fn quota(config: &Config, l: &mut Ledger, q: Quota, now: u64) -> Vec<Notice> {
    let mut out = Vec::new();
    // Failed/stale observations must not rearm a previously sent warning.
    if !q.fresh
        || !q.success
        || q.observed > now.saturating_add(300_000)
        || now.saturating_sub(q.observed) > 15 * 60_000
        || l.sources
            .get(&q.source)
            .is_some_and(|old| *old > q.observed)
    {
        return out;
    }
    if !l.sources.contains_key(&q.source) && l.sources.len() >= MAX_SOURCES {
        return out;
    }
    if !l.remember(format!("q:{}:{}", q.source, q.observation), now) {
        return out;
    }
    l.sources.insert(q.source.clone(), q.observed);
    l.marks.retain(|_, m| now.saturating_sub(m.at) <= KEEP_MS);
    let mut pools = BTreeSet::new();
    for w in q.windows {
        if w.aside && (w.pool.is_empty() || w.span != 7 * 24 * 3600) {
            continue;
        }
        let id = if w.pool.is_empty() {
            w.id.clone()
        } else {
            format!("pool:{}:{}", w.pool, w.id)
        };
        if !pools.insert(id.clone()) {
            continue;
        }
        let key = format!("{}:w:{id}", q.source);
        if config.usage_alert == 0 || w.used < f64::from(config.usage_alert) {
            l.marks.remove(&key);
        } else if l.mark(key, w.resets, now) {
            out.push(notice(
                config,
                "usage",
                format!("A subscription window is {:.1}% used.", w.used),
            ));
        }
        if config.reset_reminder_hours > 0
            && w.span >= 86400
            && w.used < 85.0
            && let Some(reset) = w.resets
            && reset > now
            && reset - now <= u64::from(config.reset_reminder_hours) * 3_600_000
            && l.mark(format!("{}:r:{id}", q.source), Some(reset), now)
        {
            out.push(notice(
                config,
                "reset",
                format!(
                    "A subscription window resets in {} minutes with {:.1}% unused.",
                    (reset - now).div_ceil(60_000),
                    100.0 - w.used
                ),
            ));
        }
    }
    if let Some((amount, unit)) = q.balance {
        let key = format!("{}:balance", q.source);
        if config.balance_alert <= 0.0 || amount > config.balance_alert {
            l.marks.remove(&key);
        } else if l.mark(key, None, now) {
            out.push(notice(
                config,
                "balance",
                format!("Available balance is {amount:.2} {unit}."),
            ));
        }
    }
    if let Some((count, expires)) = q.reset_credits
        && config.reset_reminder_hours > 0
        && expires > now
        && expires - now <= u64::from(config.reset_reminder_hours) * 3_600_000
        && l.mark(format!("{}:credits", q.source), Some(expires), now)
    {
        out.push(notice(
            config,
            "reset",
            format!(
                "{count} available reset credits expire in {} minutes.",
                (expires - now).div_ceil(60_000)
            ),
        ));
    }
    out
}
pub fn notice(config: &Config, kind: &'static str, body: String) -> Notice {
    let title = match (config.language.as_str(), kind) {
        ("zh" | "zh-CN", "mask") => "已保护敏感信息",
        ("zh-TW", "mask") => "已保護敏感資訊",
        ("zh" | "zh-CN" | "zh-TW", "turn") => "任务已完成",
        ("zh" | "zh-CN" | "zh-TW", "usage") => "额度用量提醒",
        ("zh" | "zh-CN" | "zh-TW", "balance") => "余额提醒",
        ("zh" | "zh-CN" | "zh-TW", "reset") => "额度重置提醒",
        (_, "mask") => "Sensitive values protected",
        (_, "turn") => "Agent turn finished",
        (_, "usage") => "Usage warning",
        (_, "balance") => "Balance warning",
        (_, "reset") => "Quota reset reminder",
        _ => "zcalld",
    };
    Notice {
        kind,
        title: title.into(),
        body,
    }
}
fn kind_label(kind: &str) -> &str {
    match kind {
        "API_KEY" => "API keys",
        "TOKEN" => "tokens",
        "PASSWORD" => "passwords",
        "PRIVATE_KEY" => "private keys",
        "EMAIL" => "email addresses",
        "PHONE" => "phone numbers",
        "PERSON" => "names",
        "ADDRESS" => "addresses",
        "BANK_CARD" => "bank cards",
        "ID_CARD" | "SSN" | "PASSPORT" => "identity numbers",
        _ => "sensitive values",
    }
}

#[derive(Clone, Debug)]
struct Turn {
    first: u64,
    last: u64,
    active: u32,
    due: Option<u64>,
}
#[derive(Default)]
pub struct Turns {
    values: BTreeMap<String, Turn>,
}
impl Turns {
    pub fn begin(&mut self, key: String, now: u64) -> Option<String> {
        self.values
            .retain(|_, t| t.active > 0 || now.saturating_sub(t.last) < IDLE_MS);
        if !self.values.contains_key(&key) && self.values.len() >= 1024 {
            return None;
        }
        let t = self.values.entry(key.clone()).or_insert(Turn {
            first: now,
            last: now,
            active: 0,
            due: None,
        });
        t.active = t.active.saturating_add(1);
        t.last = now;
        t.due = None;
        Some(key)
    }
    pub fn end(&mut self, key: &str, now: u64, final_reply: bool) {
        if let Some(t) = self.values.get_mut(key) {
            t.active = t.active.saturating_sub(1);
            t.last = now;
            t.due = if final_reply && t.active == 0 {
                Some(now.saturating_add(QUIET_MS))
            } else {
                None
            };
        }
    }
    pub fn due(&mut self, now: u64, threshold_ms: u64) -> Vec<u64> {
        let keys = self
            .values
            .iter()
            .filter(|(_, t)| t.active == 0 && t.due.is_some_and(|v| v <= now))
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        let mut result = Vec::new();
        for key in keys {
            if let Some(t) = self.values.remove(&key) {
                let elapsed = t.last.saturating_sub(t.first);
                if threshold_ms > 0 && elapsed >= threshold_ms {
                    result.push(elapsed);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event;
    pub(super) fn config() -> Config {
        Config {
            enabled: true,
            keep_awake: true,
            keep_display: false,
            usage_alert: 90,
            balance_alert: 5.0,
            reset_reminder_hours: 2,
            turn_done_seconds: 1,
            mask_alert: true,
            language: "en".into(),
            state_dir: "/private/state".into(),
            inbox: "/private/inbox".into(),
            record_only: true,
            notification_helper: None,
        }
    }
    fn quota(id: &str, observed: u64, used: f64, reset: u64) -> Event {
        event::parse(1,format!(r#"{{"version":1,"source_id":"opaque-account","observation_id":"{id}","observed_at_ms":{observed},"fresh":true,"success":true,"windows":[{{"id":"week","used_percent":{used},"span_seconds":604800,"reset_at_ms":{reset}}}]}}"#).as_bytes()).unwrap()
    }
    #[test]
    fn fresh_readings_rearm_but_failure_stale_and_duplicates_do_not() {
        let c = config();
        let mut l = Ledger::default();
        let mut p = PendingMask::default();
        let now = 1_000_000;
        let n = process(
            &c,
            &mut l,
            &mut p,
            quota("one", now, 91.0, now + 10_000_000),
            now,
            0,
        );
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, "usage");
        assert!(
            process(
                &c,
                &mut l,
                &mut p,
                quota("two", now + 1, 95.0, now + 10_001_000),
                now + 1,
                0
            )
            .is_empty()
        );
        let mut failed = match quota("failed", now + 2, 0.0, now + 10_000_000) {
            Event::Quota(q) => q,
            _ => unreachable!(),
        };
        failed.success = false;
        assert!(process(&c, &mut l, &mut p, Event::Quota(failed), now + 2, 0).is_empty());
        assert!(
            process(
                &c,
                &mut l,
                &mut p,
                quota("three", now + 3, 92.0, now + 10_000_000),
                now + 3,
                0
            )
            .is_empty()
        );
        assert!(
            process(
                &c,
                &mut l,
                &mut p,
                quota("four", now + 4, 20.0, now + 10_000_000),
                now + 4,
                0
            )
            .is_empty()
        );
        assert_eq!(
            process(
                &c,
                &mut l,
                &mut p,
                quota("five", now + 5, 92.0, now + 10_000_000),
                now + 5,
                0
            )
            .len(),
            1
        );
        let persisted = l.json();
        let mut reloaded = Ledger::parse(&persisted).unwrap();
        assert!(
            process(
                &c,
                &mut reloaded,
                &mut p,
                quota("five", now + 5, 92.0, now + 10_000_000),
                now + 5,
                0
            )
            .is_empty()
        );
    }
    #[test]
    fn pooled_identity_survives_missing_span_and_keeps_distinct_windows() {
        let c = config();
        let mut ledger = Ledger::default();
        let mut pending = PendingMask::default();
        let now = 1_000_000;
        let reading = |observation: &str, at: u64, id: &str, span: u64| {
            let Event::Quota(mut q) = quota(observation, at, 95.0, now + 10_000_000) else {
                unreachable!()
            };
            q.windows[0].id = id.into();
            q.windows[0].pool = "account".into();
            q.windows[0].span = span;
            Event::Quota(q)
        };
        assert_eq!(
            process(
                &c,
                &mut ledger,
                &mut pending,
                reading("cache", now, "primary", 0),
                now,
                0
            )
            .len(),
            1
        );
        // A later producer knows the period; it is still the same quota window.
        assert!(
            process(
                &c,
                &mut ledger,
                &mut pending,
                reading("headers", now + 1, "primary", 604800),
                now + 1,
                0
            )
            .is_empty()
        );
        assert_eq!(
            process(
                &c,
                &mut ledger,
                &mut pending,
                reading("second", now + 2, "secondary", 604800),
                now + 2,
                0
            )
            .len(),
            1
        );
        let mut reloaded = Ledger::parse(&ledger.json()).unwrap();
        assert!(
            process(
                &c,
                &mut reloaded,
                &mut pending,
                reading("refresh", now + 3, "secondary", 0),
                now + 3,
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn expiry_is_distinct_from_reset_and_balance_keeps_its_unit() {
        let c = config();
        let mut l = Ledger::default();
        let mut p = PendingMask::default();
        let now = 1_000_000;
        let e=event::parse(1,format!(r#"{{"version":1,"source_id":"opaque","observation_id":"first","observed_at_ms":{now},"fresh":true,"success":true,"windows":[{{"id":"day","used_percent":10,"span_seconds":86400,"reset_at_ms":{}}}],"balance":{{"amount":2.5,"unit":"CNY"}},"reset_credits":{{"count":3,"expires_at_ms":{}}}}}"#,now+3_600_000,now+3_600_000).as_bytes()).unwrap();
        let n = process(&c, &mut l, &mut p, e, now, 0);
        assert_eq!(n.iter().filter(|n| n.kind == "reset").count(), 2);
        assert!(
            n.iter()
                .any(|n| n.kind == "balance" && n.body.contains("CNY"))
        );
        assert!(n.iter().any(|n| n.body.contains("credits expire")));
    }
    #[test]
    fn short_or_mostly_used_windows_and_nonweekly_aside_do_not_remind() {
        let c = config();
        let mut l = Ledger::default();
        let mut p = PendingMask::default();
        let now = 1_000_000;
        let e=event::parse(1,format!(r#"{{"version":1,"source_id":"opaque","observation_id":"first","observed_at_ms":{now},"fresh":true,"success":true,"windows":[{{"id":"short","used_percent":10,"span_seconds":18000,"reset_at_ms":{0}}},{{"id":"used","used_percent":85,"span_seconds":86400,"reset_at_ms":{0}}},{{"id":"aside","pool":"p","aside":true,"used_percent":95,"span_seconds":86400,"reset_at_ms":{0}}}]}}"#,now+3_600_000).as_bytes()).unwrap();
        assert!(process(&c, &mut l, &mut p, e, now, 0).is_empty());
    }
    #[test]
    fn masks_are_first_value_counts_batched_and_rate_limited() {
        let c = config();
        let mut l = Ledger::default();
        let mut p = PendingMask::default();
        let input=br#"{"version":1,"event_id":"record-1","agent":"codex","counts":[{"kind":"TOKEN","new_values":2}]}"#;
        process(
            &c,
            &mut l,
            &mut p,
            event::parse(2, input).unwrap(),
            10_000,
            0,
        );
        process(
            &c,
            &mut l,
            &mut p,
            event::parse(2, input).unwrap(),
            10_001,
            1,
        );
        assert!(p.take_due(2999, &c).is_none());
        let n = p.take_due(3000, &c).unwrap();
        assert!(n.body.starts_with("2 new"));
        process(&c,&mut l,&mut p,event::parse(2,br#"{"version":1,"event_id":"record-2","agent":"codex","counts":[{"kind":"EMAIL","new_values":1}]}"#).unwrap(),11_000,4000);
        assert!(p.take_due(62_999, &c).is_none());
        assert!(p.take_due(63_000, &c).is_some());
        let mut reloaded = Ledger::parse(&l.json()).unwrap();
        let mut empty = PendingMask::default();
        process(
            &c,
            &mut reloaded,
            &mut empty,
            event::parse(2, input).unwrap(),
            12_000,
            70_000,
        );
        assert!(empty.due.is_none());
    }
    #[test]
    fn turn_requires_quiet_all_requests_and_duration() {
        let mut t = Turns::default();
        let k = t.begin("session".into(), 0).unwrap();
        t.begin(k.clone(), 100);
        t.end(&k, 1500, true);
        assert!(t.due(6000, 1000).is_empty());
        t.end(&k, 1600, true);
        assert!(t.due(5599, 1000).is_empty());
        assert_eq!(t.due(5600, 1000), vec![1600]);
        let k = t.begin("other".into(), 7000).unwrap();
        t.end(&k, 7200, true);
        assert!(t.due(12_000, 1000).is_empty());
        let k = t.begin("failed".into(), 13_000).unwrap();
        t.end(&k, 16_000, false);
        assert!(t.due(25_000, 1000).is_empty());
    }
}
