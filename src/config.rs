//! Small, strict configuration reader with separate inbound and upstream credentials.
mod caller_keys;
use crate::{
    plugins::PluginSpec,
    protocol::{self, Protocol},
};
pub use caller_keys::CallerKeys;
use std::{
    collections::BTreeMap,
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub protocol: Protocol,
    pub api_key_env: Option<String>,
    pub headers: Vec<(String, String)>,
    pub plugin: Option<String>,
    pub plugin_account: Option<String>,
    pub plugin_npm: Option<String>,
    pub native_endpoints: Vec<(Protocol, String)>,
    pub key: Option<String>,
    pub auth: Auth,
    auth_explicit: bool,
}

impl Provider {
    /// Select only explicitly configured native capabilities. Never probe/replay a generation.
    pub fn for_protocol(&self, source: Protocol) -> (Protocol, &str, Auth) {
        let (protocol, url) = self
            .native_endpoints
            .iter()
            .find(|(protocol, _)| *protocol == source)
            .map(|(_, url)| (source, url.as_str()))
            .unwrap_or((self.protocol, &self.base_url));
        let auth = if self.auth_explicit {
            self.auth
        } else {
            default_auth(protocol)
        };
        (protocol, url, auth)
    }
}

#[derive(Clone, Copy)]
pub enum Auth {
    Bearer,
    ApiKey,
    GoogleKey,
    None,
}

