//! Native AI gateway: small CLI, explicit routes, optional native plugins.

mod adapters;
mod config;
mod control;
mod curl;
mod digest;
mod extensions;
mod http;
mod maintenance;
mod migration;
mod plugins;
mod protocol;
mod server;
mod state_json;
mod system;
mod transport;

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
};

const HELP: &str = "zcalld — compact native AI gateway\n\n\
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
            if std::env::args().any(|a| a == "--json") {
                println!("{{\"ok\":false,\"error\":{}}}", http::quote(&error));
            } else {
                eprintln!("zcalld: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let management_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = control::run(&management_args) {
        return result;
    }
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
        #[cfg(unix)]
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut directory = std::fs::DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            directory.mode(0o700);
            directory.create(parent).map_err(|e| e.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let starter = include_str!("../examples/config.conf");
        #[cfg(unix)]
        let starter = starter.replace("[server]", "[server]\nunix_socket = auto");
        file.write_all(starter.as_bytes())
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
            let listen = config.listen.ok_or("client environment requires a TCP listener; use the Unix socket with a socket-capable client")?;
            let address = match listen.ip() {
                std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                    format!("127.0.0.1:{}", listen.port())
                }
                std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                    format!("[::1]:{}", listen.port())
                }
                _ => listen.to_string(),
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
            let mut socket = transport::connect(&config).map_err(|e| e.to_string())?;
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .map_err(|e| e.to_string())?;
            write!(
                socket,
                "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
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
