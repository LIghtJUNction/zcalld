//! Explicitly bound, read-only native Codex metadata. Generation stays in the AI pipeline.
use super::mcp::{failure, immediate};
use super::{Community, Stream, fail, read};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use zcalld_compat_client::json::{Value, object};
use zplugin::{Error, OwnedResource, Result};

const PROVIDER: &str = "builtin:codex";
const MAX_ROUTES: usize = 4096;

pub(super) fn endpoint(path: &str) -> Option<&'static str> {
    match path {
        "/backend-api/codex/models" => Some("models"),
        "/backend-api/wham/usage" | "/api/codex/usage" => Some("usage"),
        "/backend-api/wham/accounts/check" | "/api/codex/accounts/check" => Some("accounts"),
        "/backend-api/wham/config/bundle" | "/api/codex/config/bundle" | "/codex/config/bundle" => {
            Some("config")
        }
        _ => None,
    }
}
fn name(value: &Value, max: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
}
fn binding_valid(value: &Value) -> bool {
    let Some(fields) = value.as_object() else {
        return false;
    };
    if fields.len() != 2 || name(value.get("provider"), 128).is_none() {
        return false;
    }
    let Value::Array(routes) = value.get("routes") else {
        return false;
    };
    let mut unique = BTreeSet::new();
    !routes.is_empty()
        && routes.len() <= MAX_ROUTES
        && routes
            .iter()
            .all(|route| name(route, 512).is_some_and(|name| unique.insert(name)))
}
pub(super) fn validate_registry(registry: &Value) -> Result<()> {
    let settings = registry.get("codex_http");
    if matches!(settings, Value::Null) {
        return Ok(());
    }
    let Some(fields) = settings.as_object() else {
        return Err(Error::InvalidArgument);
    };
    if fields
        .keys()
        .any(|k| !["enabled", "loopback", "callers"].contains(&k.as_str()))
        || settings.get("enabled").as_bool().is_none()
        || (!matches!(settings.get("loopback"), Value::Null)
            && !binding_valid(settings.get("loopback")))
    {
        return Err(Error::InvalidArgument);
    }
    if !matches!(settings.get("callers"), Value::Null) {
        let Some(callers) = settings.get("callers").as_object() else {
            return Err(Error::InvalidArgument);
        };
        if callers.len() > 1024
            || callers.iter().any(|(id, binding)| {
                id.is_empty()
                    || id.len() > 256
                    || id.chars().any(char::is_control)
                    || !binding_valid(binding)
            })
        {
            return Err(Error::InvalidArgument);
        }
    }
    Ok(())
}
pub(super) fn accepts(plugin: &Community, path: &str) -> Result<u32> {
    if endpoint(path).is_none() {
        return Ok(0);
    }
    let registry = read(
        plugin
            .config
            .get("registry")
            .as_str()
            .ok_or(Error::InvalidArgument)?,
    )?;
    validate_registry(&registry)?;
    Ok(
        if registry.get("codex_http").get("enabled").as_bool() == Some(true) {
            3
        } else {
            0
        },
    )
}

fn request_params(registry: &Value, meta: &Value) -> std::result::Result<Value, &'static str> {
    let settings = registry.get("codex_http");
    if settings.get("enabled").as_bool() != Some(true) {
        return Err("Codex metadata is disabled");
    }
    let caller_binding = meta
        .get("caller_key_id")
        .as_str()
        .map(|id| settings.get("callers").get(id))
        .filter(|v| !matches!(v, Value::Null));
    let binding = caller_binding
        .or_else(|| {
            (meta.get("loopback").as_bool() == Some(true)).then(|| settings.get("loopback"))
        })
        .ok_or("this caller has no Codex metadata binding")?;
    if !binding_valid(binding) {
        return Err("this caller has no valid Codex metadata binding");
    }
    let provider_id = binding.get("provider").as_str().expect("validated binding");
    let context = meta.get("routing");
    let provider = context
        .get("providers")
        .as_array()
        .iter()
        .find(|p| p.get("id").as_str() == Some(provider_id))
        .ok_or("bound provider is not in the gateway configuration")?;
    if provider.get("plugin").as_str() != Some(PROVIDER) {
        return Err("bound provider is not the builtin Codex provider");
    }
    // Only an explicit provider account or an explicitly saved account default.
    // Never fall back to whichever enabled account happens to sort first.
    let account = name(provider.get("account"), 256)
        .or_else(|| name(registry.get("account_defaults").get(PROVIDER), 256))
        .ok_or("bound provider needs an explicit Codex account")?;
    if account != PROVIDER
        && !account
            .strip_prefix(PROVIDER)
            .is_some_and(|s| s.starts_with('#') && s.len() > 1)
    {
        return Err("bound account belongs to a different provider");
    }
    if registry
        .get("accounts")
        .get(account)
        .get("enabled")
        .as_bool()
        != Some(true)
    {
        return Err("bound Codex account is unavailable or disabled");
    }
    let mut models = Vec::new();
    for route_id in binding.get("routes").as_array() {
        let route = context
            .get("routes")
            .as_array()
            .iter()
            .find(|r| r.get("id") == route_id)
            .ok_or("bound route is not in the gateway configuration")?;
        if route.get("dynamic").as_bool() != Some(false) {
            return Err("dynamic route groups cannot advertise one account's model capabilities");
        }
        if route.get("provider").as_str() != Some(provider_id) {
            return Err("bound route belongs to a different provider");
        }
        let model = name(route.get("model"), 512).ok_or("bound route has no upstream model")?;
        models.push(object([
            ("route", route_id.clone()),
            ("model", model.into()),
        ]));
    }
    Ok(object([
        ("provider", PROVIDER.into()),
        ("account", account.into()),
        ("models", Value::Array(models)),
    ]))
}