fn default_auth(protocol: Protocol) -> Auth {
    match protocol {
        Protocol::AnthropicMessages => Auth::ApiKey,
        Protocol::GeminiGenerateContent | Protocol::GeminiInteractions => Auth::GoogleKey,
        _ => Auth::Bearer,
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    pub name: String,
    pub provider: usize,
    pub model: String,
}

#[derive(Clone)]
pub struct Config {
    pub listen: Option<SocketAddr>,
    pub unix_socket: Option<PathBuf>,
    pub trust_loopback: bool,
    pub workers: usize,
    pub queue: usize,
    pub max_body: usize,
    pub max_response: usize,
    pub request_timeout_ms: u64,
    pub upstream_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub default_max_tokens: u64,
    pub token_env: Option<String>,
    pub token: Option<String>,
    pub caller_keys: Option<CallerKeys>,
    pub providers: Vec<Provider>,
    pub routes: Vec<Route>,
    pub plugins: Vec<PluginSpec>,
    pub models_json: String,
}

pub fn default_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("ZCALLD_CONFIG") {
        return Ok(path.into());
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("XDG_CONFIG_HOME must be absolute".into());
        }
        return Ok(path.join("zcalld/config.conf"));
    }
    #[cfg(windows)]
    return env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("zcalld/config.conf"))
        .ok_or_else(|| "set APPDATA, ZCALLD_CONFIG, or --config".into());
    #[cfg(target_os = "macos")]
    let suffix = "Library/Application Support/zcalld/config.conf";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let suffix = ".config/zcalld/config.conf";
    #[cfg(not(windows))]
    env::var_os("HOME")
        .map(|p| PathBuf::from(p).join(suffix))
        .ok_or_else(|| "set HOME, XDG_CONFIG_HOME, or --config".into())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = read_bounded(path, 256 * 1024)?;
        let directory = fs::canonicalize(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|e| format!("configuration directory: {e}"))?;
        let mut config = Self::parse(&text, &directory)?;
        config
            .plugins
            .extend(crate::extensions::managed_plugins(path)?);
        if let Some(privacy) = crate::extensions::privacy_plugin(path)? {
            config.plugins.push(privacy);
        }
        if let Some(routing) = crate::extensions::routing_plugin(path)? {
            config.plugins.push(routing);
        }
        if config.plugins.len() > 32 {
            return Err("configuration exceeds plugin limit after managed extensions".into());
        }
        Ok(config)
    }

    pub fn parse(text: &str, directory: &Path) -> Result<Self, String> {
        let mut sections = parse_sections(text)?;
        let mut server = sections
            .iter()
            .position(|(name, _)| name == "server")
            .map(|index| sections.remove(index).1)
            .unwrap_or_default();
        let listen = match take(&mut server, "listen", "127.0.0.1:9415").as_str() {
            "none" => None,
            value => Some(
                value
                    .parse::<SocketAddr>()
                    .map_err(|_| "listen must be an IP address and port, or none".to_string())?,
            ),
        };
        let unix_socket = socket_path(&take(&mut server, "unix_socket", "none"), directory)?;
        if listen.is_none() && unix_socket.is_none() {
            return Err("configure listen or unix_socket".into());
        }
        let trust_loopback = match take(&mut server, "trust_loopback", "false").as_str() {
            "true" => true,
            "false" => false,
            _ => return Err("trust_loopback must be true or false".into()),
        };
        let workers = number(&mut server, "workers", 4, 1, 64)? as usize;
        let queue = number(&mut server, "queue", 16, 0, 1024)? as usize;
        let max_body = number(
            &mut server,
            "max_body_bytes",
            4 * 1024 * 1024,
            1024,
            16 * 1024 * 1024,
        )? as usize;
        let max_response = number(
            &mut server,
            "max_response_bytes",
            16 * 1024 * 1024,
            1024,
            16 * 1024 * 1024,
        )? as usize;
        let request_timeout_ms = number(&mut server, "request_timeout_ms", 10000, 100, 120000)?;
        let upstream_timeout_ms = number(&mut server, "upstream_timeout_ms", 300000, 100, 3600000)?;
        let connect_timeout_ms = number(&mut server, "connect_timeout_ms", 10000, 100, 120000)?;
        let default_max_tokens = number(&mut server, "default_max_tokens", 4096, 1, 1048576)?;
        let token_env = server.remove("token_env").filter(|s| !s.is_empty());
        let catalog_file = server
            .remove("catalog_file")
            .filter(|value| !value.is_empty())
            .map(|value| resolve(directory, &value));
        let token = token_env
            .as_deref()
            .map(|name| secret(name, directory))
            .transpose()?;
        let caller_keys = server
            .remove("caller_keys_file")
            .map(|value| {
                if value.is_empty() {
                    Err("caller_keys_file must name a private JSON file".into())
                } else {
                    caller_keys::load(&resolve(directory, &value))
                }
            })
            .transpose()?;
        if listen.is_some_and(|address| !address.ip().is_loopback())
            && token.is_none()
            && !caller_keys.as_ref().is_some_and(CallerKeys::allows_lan)
        {
            return Err(
                "a non-loopback listener requires token_env or a caller key with lan=true".into(),
            );
        }
        unknown(&server, "server")?;
        let mut providers = Vec::new();
        let mut pending_routes = Vec::new();
        let mut plugins = Vec::new();
        for (section, mut values) in sections {
            let (kind, name) = section
                .split_once(' ')
                .ok_or_else(|| format!("unknown section [{section}]"))?;
            if !valid_name(name) {
                return Err(format!("invalid section name [{section}]"));
            }
            match kind {
                "provider" => {
                    let base_url = required(&mut values, "base_url", &section)?;
                    let protocol =
                        protocol::parse_protocol(&take(&mut values, "protocol", "chat"))?;
                    protocol::endpoint(&base_url, protocol, "validation-model", false)?;
                    let mut native_endpoints = Vec::new();
                    for (key, native) in [
                        ("chat_url", Protocol::OpenAiChat),
                        ("responses_url", Protocol::OpenAiResponses),
                        ("anthropic_url", Protocol::AnthropicMessages),
                        ("gemini_url", Protocol::GeminiGenerateContent),
                        ("completions_url", Protocol::OpenAiCompletions),
                        ("interactions_url", Protocol::GeminiInteractions),
                    ] {
                        if let Some(url) = values.remove(key) {
                            protocol::endpoint(&url, native, "validation-model", false)?;
                            native_endpoints.push((native, url));
                        }
                    }
                    let api_key_env = values.remove("api_key_env").filter(|v| !v.is_empty());
                    let key = api_key_env
                        .as_deref()
                        .map(|name| secret(name, directory))
                        .transpose()?;
                    let default_auth = match protocol {
                        Protocol::AnthropicMessages => "x-api-key",
                        Protocol::GeminiGenerateContent | Protocol::GeminiInteractions => {
                            "x-goog-api-key"
                        }
                        _ => "bearer",
                    };
                    let auth_explicit = values.contains_key("auth");
                    let auth = match take(&mut values, "auth", default_auth).as_str() {
                        "bearer" => Auth::Bearer,
                        "x-api-key" => Auth::ApiKey,
                        "x-goog-api-key" => Auth::GoogleKey,
                        "none" => Auth::None,
                        _ => {
                            return Err(format!(
                                "[{section}] auth must be bearer, x-api-key, x-goog-api-key, or none"
                            ));
                        }
                    };
                    let headers = provider_headers(&mut values, directory)?;
                    let plugin = values.remove("plugin").filter(|value| !value.is_empty());
                    if plugin.as_deref().is_some_and(|value| !valid_name(value)) {
                        return Err(format!("[{section}] invalid plugin provider ID"));
                    }
                    let plugin_account = values
                        .remove("plugin_account")
                        .filter(|value| !value.is_empty());
                    let plugin_npm = values
                        .remove("plugin_npm")
                        .filter(|value| !value.is_empty());
                    if plugin.is_none() && (plugin_account.is_some() || plugin_npm.is_some()) {
                        return Err(format!(
                            "[{section}] plugin_account/plugin_npm require plugin"
                        ));
                    }
                    if plugin_account
                        .as_ref()
                        .is_some_and(|value| value.len() > 512)
                        || plugin_npm.as_ref().is_some_and(|value| value.len() > 512)
                    {
                        return Err(format!("[{section}] plugin metadata exceeds 512 bytes"));
                    }
                    unknown(&values, &section)?;
                    providers.push(Provider {
                        name: name.into(),
                        base_url,
                        protocol,
                        api_key_env,
                        headers,
                        plugin,
                        plugin_account,
                        plugin_npm,
                        native_endpoints,
                        key,
                        auth,
                        auth_explicit,
                    });
                }
                "route" => {
                    let provider = required(&mut values, "provider", &section)?;
                    let model = required(&mut values, "model", &section)?;
                    if model.len() > 512 || model.chars().any(char::is_control) {
                        return Err(format!("[{section}] invalid model"));
                    }
                    unknown(&values, &section)?;
                    pending_routes.push((name.to_string(), provider, model));
                }
                "plugin" => {
                    let path = resolve(directory, &required(&mut values, "path", &section)?);
                    let inline = values.remove("config");
                    let file = values.remove("config_file");
                    let config = match (inline, file) {
                        (Some(_), Some(_)) => {
                            return Err(format!("[{section}] choose config or config_file"));
                        }
                        (Some(value), None) => value,
                        (None, Some(value)) => read_bounded(&resolve(directory, &value), 65536)?,
                        (None, None) => String::new(),
                    };
                    unknown(&values, &section)?;
                    plugins.push(PluginSpec { path, config });
                }
                _ => return Err(format!("unknown section [{section}]")),
            }
        }
        if providers.is_empty() || pending_routes.is_empty() {
            return Err("configure at least one provider and one route".into());
        }
        if providers.len() > 128 || pending_routes.len() > 4096 || plugins.len() > 32 {
            return Err("configuration exceeds provider/route/plugin limits".into());
        }
        let mut routes = Vec::new();
        for (name, provider, model) in pending_routes {
            let index = providers
                .iter()
                .position(|p| p.name == provider)
                .ok_or_else(|| format!("route {name}: unknown provider {provider}"))?;
            routes.push(Route {
                name,
                provider: index,
                model,
            });
        }
        routes.sort_by(|a, b| a.name.cmp(&b.name));
        let models_json = model_catalog(&routes, catalog_file.as_deref())?;
        Ok(Self {
            listen,
            unix_socket,
            trust_loopback,
            workers,
            queue,
            max_body,
            max_response,
            request_timeout_ms,
            upstream_timeout_ms,
            connect_timeout_ms,
            default_max_tokens,
            token_env,
            token,
            caller_keys,
            providers,
            routes,
            plugins,
            models_json,
        })
    }

    pub fn route(&self, model: &str) -> Option<&Route> {
        self.routes
            .binary_search_by(|r| r.name.as_str().cmp(model))
            .ok()
            .map(|n| &self.routes[n])
    }
}

