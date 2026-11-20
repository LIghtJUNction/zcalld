//! An explicit native-Claude path through the existing provider resource ABI.
//! The optional Go worker owns native-profile validation, PTY sessions and
//! tool capabilities. This bridge neither chooses an account nor starts Bun.
use super::{Community, Stream};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use zcalld_compat_client::{
    Host, encode, helpers,
    json::{Value, object},
};
use zplugin::{Error, OwnedResource, Result};

pub(super) fn native_only(config: &Value) -> bool {
    config.get("claude_only").as_bool() == Some(true)
}

pub(super) fn validate_configuration(config: &Value) -> Result<bool> {
    match config.get("claude_only") {
        Value::Null => return Ok(false),
        value if value.as_bool() == Some(false) => return Ok(false),
        value if value.as_bool() == Some(true) => {}
        _ => return Err(Error::InvalidArgument),
    }
    let keys = config.as_object().ok_or(Error::InvalidArgument)?;
    if keys
        .keys()
        .any(|k| !matches!(k.as_str(), "claude_only" | "root" | "pluginctl"))
    {
        return Err(Error::InvalidArgument);
    }
    binding(config)?;
    Ok(true)
}

fn binding(config: &Value) -> Result<(PathBuf, PathBuf)> {
    let root = if native_only(config) {
        PathBuf::from(config.get("root").as_str().ok_or(Error::InvalidArgument)?)
    } else {
        Path::new(
            config
                .get("registry")
                .as_str()
                .ok_or(Error::InvalidArgument)?,
        )
        .parent()
        .ok_or(Error::InvalidArgument)?
        .to_path_buf()
    };
    let helper = helpers::resolve_legacy(Path::new(
        config
            .get("pluginctl")
            .as_str()
            .ok_or(Error::InvalidArgument)?,
    ));
    if !root.is_absolute() || !root.is_dir() || !helpers::executable(&helper) {
        return Err(Error::InvalidArgument);
    }
    Ok((root, helper))
}

pub(super) fn fetch(
    community: &mut Community,
    mut params: Value,
    started: Instant,
) -> Result<OwnedResource> {
    if params.get("provider").as_str() != Some("builtin:claude")
        || params.get("method").as_str() != Some("POST")
        || !params.get("account").as_str().is_some_and(|account| {
            account.strip_prefix("builtin:claude#").is_some_and(|id| {
                id.len() == 20
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
    {
        return Err(Error::InvalidArgument);
    }
    let (root, helper) = binding(&community.config)?;
    let timeout = params
        .get("timeout_ms")
        .as_u64()
        .unwrap_or(300_000)
        .clamp(1, 300_000);
    let deadline = started + Duration::from_millis(timeout);
    let body = params.get("body").as_str().ok_or(Error::InvalidArgument)?;
    if body.len() > 8 * 1024 * 1024 {
        return Err(Error::InvalidArgument);
    }
    params.set("body", encode(body.as_bytes()).into());
    params.set("window", 524_288usize.into());
    params.set("timeout_ms", timeout.into());
    let host = if let Some(host) = &community.claude_host {
        host.clone()
    } else {
        // Encoding the two strings prevents path delimiter collisions. Host's
        // weak registry is shared only for this exact trusted configuration.
        let key = format!(
            "native-claude:{}",
            object([
                ("root", root.to_string_lossy().into_owned().into()),
                ("helper", helper.to_string_lossy().into_owned().into()),
            ])
            .stringify()
        );
        let args = vec![
            "--root".into(),
            root.to_string_lossy().into_owned(),
            "claude-provider".into(),
            "worker".into(),
        ];
        let (host, _) = Host::shared(&key, &helper, &args, Some(&root)).map_err(|_| {
            Error::PluginStatus(zcalld_plugin_api::status::PROVIDER_TRANSPORT_FAILED)
        })?;
        community.claude_host = Some(host.clone());
        host
    };
    let call = host
        .start(object([("method", "fetch".into()), ("params", params)]))
        .map_err(|_| Error::PluginStatus(zcalld_plugin_api::status::PROVIDER_TRANSPORT_FAILED))?;
    OwnedResource::new(Stream {
        call,
        initialization: None,
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
    use zcalld_plugin_api::{GatewayPlugin, HttpPlugin, ProviderPlugin, RecoveryPlugin};

    fn config() -> Value {
        let exe = std::env::current_exe().unwrap();
        object([
            ("claude_only", true.into()),
            (
                "root",
                exe.parent().unwrap().to_string_lossy().into_owned().into(),
            ),
            ("pluginctl", exe.to_string_lossy().into_owned().into()),
        ])
    }

    #[test]
    fn native_only_is_explicit_and_has_no_community_http_or_gateway_hooks() {
        let mut community = Community::default();
        HttpPlugin::configure(&mut community, &config().stringify()).unwrap();
        assert_eq!(
            HttpPlugin::accepts(&mut community, "/mcp/magpie/tools").unwrap(),
            0
        );
        assert_eq!(
            HttpPlugin::accepts(&mut community, "/backend-api/codex/models").unwrap(),
            0
        );
        assert_eq!(RecoveryPlugin::accepts(&mut community).unwrap(), 0);
        assert_eq!(
            GatewayPlugin::request(&mut community, 3, b"{}", &mut []).unwrap(),
            0
        );
        assert!(community.host.is_none() && community.claude_host.is_none());
    }

    #[test]
    fn ordinary_configuration_still_requires_bun_host_pi_registry() {
        let mut config = config();
        config.remove("claude_only");
        assert!(
            Community::default()
                .configure_host(&config.stringify())
                .is_err()
        );
        config.set("claude_only", "true".into());
        assert!(
            Community::default()
                .configure_host(&config.stringify())
                .is_err()
        );
        config.set("claude_only", true.into());
        config.set("bun", "ignored-path".into());
        assert!(
            Community::default()
                .configure_host(&config.stringify())
                .is_err()
        );
    }

    #[test]
    fn native_only_rejects_other_providers_and_missing_accounts_without_spawn() {
        let mut community = Community::default();
        community.configure_host(&config().stringify()).unwrap();
        for provider in ["builtin:codex", "claude", "builtin:claude"] {
            let request = object([
                ("provider", provider.into()),
                ("method", "POST".into()),
                ("body", "{}".into()),
            ]);
            assert!(ProviderPlugin::fetch(&mut community, request.stringify().as_bytes()).is_err());
            assert!(community.host.is_none() && community.claude_host.is_none());
        }
    }
}
