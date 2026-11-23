//! Consumer adapter for the shared zsys Windows private file contract.
use std::{ffi::c_void, ptr};
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
}
pub fn random(bytes: &mut [u8; 32]) -> Result<(), String> {
    // SAFETY: the system-preferred RNG writes exactly the live buffer length.
    if unsafe { BCryptGenRandom(ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) } == 0 {
        Ok(())
    } else {
        Err("Windows system random source is unavailable".into())
    }
}

/// DACL-only reprotection; existing objects do not require WRITE_OWNER.
pub fn protect(file: &std::fs::File) -> Result<(), String> {
    zsys::windows::privacy::protect(
        file,
        zsys::windows::privacy::DirectoryInheritance::FilesAndDirectories,
    )
    .map_err(|error| error.to_string())
}

/// Read existing security without taking ownership or rewriting the DACL.
#[allow(dead_code)] // This source also serves callers which only create credentials.
pub fn check_private(file: &std::fs::File) -> Result<(), String> {
    zsys::windows::privacy::check_private(file).map_err(|error| error.to_string())
}

/// The caller must have exclusively created this object, before payload writes.
#[allow(dead_code)] // Only the installer initializes newly created directory cohorts.
pub fn protect_created(file: &std::fs::File) -> Result<(), String> {
    zsys::windows::privacy::protect_created(
        file,
        zsys::windows::privacy::DirectoryInheritance::FilesAndDirectories,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent native fixture construction and snapshots retained from
    // the pre-migration consumer. No production permission checker is copied.
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
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

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: *mut c_void,
            kind: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn GetAclInformation(
            acl: *const c_void,
            information: *mut c_void,
            size: u32,
            class: u32,
        ) -> i32;
        fn GetAce(acl: *const c_void, index: u32, ace: *mut *mut c_void) -> i32;
        fn EqualSid(a: *const c_void, b: *const c_void) -> i32;
        fn CreateWellKnownSid(
            kind: u32,
            domain: *const c_void,
            sid: *mut c_void,
            size: *mut u32,
        ) -> i32;
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
    fn private_user() -> Result<Vec<usize>, String> {
        let error = || "cannot inspect the current Windows account".to_string();
        let mut token = ptr::null_mut();
        // SAFETY: all output buffers are owned and sized according to Win32 results.
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
            Ok(storage)
        }
    }

    use std::{fs, fs::File, os::windows::fs::OpenOptionsExt, path::PathBuf};

    struct Directory {
        path: PathBuf,
        file: File,
    }

    impl Directory {
        fn new() -> Self {
            let mut bytes = [0; 32];
            random(&mut bytes).unwrap();
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let path = std::env::temp_dir().join(format!("zcalld-directory-acl-{id}"));
            fs::create_dir(&path).unwrap();
            let file = File::options()
                .access_mode(0x0002_0000 | 0x0004_0000 | 0x0008_0000)
                .custom_flags(0x0200_0000)
                .open(&path)
                .unwrap();
            protect_created(&file).unwrap();
            Self { path, file }
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = protect(&self.file);
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // Seed known native ACLs independently of protect(), including the old
    // non-inheriting format and deliberately invalid inspection inputs.
    fn set_acl(file: &File, flags: &str, second: &str) {
        let user = private_user().unwrap();
        // SAFETY: token/descriptor allocations stay alive through the Win32 calls.
        unsafe {
            let mut raw = ptr::null_mut();
            assert_ne!(
                ConvertSidToStringSidW(*(user.as_ptr().cast::<*mut c_void>()), &mut raw),
                0
            );
            let memory = Local(raw.cast());
            let mut length = 0;
            while length < 256 && *raw.add(length) != 0 {
                length += 1;
            }
            assert!(length < 256);
            let sid = String::from_utf16(std::slice::from_raw_parts(raw, length)).unwrap();
            drop(memory);
            let text = format!("D:P(A;{flags};FA;;;{sid})(A;{flags};FA;;;{second})")
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                ),
                0
            );
            let descriptor = Local(descriptor);
            let mut present = 0;
            let mut defaulted = 0;
            let mut acl = ptr::null_mut();
            assert_ne!(
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted),
                0
            );
            assert_ne!(present, 0);
            assert!(!acl.is_null());
            assert_eq!(
                SetSecurityInfo(
                    file.as_raw_handle(),
                    1,
                    0x80000004,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null_mut(),
                ),
                0
            );
        }
    }

    // Validate inherited children's effective grants without weakening the
    // production checker, which intentionally requires a protected, explicit ACL.
    fn assert_private_grants(file: &File, flags: u8) {
        #[repr(C)]
        struct AclSize {
            count: u32,
            used: u32,
            free: u32,
        }
        let user = private_user().unwrap();
        // SAFETY: GetSecurityInfo owns the descriptor; all ACEs stay inside it.
        unsafe {
            let mut acl = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    1,
                    4,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut acl,
                    ptr::null_mut(),
                    &mut descriptor,
                ),
                0
            );
            assert!(!descriptor.is_null() && !acl.is_null());
            let _descriptor = Local(descriptor);
            let mut size = AclSize {
                count: 0,
                used: 0,
                free: 0,
            };
            assert_ne!(
                GetAclInformation(
                    acl,
                    (&mut size as *mut AclSize).cast(),
                    std::mem::size_of::<AclSize>() as u32,
                    2,
                ),
                0
            );
            assert_eq!(size.count, 2);
            let mut system = [0usize; 16];
            let mut system_size = std::mem::size_of_val(&system) as u32;
            assert_ne!(
                CreateWellKnownSid(
                    22,
                    ptr::null(),
                    system.as_mut_ptr().cast(),
                    &mut system_size
                ),
                0
            );
            let user_sid = *(user.as_ptr().cast::<*mut c_void>());
            let mut found_user = false;
            let mut found_system = false;
            for index in 0..size.count {
                let mut ace = ptr::null_mut();
                assert_ne!(GetAce(acl, index, &mut ace), 0);
                assert!(!ace.is_null());
                let bytes = ace.cast::<u8>();
                assert_eq!(*bytes, 0); // ACCESS_ALLOWED_ACE_TYPE
                assert_eq!(*bytes.add(1), flags);
                assert_eq!(ptr::read_unaligned(bytes.add(4).cast::<u32>()), 0x001f_01ff);
                let sid = bytes.add(8).cast::<c_void>();
                let is_user = EqualSid(sid, user_sid) != 0;
                let is_system = EqualSid(sid, system.as_ptr().cast()) != 0;
                assert!(is_user || is_system);
                found_user |= is_user;
                found_system |= is_system;
            }
            assert!(found_user && found_system);
        }
    }

    #[test]
    fn directory_reprotection_preserves_inherited_children_and_new_private_files() {
        let directory = Directory::new();
        set_acl(&directory.file, "OICI", "SY");
        // Match desktop::private_dir and the installer's read-only check_dir.
        let desktop = File::options()
            .access_mode(0x0002_0000 | 0x0004_0000)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(&directory.path)
            .unwrap();
        let read_only = File::options()
            .access_mode(0x0002_0000)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(&directory.path)
            .unwrap();
        let existing = directory.path.join("existing.json");
        let child_dir = directory.path.join("child");
        fs::create_dir(&child_dir).unwrap();
        let nested = child_dir.join("existing.json");
        fs::write(&existing, b"old").unwrap();
        fs::write(&nested, b"nested").unwrap();
        assert_private_grants(&File::open(&existing).unwrap(), 0x10);
        assert_private_grants(&File::open(&nested).unwrap(), 0x10);
        for _ in 0..2 {
            protect(&desktop).unwrap();
            check_private(&read_only).unwrap();
            assert_eq!(fs::read(&existing).unwrap(), b"old");
            assert_eq!(fs::read(&nested).unwrap(), b"nested");
            fs::write(&existing, b"old").unwrap();
            fs::write(&nested, b"nested").unwrap();
        }
        let inherited_directory = File::options()
            .access_mode(0x0002_0000)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(&child_dir)
            .unwrap();
        assert_private_grants(&inherited_directory, 0x13);
        assert!(check_private(&inherited_directory).is_err());
        for parent in [&directory.path, &child_dir] {
            let new = parent.join("new.json");
            fs::write(&new, b"new").unwrap();
            assert_eq!(fs::read(&new).unwrap(), b"new");
            let file = File::open(&new).unwrap();
            assert_private_grants(&file, 0x10);
            assert!(check_private(&file).is_err());
        }
    }

    #[test]
    fn directory_inspection_accepts_legacy_acl_and_rejects_other_flags_and_subjects() {
        let directory = Directory::new();
        for flags in ["", "OICI"] {
            set_acl(&directory.file, flags, "SY");
            check_private(&directory.file).unwrap();
        }
        for flags in ["OI", "CI", "OICINP", "OICIIO"] {
            set_acl(&directory.file, flags, "SY");
            assert!(check_private(&directory.file).is_err(), "accepted {flags}");
        }
        // Protected assignment makes supplied ID ACEs explicit. Check the stored
        // ACL; actual inherited child ACEs are rejected in the propagation test.
        set_acl(&directory.file, "ID", "SY");
        assert_private_grants(&directory.file, 0);
        check_private(&directory.file).unwrap();
        set_acl(&directory.file, "OICI", "WD");
        assert!(check_private(&directory.file).is_err());
    }

    #[test]
    fn regular_file_protection_keeps_non_inheriting_explicit_private_acl() {
        let directory = Directory::new();
        let path = directory.path.join("secret.json");
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(0x8000_0000 | 0x4000_0000 | 0x0004_0000 | 0x0008_0000)
            .open(&path)
            .unwrap();
        protect_created(&file).unwrap();
        assert_private_grants(&file, 0);
        check_private(&file).unwrap();
        set_acl(&file, "OICI", "SY");
        // Noncontainer assignment may normalize the requested propagation flags.
        // Require an explicit readback of the expected ACL, not input SDDL alone.
        assert_private_grants(&file, 0);
        check_private(&file).unwrap();
        protect(&file).unwrap();
        check_private(&file).unwrap();
        assert_private_grants(&file, 0);
        fs::write(&path, b"private").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"private");
    }
}