type Section = BTreeMap<String, String>;
fn parse_sections(text: &str) -> Result<Vec<(String, Section)>, String> {
    let mut sections: Vec<(String, Section)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let err = |message: &str| format!("configuration line {}: {message}", index + 1);
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let section = section.trim().to_string();
            if sections.iter().any(|(name, _)| name == &section) {
                return Err(err("duplicate section"));
            }
            sections.push((section, Section::new()));
            continue;
        }
        let (_, section) = sections
            .last_mut()
            .ok_or_else(|| err("key outside a section"))?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| err("expected key=value"))?;
        let key = key.trim();
        let header = key
            .strip_prefix("header.")
            .or_else(|| key.strip_prefix("header_env."));
        if key.is_empty()
            || !header
                .map(valid_header_name)
                .unwrap_or_else(|| key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        {
            return Err(err("invalid key"));
        }
        let value = value.trim();
        let value = if value.starts_with('"') || value.starts_with('\'') {
            let quote = value.as_bytes()[0] as char;
            value
                .strip_prefix(quote)
                .and_then(|v| v.strip_suffix(quote))
                .ok_or_else(|| err("unclosed quoted value"))?
        } else {
            value
        };
        if value.chars().any(char::is_control) {
            return Err(err("control character in value"));
        }
        if section.insert(key.into(), value.into()).is_some() {
            return Err(err("duplicate key"));
        }
    }
    Ok(sections)
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.contains(['[', ']'])
}