fn client_version<'a>(
    operation: &str,
    query: &'a str,
) -> std::result::Result<&'a str, &'static str> {
    if operation != "models" {
        return if query.is_empty() {
            Ok("")
        } else {
            Err("this Codex endpoint does not accept query parameters")
        };
    }
    let Some(version) = query.strip_prefix("client_version=") else {
        return Err("models requires exactly one client_version query parameter");
    };
    if version.is_empty()
        || version.len() > 64
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+-_".contains(&b))
    {
        return Err("invalid client_version query parameter");
    }
    Ok(version)
}

fn account_hint(meta: &Value) -> std::result::Result<Option<&str>, &'static str> {
    let values = meta.get("untrusted_headers").get("chatgpt-account-id");
    if matches!(values, Value::Null) {
        return Ok(None);
    }
    let Value::Array(values) = values else {
        return Err("invalid Codex account hint");
    };
    if values.is_empty() {
        return Ok(None);
    }
    if values.len() != 1 {
        return Err("duplicate Codex account hints are not accepted");
    }
    let value = name(&values[0], 256)
        .filter(|s| s.trim() == *s)
        .ok_or("empty or invalid Codex account hint")?;
    Ok(Some(value))
}

pub(super) fn handle(plugin: &mut Community, meta: &Value, body: &[u8]) -> Result<OwnedResource> {
    let started = Instant::now();
    let operation = endpoint(meta.get("path").as_str().ok_or(Error::InvalidArgument)?)
        .ok_or(Error::InvalidArgument)?;
    if meta.get("method").as_str() != Some("GET") {
        return immediate(
            405,
            object([
                ("allow", "GET".into()),
                ("content-type", "application/json".into()),
            ]),
            br#"{"error":"Codex metadata is read-only; use GET"}"#.to_vec(),
        );
    }
    if !body.is_empty() {
        return failure(400, "Codex metadata GET must not have a request body");
    }
    let version = match client_version(operation, meta.get("query").as_str().unwrap_or("")) {
        Ok(v) => v,
        Err(message) => return failure(400, message),
    };
    let registry = read(
        plugin
            .config
            .get("registry")
            .as_str()
            .ok_or(Error::InvalidArgument)?,
    )?;
    validate_registry(&registry)?;
    let mut params = match request_params(&registry, meta) {
        Ok(value) => value,
        Err(message) => return failure(403, message),
    };
    let timeout = meta
        .get("timeout_ms")
        .as_u64()
        .unwrap_or(30_000)
        .clamp(1, 30_000);
    match account_hint(meta) {
        Ok(Some(hint)) => params.set("account_hint", hint.into()),
        Ok(None) => {}
        Err(message) => return failure(400, message),
    }
    params.set("operation", operation.into());
    params.set("client_version", version.into());
    params.set("timeout_ms", timeout.into());
    params.set(
        "max_response_bytes",
        meta.get("max_response_bytes")
            .as_u64()
            .unwrap_or(4 << 20)
            .clamp(1, 4 << 20)
            .into(),
    );
    params.set("window", 524_288usize.into());
    let deadline = started + Duration::from_millis(timeout);
    let (host, initialization) = plugin.host_start()?;
    let call = host
        .start(object([("method", "codexHttp".into()), ("params", params)]))
        .map_err(fail)?;
    OwnedResource::new(Stream {
        call,
        initialization,
        head: None,
        bytes: Vec::new(),
        offset: 0,
        ended: false,
        deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcalld_compat_client::json;
    fn parse(s: &str) -> Value {
        json::parse(s.as_bytes()).unwrap()
    }
    fn registry() -> Value {
        parse(
            r#"{
        "codex_http":{"enabled":true,"loopback":{"provider":"local","routes":["alias"]},
          "callers":{"verified":{"provider":"other","routes":["other-route"]}}},
        "accounts":{"builtin:codex#a":{"enabled":true},"builtin:codex#b":{"enabled":true}}
    }"#,
        )
    }
    fn meta() -> Value {
        parse(
            r#"{
        "loopback":true,"caller_key_id":null,
        "routing":{"providers":[{"id":"local","plugin":"builtin:codex","account":"builtin:codex#a"},
          {"id":"other","plugin":"builtin:codex","account":"builtin:codex#b"}],
        "routes":[{"id":"alias","dynamic":false,"provider":"local","model":"actual-slug"},
          {"id":"other-route","dynamic":false,"provider":"other","model":"other-slug"}]}
    }"#,
        )
    }

    #[test]
    fn account_hint_is_optional_and_rejects_duplicates_empty_or_controls() {
        assert_eq!(account_hint(&meta()).unwrap(), None);
        for values in [
            r#"["a","a"]"#,
            r#"["a","b"]"#,
            r#"[""]"#,
            r#"["a\nb"]"#,
            r#"[" a "]"#,
        ] {
            let mut m = meta();
            m.set(
                "untrusted_headers",
                object([("chatgpt-account-id", parse(values))]),
            );
            assert!(account_hint(&m).is_err());
        }
        let mut m = meta();
        m.set(
            "untrusted_headers",
            object([("chatgpt-account-id", Value::Array(vec!["expected".into()]))]),
        );
        assert_eq!(account_hint(&m).unwrap(), Some("expected"));
    }
    #[test]
    fn metadata_endpoints_are_an_exact_allowlist() {
        assert_eq!(endpoint("/backend-api/codex/models"), Some("models"));
        assert_eq!(endpoint("/api/codex/usage"), Some("usage"));
        for path in [
            "/backend-api/codex/responses/compact",
            "/backend-api/codex/models/",
            "/api/codex/tasks",
            "https://evil.example/usage",
        ] {
            assert_eq!(endpoint(path), None);
        }
        assert!(client_version("models", "client_version=0.170.0").is_ok());
        for q in [
            "",
            "url=https://evil",
            "client_version=x&account=b",
            "client_version=%2F",
            "client_version=a&client_version=b",
        ] {
            assert!(client_version("models", q).is_err());
        }
        assert!(client_version("usage", "account=b").is_err());
    }
    #[test]
    fn metadata_has_no_first_enabled_account_fallback() {
        let mut m = meta();
        let providers = m.get("routing").get("providers").as_array().to_vec();
        let mut p = providers[0].clone();
        p.set("account", Value::Null);
        let mut ctx = m.get("routing").clone();
        ctx.set("providers", Value::Array(vec![p]));
        m.set("routing", ctx);
        assert!(request_params(&registry(), &m).is_err());
        let mut r = registry();
        r.set(
            "account_defaults",
            object([(PROVIDER, "builtin:codex#b".into())]),
        );
        assert_eq!(
            request_params(&r, &m).unwrap().get("account").as_str(),
            Some("builtin:codex#b")
        );
    }
    #[test]
    fn verified_caller_binding_is_required_for_lan() {
        let mut m = meta();
        m.set("loopback", false.into());
        m.set(
            "headers",
            object([
                ("x-loopback", "true".into()),
                ("x-caller-key-id", "verified".into()),
                ("chatgpt-account-id", "forged".into()),
            ]),
        );
        assert!(request_params(&registry(), &m).is_err());
        m.set("caller_key_id", "verified".into());
        let params = request_params(&registry(), &m).unwrap();
        assert_eq!(params.get("account").as_str(), Some("builtin:codex#b"));
        assert!(!params.stringify().contains("forged"));
    }
    #[test]
    fn groups_cross_provider_and_disabled_account_are_not_metadata_sources() {
        let base = meta();
        for field in ["dynamic", "provider"] {
            let mut m = base.clone();
            let mut route = m.get("routing").get("routes").as_array()[0].clone();
            route.set(
                field,
                if field == "dynamic" {
                    true.into()
                } else {
                    "other".into()
                },
            );
            let mut ctx = m.get("routing").clone();
            ctx.set("routes", Value::Array(vec![route]));
            m.set("routing", ctx);
            assert!(request_params(&registry(), &m).is_err());
        }
        let mut r = registry();
        r.set(
            "accounts",
            object([("builtin:codex#a", object([("enabled", false.into())]))]),
        );
        assert!(request_params(&r, &base).is_err());
    }
    #[test]
    fn bindings_reject_unknown_configuration_or_fabricated_model_maps() {
        validate_registry(&registry()).unwrap();
        for text in [
            r#"{"codex_http":{"enabled":true,"loopback":{"provider":"local","routes":["a"],"models":{"a":"made-up"}}}}"#,
            r#"{"codex_http":{"enabled":true,"loopback":{"provider":"local","routes":["a","a"]}}}"#,
            r#"{"codex_http":{"enabled":true,"url":"https://evil"}}"#,
        ] {
            assert!(validate_registry(&parse(text)).is_err());
        }
    }
}
