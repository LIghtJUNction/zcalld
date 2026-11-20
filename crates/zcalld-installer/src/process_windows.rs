use std::{
    ffi::{OsStr, c_void},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    path::Path,
    process::Command,
    ptr,
};

const MAX_COMMAND_LINE: usize = 32767;

#[derive(Default)]
#[repr(C)]
struct StartupInfo {
    size: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved_bytes: u16,
    reserved_data: *mut u8,
    input: *mut c_void,
    output: *mut c_void,
    error: *mut c_void,
}

#[derive(Default)]
#[repr(C)]
struct ProcessInformation {
    process: *mut c_void,
    thread: *mut c_void,
    process_id: u32,
    thread_id: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateProcessW(
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: i32,
        flags: u32,
        environment: *mut c_void,
        directory: *const u16,
        startup: *mut StartupInfo,
        information: *mut ProcessInformation,
    ) -> i32;
}

fn wide(value: &OsStr) -> Result<Vec<u16>, String> {
    let value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err("deferred installer argument or environment contains NUL".into());
    }
    Ok(value)
}

fn append_argument(out: &mut Vec<u16>, argument: &OsStr) -> Result<(), String> {
    let argument = wide(argument)?;
    if argument.len() > MAX_COMMAND_LINE - 3 {
        return Err("deferred installer command line is too long".into());
    }
    out.push(b'"' as u16);
    let mut slashes = 0;
    for unit in argument {
        if unit == b'\\' as u16 {
            slashes += 1;
        } else {
            // CRT quoting: double backslashes before a quote, then escape it.
            let count = if unit == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            };
            out.extend(std::iter::repeat_n(b'\\' as u16, count));
            out.push(unit);
            slashes = 0;
        }
    }
    // Trailing backslashes precede our closing quote and must also be doubled.
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    if out.len() >= MAX_COMMAND_LINE {
        return Err("deferred installer command line is too long".into());
    }
    Ok(())
}

fn command_line(command: &Command) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for argument in std::iter::once(command.get_program()).chain(command.get_args()) {
        if !out.is_empty() {
            out.push(b' ' as u16);
        }
        append_argument(&mut out, argument)?;
    }
    out.push(0);
    Ok(out)
}

fn environment(command: &Command) -> Result<Vec<u16>, String> {
    let mut entries = Vec::new();
    // The caller uses process::command's env_clear + ASCII-key whitelist. Do
    // not inherit the ambient environment or add drive-current-directory keys.
    for (key, value) in command.get_envs() {
        let Some(value) = value else { continue };
        let key = key
            .to_str()
            .filter(|s| !s.is_empty() && s.is_ascii() && !s.contains(['=', '\0']))
            .ok_or("invalid deferred installer environment name")?;
        entries.push((key, wide(value)?));
    }
    entries.sort_by_cached_key(|(key, _)| key.to_ascii_uppercase());
    let mut out = Vec::new();
    for (key, value) in entries {
        out.extend(key.encode_utf16());
        out.push(b'=' as u16);
        out.extend(value);
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    Ok(out)
}

pub(super) fn spawn(command: &Command) -> Result<(), String> {
    let mut application = wide(command.get_program())?;
    if !Path::new(command.get_program()).is_absolute() || application.contains(&(b'"' as u16)) {
        return Err("deferred installer executable must be an absolute path".into());
    }
    application.push(0);
    let mut command_line = command_line(command)?;
    let mut environment = environment(command)?;
    let mut startup = StartupInfo {
        size: size_of::<StartupInfo>() as u32,
        ..Default::default()
    };
    let mut information = ProcessInformation::default();
    // This is only the verified, fixed-argument deferred resume helper. Rust's
    // Command currently enables handle inheritance even with null stdio; that
    // can retain a caller's captured pipe until the 120-second helper exits.
    // FALSE prevents all such inheritance without changing any global handles.
    // No STARTF_USESTDHANDLES: a windowless helper has no standard handles.
    // SAFETY: all UTF-16 buffers live through the call; command_line is writable,
    // environment ends in two NULs, and both repr(C) structures match Win32.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            0x08000000 | 0x00000200 | 0x00000400,
            environment.as_mut_ptr().cast(),
            ptr::null(),
            &mut startup,
            &mut information,
        )
    };
    if created == 0 {
        return Err("cannot start verified deferred installer".into());
    }
    // SAFETY: success returns two owned, non-inheritable handles. Close both on
    // return without waiting for or terminating the independent resume process.
    let _process = unsafe { OwnedHandle::from_raw_handle(information.process) };
    let _thread = unsafe { OwnedHandle::from_raw_handle(information.thread) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_preserve_spaces_quotes_backslashes_and_utf16_boundaries() {
        for (argument, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            (r#"a\"b"#, r#""a\\\"b""#),
            ("C:\\路径 with spaces\\", r#""C:\路径 with spaces\\""#),
            ("emoji 🦀", "\"emoji 🦀\""),
        ] {
            let mut out = Vec::new();
            append_argument(&mut out, OsStr::new(argument)).unwrap();
            assert_eq!(String::from_utf16(&out).unwrap(), expected);
        }
        let mut cmd = Command::new(r"C:\Program Files\工具\runner.exe");
        cmd.args([
            "resume",
            "--transaction",
            r"C:\用户\pending\",
            "--wait-ms",
            "120000",
        ]);
        assert_eq!(
            String::from_utf16(&command_line(&cmd).unwrap()).unwrap(),
            concat!(
                r#""C:\Program Files\工具\runner.exe" "resume" "--transaction" "C:\用户\pending\\" "--wait-ms" "120000""#,
                "\0"
            )
        );
        assert_eq!(
            command_line(&Command::new("x".repeat(MAX_COMMAND_LINE - 3)))
                .unwrap()
                .len(),
            MAX_COMMAND_LINE
        );
        assert!(command_line(&Command::new("x".repeat(MAX_COMMAND_LINE - 2))).is_err());
        assert!(command_line(Command::new("runner.exe").arg("bad\0argument")).is_err());
    }

    #[test]
    fn environment_keeps_only_explicit_sorted_unicode_entries_and_double_nul() {
        let mut cmd = Command::new("runner.exe");
        cmd.env_clear()
            .env("z_key", "末🦀")
            .env("alpha", "a=b")
            .env("Path", r"C:\Program Files\bin")
            .env_remove("removed");
        assert_eq!(
            String::from_utf16(&environment(&cmd).unwrap()).unwrap(),
            "alpha=a=b\0Path=C:\\Program Files\\bin\0z_key=末🦀\0\0"
        );
        cmd.env_clear();
        assert_eq!(environment(&cmd).unwrap(), [0, 0]);
        cmd.env("BAD", "bad\0value");
        assert!(environment(&cmd).is_err());
        cmd.env_clear().env("BAD=NAME", "value");
        assert!(environment(&cmd).is_err());
    }
}