fn model_catalog(routes: &[Route], path: Option<&Path>) -> Result<String, String> {
    let mut metadata = BTreeMap::new();
    if let Some(path) = path {
        let text = read_bounded(path, 4 * 1024 * 1024)?;
        let value =
            crate::state_json::parse(text.as_bytes()).map_err(|e| format!("model catalog: {e}"))?;
        let crate::state_json::Value::Array(models) = value.get("data") else {
            return Err("model catalog requires a JSON object with a data array".into());
        };
        if models.len() > 4096 {
            return Err("model catalog exceeds 4096 entries".into());
        }
        for model in models {
            let id = model
                .get("id")
                .as_str()
                .filter(|id| valid_name(id))
                .ok_or("model catalog entry requires a valid string id")?;
            if metadata.insert(id.to_owned(), model.stringify()).is_some() {
                return Err(format!("model catalog has duplicate id {id}"));
            }
        }
    }
    let mut result = String::from("{\"object\":\"list\",\"data\":[");
    for (index, route) in routes.iter().enumerate() {
        if index != 0 {
            result.push(',');
        }
        if let Some(model) = metadata.get(&route.name) {
            result.push_str(model);
        } else {
            result.push_str(&format!(
                "{{\"id\":{},\"object\":\"model\",\"created\":0,\"owned_by\":\"zcalld\"}}",
                crate::http::quote(&route.name)
            ));
        }
    }
    result.push_str("]}");
    Ok(result)
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn provider_headers(
    values: &mut Section,
    directory: &Path,
) -> Result<Vec<(String, String)>, String> {
    let keys: Vec<String> = values
        .keys()
        .filter(|key| key.starts_with("header.") || key.starts_with("header_env."))
        .cloned()
        .collect();
    if keys.len() > 32 {
        return Err("provider allows at most 32 extra headers".into());
    }
    let mut headers = BTreeMap::new();
    let mut bytes = 0usize;
    for key in keys {
        let (prefix, name) = key.split_once('.').expect("header prefix");
        let name = name.to_ascii_lowercase();
        if !valid_header_name(&name)
            || matches!(
                name.as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "proxy-connection"
                    | "te"
                    | "trailer"
                    | "upgrade"
                    | "expect"
                    | "content-type"
                    | "content-encoding"
                    | "accept"
                    | "accept-encoding"
                    | "authorization"
                    | "proxy-authorization"
                    | "x-api-key"
                    | "x-goog-api-key"
            )
        {
            return Err(format!("provider header {name} is managed by the gateway"));
        }
        let value = values.remove(&key).expect("enumerated key");
        let value = if prefix == "header_env" {
            secret(&value, directory)?
        } else {
            value
        };
        if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(format!("provider header {name} is empty or invalid"));
        }
        bytes += name.len() + value.len() + 4;
        if bytes > 16 * 1024 {
            return Err("provider extra headers exceed 16 KiB".into());
        }
        if headers.insert(name.clone(), value).is_some() {
            return Err(format!("duplicate provider header {name}"));
        }
    }
    Ok(headers.into_iter().collect())
}
fn take(values: &mut Section, key: &str, default: &str) -> String {
    values.remove(key).unwrap_or_else(|| default.into())
}
fn required(values: &mut Section, key: &str, section: &str) -> Result<String, String> {
    values
        .remove(key)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("[{section}] requires {key}"))
}
fn number(
    values: &mut Section,
    key: &str,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    match values.remove(key) {
        None => Ok(default),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|n| (minimum..=maximum).contains(n))
            .ok_or_else(|| format!("{key} must be {minimum}..={maximum}")),
    }
}
fn unknown(values: &Section, section: &str) -> Result<(), String> {
    if let Some(key) = values.keys().next() {
        Err(format!("[{section}] unknown key {key}"))
    } else {
        Ok(())
    }
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn secret(name: &str, directory: &Path) -> Result<String, String> {
    resolve_secret(name, directory, env::var(name))
}

fn resolve_secret(
    name: &str,
    directory: &Path,
    environment: Result<String, env::VarError>,
) -> Result<String, String> {
    if !valid_env_name(name) {
        return Err("credential variable must be a valid environment name".into());
    }
    let value = match environment {
        Ok(value) => value,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!(
                "credential environment variable {name} is not UTF-8"
            ));
        }
        Err(env::VarError::NotPresent) => {
            let path = directory.join("credentials");
            let text = read_credentials(&path)?;
            parse_credentials(&text)?.remove(name).ok_or_else(|| {
                format!(
                    "required credential {name} is not set in the environment or credentials file"
                )
            })?
        }
    };
    if value.is_empty() || value.len() > 16384 || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(format!("credential {name} is empty or invalid"));
    }
    Ok(value)
}

