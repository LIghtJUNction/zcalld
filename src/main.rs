//! Linux-native AI gateway: small CLI, explicit routes, optional native plugins.
#[cfg(not(target_os = "linux"))]
compile_error!("zcalld currently targets Linux");

mod config;
mod curl;
mod http;
mod plugins;
mod protocol;
mod server;
mod system;

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
};

const HELP: &str = "zcalld — compact Linux AI gateway\n\n\
Usage: zcalld <command> [--config PATH] [--json]\n\n\
Commands:\n\
  init                 Create a starter config without overwriting files\n\
  serve                Run the gateway in the foreground (systemd friendly)\n\
  check                Validate config, credentials and plugin ABI\n\
  models               List configured public model names\n\
  providers            List configured upstreams without credentials\n\
  plugins              List explicitly enabled native plugins\n\
  env [openai|anthropic|gemini]  Print shell client environment exports\n\
  health               Query the configured gateway's health endpoint\n\
  version              Show the version\n\n\
Options:\n\
  -c, --config PATH    Override ZCALLD_CONFIG / XDG configuration path\n\
  --json               Machine-readable list/check output\n\
  -h, --help           Show this help\n\
  -V, --version        Show the version\n\n\
Config: $ZCALLD_CONFIG, otherwise $XDG_CONFIG_HOME/zcalld/config.conf\n\
        or $HOME/.config/zcalld/config.conf\n";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zcalld: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut extra = Vec::new();
    let mut path = None;
    let mut json = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("zcalld {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-c" | "--config" => {
                if path.is_some() {
                    return Err("--config specified twice".into());
                }
                path = Some(PathBuf::from(
                    args.next().ok_or("--config requires a path")?,
                ));
            }
            "--json" => json = true,
            value if value.starts_with('-') => return Err(format!("unknown option {value}")),
            _ if command.is_none() => command = Some(argument),
            _ => extra.push(argument),
        }
    }
    let command = command.as_deref().unwrap_or("help");
    if command == "help" {
        print!("{HELP}");
        return Ok(());
    }
    if command == "version" {
        println!("zcalld {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !matches!(
        command,
        "init" | "serve" | "check" | "models" | "providers" | "plugins" | "env" | "health"
    ) {
        return Err(format!("unknown command {command}; use --help"));
    }
    if (command != "env" && !extra.is_empty()) || extra.len() > 1 {
        return Err("unexpected command argument".into());
    }
    let path = path.map(Ok).unwrap_or_else(config::default_path)?;
    if command == "init" {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(include_bytes!("../examples/config.conf"))
            .map_err(|e| e.to_string())?;
        println!("Created {}", path.display());
        return Ok(());
    }
    let config = config::Config::load(&path)?;
    match command {
        "serve" => server::serve(config),
        "check" => {
            curl::init()?;
            let plugins = plugins::Plugins::load(&config.plugins)?;
            if json {
                println!(
                    "{{\"valid\":true,\"providers\":{},\"routes\":{},\"plugins\":{},\"transport\":{}}}",
                    config.providers.len(),
                    config.routes.len(),
                    plugins.len(),
                    http::quote(&curl::version())
                );
            } else {
                println!(
                    "Valid: {} providers, {} routes, {} plugins; {}",
                    config.providers.len(),
                    config.routes.len(),
                    plugins.len(),
                    curl::version()
                );
            }
            Ok(())
        }
        "models" => {
            if json {
                println!("{}", server::models_json(&config));
            } else {
                for route in &config.routes {
                    println!(
                        "{}\t{}\t{}",
                        route.name, config.providers[route.provider].name, route.model
                    );
                }
            }
            Ok(())
        }
        "providers" => {
            if json {
                let values = config
                    .providers
                    .iter()
                    .map(|provider| {
                        format!(
                            "{{\"name\":{},\"protocol\":{}}}",
                            http::quote(&provider.name),
                            http::quote(protocol::protocol_name(provider.protocol))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                println!("[{values}]");
            } else {
                for provider in &config.providers {
                    println!(
                        "{}\t{}",
                        provider.name,
                        protocol::protocol_name(provider.protocol)
                    );
                }
            }
            Ok(())
        }
        "plugins" => {
            if json {
                println!(
                    "[{}]",
                    config
                        .plugins
                        .iter()
                        .map(|p| http::quote(&p.path.to_string_lossy()))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            } else {
                for plugin in &config.plugins {
                    println!("{}", plugin.path.display());
                }
            }
            Ok(())
        }
        "env" => {
            let token = config
                .token_env
                .as_ref()
                .map(|name| format!("\"${{{name}}}\""))
                .unwrap_or_else(|| "zcalld".into());
            let address = match config.listen.ip() {
                std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                    format!("127.0.0.1:{}", config.listen.port())
                }
                std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                    format!("[::1]:{}", config.listen.port())
                }
                _ => config.listen.to_string(),
            };
            match extra.first().map(String::as_str).unwrap_or("openai") {
                "openai" => println!(
                    "export OPENAI_BASE_URL=http://{address}/v1\nexport OPENAI_API_KEY={token}"
                ),
                "anthropic" => println!(
                    "export ANTHROPIC_BASE_URL=http://{address}\nexport ANTHROPIC_API_KEY={token}"
                ),
                "gemini" => println!(
                    "export GOOGLE_GEMINI_BASE_URL=http://{address}\nexport GEMINI_API_KEY={token}"
                ),
                _ => return Err("env accepts openai, anthropic, or gemini".into()),
            }
            Ok(())
        }
        "health" => {
            let address = if config.listen.ip().is_unspecified() {
                std::net::SocketAddr::new(
                    if config.listen.is_ipv4() {
                        "127.0.0.1".parse().unwrap()
                    } else {
                        "::1".parse().unwrap()
                    },
                    config.listen.port(),
                )
            } else {
                config.listen
            };
            let mut socket =
                std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(3))
                    .map_err(|e| e.to_string())?;
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .map_err(|e| e.to_string())?;
            write!(
                socket,
                "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .map_err(|e| e.to_string())?;
            let mut result = String::new();
            socket
                .take(65536)
                .read_to_string(&mut result)
                .map_err(|e| e.to_string())?;
            if !result.starts_with("HTTP/1.1 200 ") {
                return Err("gateway health check failed".into());
            }
            println!(
                "{}",
                result
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body)
                    .unwrap_or("")
            );
            Ok(())
        }
        _ => unreachable!(),
    }
}
