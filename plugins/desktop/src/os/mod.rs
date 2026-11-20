use crate::{config::Config, logic::Notice, private};
use zcalld_compat_client::json::object;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Off,
    System,
    Display,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Capability {
    Unknown = 0,
    Available = 1,
    Unavailable = 2,
    Denied = 3,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    NoIdentity = 4,
    Failed = 5,
    RecordOnly = 6,
}
impl Capability {
    pub fn name(code: u32) -> &'static str {
        match code {
            0 => "unknown",
            1 => "available",
            2 => "unavailable",
            3 => "denied",
            4 => "identity_required",
            5 => "failed",
            6 => "record_only",
            _ => "unknown",
        }
    }
}
pub trait Backend {
    fn inhibit(&mut self, level: Level) -> Capability;
    fn notify(&mut self, notice: &Notice) -> Capability;
    fn display_supported(&self) -> bool {
        false
    }
}
pub fn create(config: &Config) -> Box<dyn Backend> {
    if config.record_only {
        return Box::new(Record {
            dir: config.state_dir.clone(),
            level: Level::Off,
        });
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::Linux::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::Windows::new(config.notification_helper.clone()))
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::Mac::new(config.notification_helper.clone()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Box::new(Unavailable)
    }
}
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
struct Unavailable;
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
impl Backend for Unavailable {
    fn inhibit(&mut self, _: Level) -> Capability {
        Capability::Unavailable
    }
    fn notify(&mut self, _: &Notice) -> Capability {
        Capability::Unavailable
    }
}
struct Record {
    dir: std::path::PathBuf,
    level: Level,
}
impl Backend for Record {
    fn inhibit(&mut self, level: Level) -> Capability {
        if self.level != level {
            let v = object([
                ("kind", "inhibit".into()),
                (
                    "level",
                    match level {
                        Level::Off => "off",
                        Level::System => "system",
                        Level::Display => "display",
                    }
                    .into(),
                ),
            ]);
            if private::record(&self.dir, &v).is_err() {
                return Capability::Failed;
            }
            self.level = level;
        }
        Capability::RecordOnly
    }
    fn notify(&mut self, notice: &Notice) -> Capability {
        if private::record(&self.dir, &notice.json()).is_ok() {
            Capability::RecordOnly
        } else {
            Capability::Failed
        }
    }
    fn display_supported(&self) -> bool {
        true
    }
}
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod helper;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
