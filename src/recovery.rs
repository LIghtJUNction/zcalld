//! Optional provider recovery after a complete refusal and exhausted alternatives.
//! The core carries only configured identities; the plugin owns provider policy.
use crate::{config::Provider, http, plugins::Plugins, system::STOPPING, transport::Stream};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use zcalld_plugin_api::recovery;

#[derive(Clone)]
pub(crate) struct Candidate {
    pub route: String,
    provider: String,
    account: String,
    model: String,
    status: u16,
}
impl Candidate {
    pub fn capture(route: &str, provider: &Provider, model: &str, status: u16) -> Option<Self> {
        if !matches!(status, 429 | 503) {
            return None;
        }
        let plugin = provider.plugin.as_deref()?;
        let account = provider.plugin_account.as_deref()?;
        if [route, plugin, account, model]
            .iter()
            .any(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
        {
            return None;
        }
        Some(Self {
            route: route.into(),
            provider: plugin.into(),
            account: account.into(),
            model: model.into(),
            status,
        })
    }
}

fn metadata(candidates: &[Candidate], timeout_ms: u64) -> Result<String, String> {
    if candidates.is_empty() || candidates.len() > recovery::MAX_ATTEMPTS || timeout_ms == 0 {
        return Err("invalid provider recovery bounds".into());
    }
    let attempts = candidates
        .iter()
        .map(|c| {
            format!(
                "{{\"route\":{},\"provider\":{},\"account\":{},\"model\":{},\"status\":{}}}",
                http::quote(&c.route),
                http::quote(&c.provider),
                http::quote(&c.account),
                http::quote(&c.model),
                c.status,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let message = format!(
        "{{\"phase\":\"routes_exhausted\",\"timeout_ms\":{},\"attempts\":[{attempts}]}}",
        timeout_ms.min(60_000),
    );
    if message.len() > recovery::MAX_METADATA_BYTES {
        return Err("provider recovery metadata exceeds limit".into());
    }
    Ok(message)
}

/// Waiting remains part of the original HTTP request. No retry is authorized by
/// an unknown result, transport error, cancelled peer, or expired deadline.
pub(crate) fn attempt(
    socket: &Stream,
    plugins: &mut Plugins,
    candidates: &[Candidate],
    deadline: Instant,
) -> Result<Option<usize>, String> {
    if !plugins.has_recovery() || candidates.is_empty() {
        return Ok(None);
    }
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(60_000) as u64;
    if remaining == 0 || socket.is_disconnected() || STOPPING.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let request = metadata(candidates, remaining)?;
    let Some(mut response) = plugins.begin_recovery(request.as_bytes())? else {
        return Ok(None);
    };
    let recovery_deadline = deadline.min(Instant::now() + Duration::from_millis(remaining));
    loop {
        if socket.is_disconnected()
            || STOPPING.load(Ordering::Relaxed)
            || Instant::now() >= recovery_deadline
        {
            return Ok(None);
        }
        if let Some(result) = response.poll()? {
            if result.is_some_and(|index| index >= candidates.len()) {
                return Err("recovery selected an unattempted candidate".into());
            }
            return Ok(result);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, state_json};

    fn provider(account: bool) -> Provider {
        let text = format!(
            "[provider fixture]\nbase_url=https://example.invalid/v1\nplugin=builtin:codex\n{}[route route]\nprovider=fixture\nmodel=actual\n",
            if account {
                "plugin_account=builtin:codex#12345678901234567890\n"
            } else {
                ""
            },
        );
        Config::parse(&text, std::path::Path::new("."))
            .unwrap()
            .providers
            .remove(0)
    }

    #[test]
    fn recovery_requires_an_explicit_configured_account_and_complete_refusal() {
        let explicit = provider(true);
        assert!(Candidate::capture("route", &explicit, "actual", 429).is_some());
        assert!(Candidate::capture("route", &explicit, "actual", 503).is_some());
        assert!(Candidate::capture("route", &provider(false), "actual", 429).is_none());
        for status in [200, 400, 401, 403, 500, 502] {
            assert!(Candidate::capture("route", &explicit, "actual", status).is_none());
        }
    }

    #[test]
    fn recovery_metadata_contains_only_bounded_configured_identities() {
        let candidate = Candidate::capture("route", &provider(true), "actual", 429).unwrap();
        let text = metadata(std::slice::from_ref(&candidate), u64::MAX).unwrap();
        let value = state_json::parse(text.as_bytes()).unwrap();
        assert_eq!(value.get("timeout_ms").as_u64(), Some(60_000));
        assert_eq!(value.get("phase").as_str(), Some("routes_exhausted"));
        let attempt = &value.get("attempts").as_array()[0];
        assert_eq!(attempt.get("route").as_str(), Some("route"));
        assert_eq!(attempt.as_object().unwrap().len(), 5);
        for field in ["body", "headers", "url", "token"] {
            assert!(matches!(attempt.get(field), state_json::Value::Null));
        }
        assert!(metadata(&[], 1).is_err());
        assert!(metadata(std::slice::from_ref(&candidate), 0).is_err());
        assert!(metadata(&vec![candidate; recovery::MAX_ATTEMPTS + 1], 1).is_err());
    }
}
