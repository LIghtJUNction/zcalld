use super::{Backend, Capability, Level, helper};
use crate::logic::Notice;
use std::{
    ffi::{c_char, c_void},
    path::PathBuf,
    ptr,
};

type CFRef = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: CFRef, text: *const c_char, encoding: u32) -> CFRef;
    fn CFRelease(value: CFRef);
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(kind: CFRef, level: u32, name: CFRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}
pub struct Mac {
    helper: Option<PathBuf>,
    assertions: Vec<u32>,
}
impl Mac {
    pub fn new(helper: Option<PathBuf>) -> Self {
        Self {
            helper,
            assertions: Vec::new(),
        }
    }
    fn clear(&mut self) {
        for id in self.assertions.drain(..) {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
    }
}
impl Backend for Mac {
    fn inhibit(&mut self, level: Level) -> Capability {
        self.clear();
        if level == Level::Off {
            return Capability::Available;
        }
        // Retained CF string lives through both synchronous assertion creations.
        let name = unsafe {
            CFStringCreateWithCString(ptr::null(), c"zcalld agent activity".as_ptr(), 0x08000100)
        };
        if name.is_null() {
            return Capability::Failed;
        }
        // IOPMLib's assertion type constants are CFSTR macros, not exported symbols.
        let kinds = [
            c"PreventUserIdleSystemSleep",
            c"PreventUserIdleDisplaySleep",
        ];
        let n = if level == Level::Display { 2 } else { 1 };
        let mut result = Capability::Available;
        for kind in &kinds[..n] {
            let kind = unsafe { CFStringCreateWithCString(ptr::null(), kind.as_ptr(), 0x08000100) };
            if kind.is_null() {
                result = Capability::Failed;
                break;
            }
            let mut id = 0;
            let rc = unsafe { IOPMAssertionCreateWithName(kind, 255, name, &mut id) };
            unsafe {
                CFRelease(kind);
            }
            if rc != 0 {
                result = Capability::Denied;
                break;
            }
            self.assertions.push(id);
        }
        unsafe {
            CFRelease(name);
        }
        if result != Capability::Available {
            self.clear();
        }
        result
    }
    fn notify(&mut self, notice: &Notice) -> Capability {
        helper::send(self.helper.as_deref(), notice)
    }
    fn display_supported(&self) -> bool {
        true
    }
}
impl Drop for Mac {
    fn drop(&mut self) {
        self.clear();
    }
}
