//! Small, strict configuration reader. Credentials are resolved from the environment.
use crate::{
    plugins::PluginSpec,
    protocol::{self, Protocol},
};
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
    pub key: Option<String>,
    pub auth: Auth,
}

#[derive(Clone, Copy)]
pub enum Auth {
    Bearer,
    ApiKey,
    GoogleKey,
    None,
}

#[derive(Clone, Debug)]
pub struct Route {
    pub name: String,
    pub provider: usize,
    pub model: String,
}

#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
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
    pub providers: Vec<Provider>,
    pub routes: Vec<Route>,
    pub plugins: Vec<PluginSpec>,
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
    env::var_os("HOME")
        .map(|p| PathBuf::from(p).join(".config/zcalld/config.conf"))
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
        Self::parse(&text, &directory)
    }

    pub fn parse(text: &str, directory: &Path) -> Result<Self, String> {
        let mut sections = parse_sections(text)?;
        let mut server = sections
            .iter()
            .position(|(name, _)| name == "server")
            .map(|index| sections.remove(index).1)
            .unwrap_or_default();
        let listen: SocketAddr = take(&mut server, "listen", "127.0.0.1:9415")
            .parse()
            .map_err(|_| "listen must be an IP address and port".to_string())?;
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
        let token = token_env.as_deref().map(secret).transpose()?;
        if !listen.ip().is_loopback() && token.is_none() {
            return Err("a non-loopback listener requires token_env".into());
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
                    let key = values
                        .remove("api_key_env")
                        .filter(|v| !v.is_empty())
                        .as_deref()
                        .map(secret)
                        .transpose()?;
                    let default_auth = match protocol {
                        Protocol::AnthropicMessages => "x-api-key",
                        Protocol::GeminiGenerateContent | Protocol::GeminiInteractions => {
                            "x-goog-api-key"
                        }
                        _ => "bearer",
                    };
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
                    unknown(&values, &section)?;
                    providers.push(Provider {
                        name: name.into(),
                        base_url,
                        protocol,
                        key,
                        auth,
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
        Ok(Self {
            listen,
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
            providers,
            routes,
            plugins,
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
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
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
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
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
fn secret(name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.as_bytes()[0].is_ascii_digit()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("credential variable must be a valid environment name".into());
    }
    let value = env::var(name)
        .map_err(|_| format!("required credential environment variable {name} is not set"))?;
    if value.is_empty() || value.len() > 16384 || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(format!(
            "credential environment variable {name} is empty or invalid"
        ));
    }
    Ok(value)
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