fn read_credentials(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(format!("credentials file: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("credentials file: {e}"))?;
    if !metadata.is_file() {
        return Err("credentials path must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("credentials file must have mode 0600 (chmod 600 credentials)".into());
        }
    }
    let mut text = String::new();
    file.take(256 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "credentials file must contain UTF-8 text".to_string())?;
    if text.len() > 256 * 1024 {
        return Err("credentials file exceeds 256 KiB".into());
    }
    Ok(text)
}

fn parse_credentials(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let invalid = || {
            format!(
                "credentials line {}: expected NAME=value with no interpolation or multiline values",
                index + 1
            )
        };
        let (name, value) = line.split_once('=').ok_or_else(invalid)?;
        let name = name.trim();
        if !valid_env_name(name) {
            return Err(invalid());
        }
        let value = value.trim();
        let value = if value.starts_with('"') {
            let text = zconv::json::JsonString::parse(value.as_bytes()).map_err(|_| invalid())?;
            if text.decoded_len() > 16384 {
                return Err(invalid());
            }
            let mut bytes = vec![0; text.decoded_len()];
            text.decode_into(&mut bytes).map_err(|_| invalid())?;
            String::from_utf8(bytes).map_err(|_| invalid())?
        } else if value.starts_with('\'') {
            value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .ok_or_else(invalid)?
                .to_owned()
        } else {
            value.to_owned()
        };
        if value.is_empty() || value.len() > 16384 || value.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!(
                "credentials line {}: empty or invalid value",
                index + 1
            ));
        }
        if values.insert(name.to_owned(), value).is_some() {
            return Err(format!(
                "credentials line {}: duplicate variable",
                index + 1
            ));
        }
    }
    Ok(values)
}

