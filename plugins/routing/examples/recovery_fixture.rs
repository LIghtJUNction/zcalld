//! Test-only native ABI fixture. It has no network transport or credentials.
use std::{fs, io::Write, path::PathBuf};
use zcalld_plugin_api::{
    GatewayPlugin, GatewayPluginExport, ProviderPlugin, ProviderPluginExport, RecoveryPlugin,
    RecoveryPluginExport, recovery, status,
};
use zplugin::{Error, OwnedResource, ResourceHandler, Result};

#[allow(dead_code)]
#[path = "../../../src/state_json.rs"]
mod json;
mod http {
    pub fn quote(value: &str) -> String {
        let mut out = String::from("\"");
        for c in value.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}
use json::{Value, object};

#[derive(Default)]
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn configure_path(&mut self, value: &str) -> Result<()> {
        self.root = PathBuf::from(value);
        if !self.root.is_absolute() || !self.root.is_dir() {
            return Err(Error::InvalidArgument);
        }
        Ok(())
    }
    fn settings(&self) -> Result<Value> {
        let data = fs::read(self.root.join("settings.json")).map_err(|_| Error::InvalidArgument)?;
        json::parse(&data).map_err(|_| Error::InvalidArgument)
    }
    fn record(&self, name: &str, data: &[u8]) -> Result<()> {
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.root.join(name))
            .and_then(|mut file| {
                file.write_all(data)?;
                file.write_all(b"\n")
            })
            .map_err(|_| Error::InvalidArgument)
    }
}
impl GatewayPlugin for Fixture {
    fn configure(&mut self, value: &str) -> Result<()> {
        self.configure_path(value)
    }
    fn route(&mut self, _model: &str, _output: &mut [u8]) -> Result<u64> {
        Ok(0)
    }
    fn request(&mut self, _protocol: u32, _input: &[u8], _output: &mut [u8]) -> Result<u64> {
        self.record("request.log", b"request")?;
        Ok(0)
    }
    fn observe(&mut self, _status: u32, _elapsed: u64, _input: u64, _output: u64) -> Result<()> {
        Ok(())
    }
}
impl ProviderPlugin for Fixture {
    fn configure(&mut self, value: &str) -> Result<()> {
        self.configure_path(value)
    }
    fn fetch(&mut self, request: &[u8]) -> Result<OwnedResource> {
        let input = json::parse(request).map_err(|_| Error::InvalidArgument)?;
        let body = input.get("body").as_str().ok_or(Error::InvalidArgument)?;
        let body = json::parse(body.as_bytes()).map_err(|_| Error::InvalidArgument)?;
        let model = body.get("model").as_str().ok_or(Error::InvalidArgument)?;
        self.record(
            "fetch.log",
            object([
                ("model", model.into()),
                ("account", input.get("account").clone()),
                ("body", body.clone()),
            ])
            .stringify()
            .as_bytes(),
        )?;
        let settings = self.settings()?;
        let recovered = self.root.join("reset").exists();
        let code = settings
            .get(if recovered { "after" } else { "before" })
            .get(model)
            .as_u64()
            .unwrap_or(if recovered { 200 } else { 429 });
        let data = if code == 200 {
            object([
                ("model", model.into()),
                (
                    "choices",
                    Value::Array(vec![object([(
                        "message",
                        object([
                            ("role", "assistant".into()),
                            ("content", "fixture success".into()),
                        ]),
                    )])]),
                ),
            ])
            .stringify()
        } else {
            "{\"error\":{\"message\":\"fixture refusal\"}}".into()
        };
        let head = object([
            ("status", code.into()),
            (
                "headers",
                object([("content-type", "application/json".into())]),
            ),
        ])
        .stringify()
        .into_bytes();
        OwnedResource::new(Stream {
            head,
            body: data.into_bytes(),
            offset: 0,
            fail_head: settings.get("fail_head").as_bool() == Some(true),
            fail_body: settings.get("fail_body").as_bool() == Some(true),
        })
    }
}
struct Stream {
    head: Vec<u8>,
    body: Vec<u8>,
    offset: usize,
    fail_head: bool,
    fail_body: bool,
}
fn copy(data: &[u8], output: &mut [u8]) -> Result<usize> {
    if output.len() < data.len() {
        return Err(Error::BufferTooSmall);
    }
    output[..data.len()].copy_from_slice(data);
    Ok(data.len())
}
impl ResourceHandler for Stream {
    const ID: &'static str = "dev.zcalld.provider-stream";
    const VERSION: u32 = 1;
    fn call(&mut self, operation: u32, _input: &[u8], output: &mut [u8]) -> Result<usize> {
        match operation {
            1 if self.fail_head => Err(Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED)),
            1 => copy(&self.head, output),
            2 if self.fail_body => Err(Error::PluginStatus(status::PROVIDER_TRANSPORT_FAILED)),
            2 => {
                let count = (self.body.len() - self.offset).min(output.len());
                output[..count].copy_from_slice(&self.body[self.offset..self.offset + count]);
                self.offset += count;
                Ok(count)
            }
            3 => Ok(0),
            _ => Err(Error::Unsupported),
        }
    }
}
impl RecoveryPlugin for Fixture {
    fn configure(&mut self, value: &str) -> Result<()> {
        self.configure_path(value)
    }
    fn accepts(&mut self) -> Result<u32> {
        Ok(1)
    }
    fn begin(&mut self, metadata: &[u8]) -> Result<OwnedResource> {
        self.record("recovery.log", metadata)?;
        let settings = self.settings()?;
        let mode = settings.get("mode").as_str().unwrap_or("recover");
        let result = match mode {
            "decline" | "unknown" => b"{\"recovered\":false}".to_vec(),
            "malformed" => b"{\"recovered\":true,\"candidate\":0,\"surprise\":true}".to_vec(),
            _ => object([
                ("recovered", true.into()),
                (
                    "candidate",
                    settings.get("candidate").as_u64().unwrap_or(0).into(),
                ),
            ])
            .stringify()
            .into_bytes(),
        };
        OwnedResource::new(Recovery {
            root: self.root.clone(),
            result,
            pending: mode == "pending",
            finished: false,
            reset: mode == "recover",
        })
    }
    fn resume(
        &mut self,
        _protocol: u32,
        _model: &str,
        _body: &[u8],
        _output: &mut [u8],
    ) -> Result<u64> {
        self.record("resume.log", b"resume")?;
        Ok(0)
    }
}
struct Recovery {
    root: PathBuf,
    result: Vec<u8>,
    pending: bool,
    finished: bool,
    reset: bool,
}
impl ResourceHandler for Recovery {
    const ID: &'static str = recovery::RESOURCE_ID;
    const VERSION: u32 = recovery::RESOURCE_VERSION;
    fn call(&mut self, operation: u32, _input: &[u8], output: &mut [u8]) -> Result<usize> {
        if operation != recovery::POLL {
            return Err(Error::Unsupported);
        }
        if self.pending {
            return Err(Error::PluginStatus(3));
        }
        let count = copy(&self.result, output)?;
        if !self.finished && self.reset {
            fs::write(self.root.join("reset"), b"synthetic reset")
                .map_err(|_| Error::InvalidArgument)?;
        }
        self.finished = true;
        Ok(count)
    }
}
impl Drop for Recovery {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::write(self.root.join("cancelled"), b"cancelled");
        }
    }
}
fn create() -> Result<Fixture> {
    Ok(Fixture::default())
}
zplugin::export_interfaces!(
    version=(0,0,0),unload=pinned;
    GatewayPluginExport<Fixture> => create,
    ProviderPluginExport<Fixture> => create,
    RecoveryPluginExport<Fixture> => create,
);
