use super::{Backend, Capability, Level, helper};
use crate::logic::Notice;
use std::path::PathBuf;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}
pub struct Windows {
    helper: Option<PathBuf>,
    held: bool,
}
impl Windows {
    pub fn new(helper: Option<PathBuf>) -> Self {
        Self {
            helper,
            held: false,
        }
    }
}
impl Backend for Windows {
    fn inhibit(&mut self, level: Level) -> Capability {
        let mut flags = 0x80000000;
        if level != Level::Off {
            flags |= 1;
            if level == Level::Display {
                flags |= 2;
            }
        }
        // All calls, including release and Drop, occur on this one OS worker thread.
        if unsafe { SetThreadExecutionState(flags) } == 0 {
            return Capability::Failed;
        }
        self.held = level != Level::Off;
        Capability::Available
    }
    fn notify(&mut self, notice: &Notice) -> Capability {
        helper::send(self.helper.as_deref(), notice)
    }
    fn display_supported(&self) -> bool {
        true
    }
}
impl Drop for Windows {
    fn drop(&mut self) {
        if self.held {
            unsafe {
                SetThreadExecutionState(0x80000000);
            }
        }
    }
}
