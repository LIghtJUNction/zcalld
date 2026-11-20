//! Windows private-file protection shared by the host and JSON writers.
use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        buffer: *mut c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn GetSecurityDescriptorDacl(
        descriptor: *mut c_void,
        present: *mut i32,
        acl: *mut *mut c_void,
        defaulted: *mut i32,
    ) -> i32;
    fn SetSecurityInfo(
        handle: *mut c_void,
        kind: u32,
        information: u32,
        owner: *mut c_void,
        group: *mut c_void,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}
struct Token(*mut c_void);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
pub fn random(bytes: &mut [u8; 32]) -> Result<(), String> {
    // SAFETY: the system-preferred RNG writes exactly the live buffer length.
    if unsafe { BCryptGenRandom(ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) } == 0 {
        Ok(())
    } else {
        Err("Windows system random source is unavailable".into())
    }
}
pub fn protect(file: &std::fs::File) -> Result<(), String> {
    let error =
        || "cannot restrict the private file to this Windows account and SYSTEM".to_string();
    let mut token = ptr::null_mut();
    // SAFETY: handles and output buffers follow the synchronous Win32 contracts.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 {
            return Err(error());
        }
        let token = Token(token);
        let mut size = 0;
        GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut size);
        if size == 0 || size > 65536 {
            return Err(error());
        }
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(token.0, 1, storage.as_mut_ptr().cast(), size, &mut size) == 0 {
            return Err(error());
        }
        let sid = *(storage.as_ptr().cast::<*mut c_void>());
        let mut raw = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut raw) == 0 || raw.is_null() {
            return Err(error());
        }
        let sid_memory = Local(raw.cast());
        let mut length = 0;
        while length < 256 && *raw.add(length) != 0 {
            length += 1;
        }
        if length == 256 {
            return Err(error());
        }
        let sid =
            String::from_utf16(std::slice::from_raw_parts(raw, length)).map_err(|_| error())?;
        drop(sid_memory);
        let text = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(error());
        }
        let descriptor = Local(descriptor);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = ptr::null_mut();
        if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) == 0
            || present == 0
            || acl.is_null()
        {
            return Err(error());
        }
        if SetSecurityInfo(
            file.as_raw_handle(),
            1,
            0x80000004,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null_mut(),
        ) != 0
        {
            return Err(error());
        }
    }
    Ok(())
}
