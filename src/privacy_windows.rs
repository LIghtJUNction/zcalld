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

// These read-side functions share the same Windows module with installer builds.
// Hosts which only write credentials do not need to call the inspection API.
#[link(name = "advapi32")]
unsafe extern "system" {
    fn GetSecurityInfo(handle: *mut c_void, kind: u32, information: u32,
        owner: *mut *mut c_void, group: *mut *mut c_void, dacl: *mut *mut c_void,
        sacl: *mut *mut c_void, descriptor: *mut *mut c_void) -> u32;
    fn GetSecurityDescriptorControl(descriptor: *const c_void, control: *mut u16, revision: *mut u32) -> i32;
    fn GetAclInformation(acl: *const c_void, information: *mut c_void, size: u32, class: u32) -> i32;
    fn GetAce(acl: *const c_void, index: u32, ace: *mut *mut c_void) -> i32;
    fn EqualSid(a: *const c_void, b: *const c_void) -> i32;
    fn GetLengthSid(sid: *const c_void) -> u32;
    fn IsValidSid(sid: *const c_void) -> i32;
    fn CreateWellKnownSid(kind: u32, domain: *const c_void, sid: *mut c_void, size: *mut u32) -> i32;
}

fn private_user() -> Result<Vec<usize>, String> {
    let error = || "cannot inspect the current Windows account".to_string();
    let mut token = ptr::null_mut();
    // SAFETY: all output buffers are owned and sized according to Win32 results.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 { return Err(error()); }
        let token = Token(token);
        let mut size = 0;
        GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut size);
        if size == 0 || size > 65536 { return Err(error()); }
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(token.0, 1, storage.as_mut_ptr().cast(), size, &mut size) == 0 { return Err(error()); }
        Ok(storage)
    }
}

/// Read-only privacy validation: owner is this account, and a protected DACL
/// grants access only to this account and SYSTEM. Unknown/conditional/object
/// ACEs, inherited access, a missing DACL, and any other SID are rejected.
#[allow(dead_code)] // Shared source: write-only credential callers need no inspection API.
pub fn check_private(file: &std::fs::File) -> Result<(), String> {
    let error = || "private Windows files must be owned by this account and accessible only to it and SYSTEM".to_string();
    let user = private_user()?;
    #[repr(C)]
    struct AclSize { count: u32, used: u32, free: u32 }
    let mut owner = ptr::null_mut();
    let mut acl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: GetSecurityInfo returns one LocalFree-owned descriptor. Its SID and
    // ACL pointers remain live until the Local guard is dropped below.
    unsafe {
        if GetSecurityInfo(file.as_raw_handle(), 1, 1 | 4, &mut owner, ptr::null_mut(), &mut acl,
            ptr::null_mut(), &mut descriptor) != 0 || descriptor.is_null() { return Err(error()); }
        let descriptor = Local(descriptor);
        let user_sid = *(user.as_ptr().cast::<*mut c_void>());
        if owner.is_null() || acl.is_null() || EqualSid(owner, user_sid) == 0 { return Err(error()); }
        let mut control = 0; let mut revision = 0;
        if GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0 || control & 0x1000 == 0 { return Err(error()); }
        let mut size = AclSize { count: 0, used: 0, free: 0 };
        if GetAclInformation(acl, (&mut size as *mut AclSize).cast(), std::mem::size_of::<AclSize>() as u32, 2) == 0
            || size.count == 0 || size.count > 2 || size.used > 65536 { return Err(error()); }
        let mut system = [0usize; 16]; let mut system_size = std::mem::size_of_val(&system) as u32;
        if CreateWellKnownSid(22, ptr::null(), system.as_mut_ptr().cast(), &mut system_size) == 0 { return Err(error()); }
        let mut found_user = false; let mut found_system = false;
        for index in 0..size.count {
            let mut ace = ptr::null_mut();
            if GetAce(acl, index, &mut ace) == 0 || ace.is_null() { return Err(error()); }
            let bytes = ace.cast::<u8>();
            let offset = (bytes as usize).checked_sub(acl as usize).ok_or_else(error)?;
            if offset.checked_add(8).is_none_or(|n| n > size.used as usize) { return Err(error()); }
            let length = ptr::read_unaligned(bytes.add(2).cast::<u16>()) as usize;
            if *bytes != 0 || *bytes.add(1) != 0 || length < 16 || offset.checked_add(length).is_none_or(|n| n > size.used as usize) { return Err(error()); }
            let sid = bytes.add(8).cast::<c_void>();
            if GetLengthSid(sid) as usize > length - 8 || IsValidSid(sid) == 0 { return Err(error()); }
            let is_user = EqualSid(sid, user_sid) != 0;
            let is_system = EqualSid(sid, system.as_ptr().cast()) != 0;
            if !is_user && !is_system { return Err(error()); }
            found_user |= is_user; found_system |= is_system;
        }
        if !found_user || !found_system { return Err(error()); }
    }
    Ok(())
}

/// Protect a handle immediately after exclusive creation, before any payload is
/// written. This additionally fixes the new object's owner under admin tokens;
/// it must never be used to take over an existing file or directory.
#[allow(dead_code)] // Shared source: only the installer creates directory cohorts.
pub fn protect_created(file: &std::fs::File) -> Result<(), String> {
    let user = private_user()?;
    // SAFETY: the token SID remains inside the live user buffer for this call.
    let result = unsafe { SetSecurityInfo(file.as_raw_handle(), 1, 1,
        *(user.as_ptr().cast::<*mut c_void>()), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) };
    if result != 0 { return Err("cannot set the new private object's Windows owner".into()); }
    protect(file)?;
    check_private(file)
}