fn socket_path(value: &str, directory: &Path) -> Result<Option<PathBuf>, String> {
    if value == "none" {
        return Ok(None);
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        if value == "auto" {
            return Ok(None);
        }
        Err("Unix socket listeners are unavailable on this platform; use listen".into())
    }
    #[cfg(unix)]
    {
        let path = if value == "auto" {
            if let Some(path) = env::var_os("RUNTIME_DIRECTORY") {
                // systemd uses ':' to separate multiple RuntimeDirectory entries.
                let first = path
                    .to_str()
                    .ok_or("RUNTIME_DIRECTORY must be UTF-8")?
                    .split(':')
                    .next()
                    .unwrap_or("");
                PathBuf::from(first).join("gateway.sock")
            } else if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
                PathBuf::from(path).join("zcalld/gateway.sock")
            } else {
                directory.join("run/gateway.sock")
            }
        } else {
            if value.is_empty() {
                return Err("unix_socket requires auto, none, or a path".into());
            }
            resolve(directory, value)
        };
        if !path.is_absolute() || path.file_name().is_none() {
            return Err("Unix socket path must resolve to an absolute file path".into());
        }
        Ok(Some(path))
    }
}

fn resolve(directory: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory.join(path)
    }
}
fn read_bounded(path: &Path, maximum: u64) -> Result<String, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    file.take(maximum + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() as u64 > maximum {
        return Err(format!("{} exceeds {maximum} bytes", path.display()));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_strict_and_values_never_enter_errors() {
        let values =
            parse_credentials("# local secrets\nKEY='a=b'\nOTHER=literal-$value\n").unwrap();
        assert_eq!(values["KEY"], "a=b");
        assert_eq!(values["OTHER"], "literal-$value");
        let original = "backslash\\quote\"literal-$value";
        let encoded = format!("KEY={}", crate::http::quote(original));
        assert_eq!(parse_credentials(&encoded).unwrap()["KEY"], original);
        assert!(parse_credentials("KEY=\"bad\\ncontrol\"").is_err());
        for text in [
            "KEY=secret\nKEY=other",
            "1KEY=secret",
            "export KEY=secret",
            "KEY='secret",
            "KEY=",
            "KEY=secret\u{0}",
        ] {
            let error = parse_credentials(text).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn provider_headers_are_case_unique_and_cannot_override_credentials_or_framing() {
        let config = BASE.replace("base_url=", "header.ChatGPT-Account-Id=example-account\nheader.openai-beta=responses=experimental\nbase_url=");
        let parsed = Config::parse(&config, Path::new("/tmp")).unwrap();
        assert_eq!(
            parsed.providers[0].headers[0],
            ("chatgpt-account-id".into(), "example-account".into())
        );
        let duplicate = config.replace(
            "header.openai-beta=",
            "header.CHATGPT-ACCOUNT-ID=duplicate\nheader.openai-beta=",
        );
        assert!(Config::parse(&duplicate, Path::new("/tmp")).is_err());
        for name in [
            "Host",
            "Authorization",
            "x-api-key",
            "Content-Length",
            "Connection",
            "Transfer-Encoding",
        ] {
            let config = BASE.replace("base_url=", &format!("header.{name}=blocked\nbase_url="));
            assert!(Config::parse(&config, Path::new("/tmp")).is_err());
        }
    }

    #[test]
    fn native_protocol_endpoints_preserve_exact_configured_urls() {
        let text = BASE.replace("base_url=", "anthropic_url=https://example.com/custom/v1/messages\nresponses_url=https://example.com/codex/responses?flag=yes\nbase_url=");
        let cfg = Config::parse(&text, Path::new("/tmp")).unwrap();
        let provider = &cfg.providers[0];
        let (protocol, url, auth) = provider.for_protocol(Protocol::AnthropicMessages);
        assert_eq!(protocol, Protocol::AnthropicMessages);
        assert_eq!(url, "https://example.com/custom/v1/messages");
        assert!(matches!(auth, Auth::ApiKey));
        assert_eq!(
            protocol::endpoint(url, protocol, "model", false).unwrap(),
            url
        );
        assert_eq!(
            provider.for_protocol(Protocol::OpenAiCompletions).0,
            Protocol::OpenAiChat
        );
        let explicit = Config::parse(
            &text.replace("anthropic_url=", "auth=bearer\nanthropic_url="),
            Path::new("/tmp"),
        )
        .unwrap();
        assert!(matches!(
            explicit.providers[0]
                .for_protocol(Protocol::AnthropicMessages)
                .2,
            Auth::Bearer
        ));
    }

    #[test]
    fn catalog_keeps_metadata_only_for_routes_and_caches_it() {
        let directory = std::env::temp_dir().join(format!("zcalld-catalog-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("models.json");
        fs::write(&path, r#"{"data":[{"id":"test@account#fast","context":200000,"reasoning":true,"tool_call":true,"extra":{"unchanged":9007199254740993}},{"id":"not-routed","secret":"not-listed"}]}"#).unwrap();
        let text = format!(
            "[server]\ncatalog_file=models.json\n{}\n[route new]\nprovider=local\nmodel=new\n",
            BASE.replace("[route test]", "[route test@account#fast]")
        );
        let cfg = Config::parse(&text, &directory).unwrap();
        let catalog = crate::state_json::parse(cfg.models_json.as_bytes()).unwrap();
        assert_eq!(catalog.get("data").as_array().len(), 2);
        let model = &catalog.get("data").as_array()[1];
        assert_eq!(model.get("context").as_u64(), Some(200000));
        assert_eq!(
            model.get("extra").get("unchanged").as_u64(),
            Some(9007199254740993)
        );
        assert!(!cfg.models_json.contains("not-routed"));
        fs::write(&path, r#"{"data":[{"id":"duplicate"},{"id":"duplicate"}]}"#).unwrap();
        assert!(Config::parse(&text, &directory).is_err());
        fs::remove_dir_all(directory).unwrap();
        assert!(cfg.models_json.contains("test@account#fast"));
        assert!(!valid_name("bad]\\n[provider injected"));
    }

    #[test]
    fn environment_takes_precedence_without_reading_a_file() {
        assert_eq!(
            resolve_secret(
                "ZCALLD_TEST",
                Path::new("/missing/directory"),
                Ok("environment-value".into())
            )
            .unwrap(),
            "environment-value"
        );
        assert!(resolve_secret("INVALID-NAME", Path::new("/tmp"), Ok("secret".into())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_only_configuration_and_private_credential_file() {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let directory =
            std::env::temp_dir().join(format!("zcalld-credentials-{}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("credentials");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        writeln!(file, "ZCALLD_TEST_FILE_ONLY=file-value").unwrap();
        assert_eq!(
            resolve_secret(
                "ZCALLD_TEST_FILE_ONLY",
                &directory,
                Err(env::VarError::NotPresent)
            )
            .unwrap(),
            "file-value"
        );
        let cfg = Config::parse(
            &format!("[server]\nlisten=none\nunix_socket=run/gateway.sock\n{BASE}"),
            &directory,
        )
        .unwrap();
        assert!(cfg.listen.is_none());
        assert_eq!(cfg.unix_socket.unwrap(), directory.join("run/gateway.sock"));
        assert!(Config::parse(&format!("[server]\nlisten=none\n{BASE}"), &directory).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            resolve_secret(
                "ZCALLD_TEST_FILE_ONLY",
                &directory,
                Err(env::VarError::NotPresent)
            )
            .is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }
    const BASE: &str = "[provider local]\nbase_url=http://127.0.0.1:11434/v1\n[route test]\nprovider=local\nmodel=upstream\n";
    #[test]
    fn strict_configuration() {
        let cfg = Config::parse(BASE, Path::new("/tmp")).unwrap();
        assert_eq!(cfg.route("test").unwrap().model, "upstream");
        assert!(Config::parse(&format!("[server]\nworkers=0\n{BASE}"), Path::new("/tmp")).is_err());
        assert!(Config::parse(&format!("[server]\nworker=4\n{BASE}"), Path::new("/tmp")).is_err());
        assert!(
            Config::parse(
                &format!("[server]\nlisten=0.0.0.0:9415\n{BASE}"),
                Path::new("/tmp")
            )
            .is_err()
        );
        assert!(Config::parse(&format!("{BASE}\nmodel=duplicate"), Path::new("/tmp")).is_err());
    }
}
