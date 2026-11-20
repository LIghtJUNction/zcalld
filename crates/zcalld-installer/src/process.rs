use std::{ffi::OsStr, io::{self, Read}, path::PathBuf, process::{Command, Stdio}, thread, time::{Duration, Instant}};

pub fn executable(name: &str) -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").ok_or_else(|| format!("required system program is unavailable: {name}"))?;
    for directory in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
        let candidate = directory.join(if cfg!(windows) { format!("{name}.exe") } else { name.into() });
        if candidate.is_file() { return candidate.canonicalize().map_err(|_| "cannot resolve system program".into()); }
    }
    Err(format!("required system program is unavailable: {name}"))
}

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd=Command::new(program);
    cmd.env_clear();
    for key in ["PATH","SystemRoot","SYSTEMROOT","WINDIR","COMSPEC","ProgramFiles","ProgramFiles(x86)","TMP","TEMP","TMPDIR",
        "SSL_CERT_FILE","SSL_CERT_DIR","CURL_CA_BUNDLE","HTTPS_PROXY","HTTP_PROXY","ALL_PROXY","NO_PROXY",
        "https_proxy","http_proxy","all_proxy","no_proxy"] {
        if let Some(value)=std::env::var_os(key) { cmd.env(key,value); }
    }
    cmd.env("LC_ALL","C").env("TZ","UTC").stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000 | 0x00000200);
    }
    cmd
}

// Read the owned pipe without a blocking reader thread: a child retaining stdout
// cannot turn a command deadline into an unbounded thread join.
#[cfg(unix)]
fn make_nonblocking(pipe: &std::process::ChildStdout) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" { fn fcntl(fd:i32,command:i32,...)->i32; }
    #[cfg(target_os="macos")] const NONBLOCK:i32=4;
    #[cfg(not(target_os="macos"))] const NONBLOCK:i32=2048;
    // SAFETY: fcntl only changes flags on our live, exclusively owned pipe.
    unsafe { let flags=fcntl(pipe.as_raw_fd(),3); if flags<0 || fcntl(pipe.as_raw_fd(),4,flags|NONBLOCK)<0 { return Err("cannot bound installer child output".into()); } }
    Ok(())
}
#[cfg(windows)] fn make_nonblocking(_: &std::process::ChildStdout)->Result<(),String>{Ok(())}

#[cfg(unix)]
fn read_pipe(pipe:&mut std::process::ChildStdout,buffer:&mut [u8])->io::Result<usize>{pipe.read(buffer)}
#[cfg(windows)]
fn read_pipe(pipe:&mut std::process::ChildStdout,buffer:&mut [u8])->io::Result<usize>{
    use std::{ffi::c_void,os::windows::io::AsRawHandle,ptr};
    #[link(name="kernel32")] unsafe extern "system" {
        fn PeekNamedPipe(handle:*mut c_void,buffer:*mut c_void,size:u32,read:*mut u32,available:*mut u32,left:*mut u32)->i32;
    }
    let mut available=0;
    // SAFETY: PeekNamedPipe inspects the live anonymous pipe without consuming it.
    if unsafe{PeekNamedPipe(pipe.as_raw_handle(),ptr::null_mut(),0,ptr::null_mut(),&mut available,ptr::null_mut())}==0 {
        let error=io::Error::last_os_error();
        if matches!(error.raw_os_error(),Some(109|232)){return Ok(0);} return Err(error);
    }
    if available==0 { return Err(io::ErrorKind::WouldBlock.into()); }
    let n=(available as usize).min(buffer.len());pipe.read(&mut buffer[..n])
}

pub fn stream(mut cmd: Command, max:u64, timeout:Duration, mut consume:impl FnMut(&[u8])->Result<(),String>)->Result<(),String>{
    cmd.stdout(Stdio::piped());
    let mut child=cmd.spawn().map_err(|_|"cannot start required installer program")?;
    let mut pipe=child.stdout.take().ok_or("installer child output is unavailable")?;
    let result=(||{
        make_nonblocking(&pipe)?;
        let deadline=Instant::now()+timeout;let mut total=0u64;let mut buffer=[0u8;16384];
        loop {
            if Instant::now()>=deadline{return Err("installer program exceeded its deadline".into());}
            match read_pipe(&mut pipe,&mut buffer){
                Ok(0)=>break,
                Ok(n)=>{
                    total=total.checked_add(n as u64).ok_or("installer output exceeds its limit")?;
                    if total>max{return Err("installer output exceeds its limit".into());}
                    consume(&buffer[..n])?;
                }
                Err(e) if e.kind()==io::ErrorKind::WouldBlock => {thread::sleep(Duration::from_millis(4));}
                Err(e) if e.kind()==io::ErrorKind::Interrupted=>{}
                Err(_)=>return Err("cannot read installer program output".into()),
            }
        }
        loop {
            match child.try_wait().map_err(|_|"cannot inspect installer program")? {
                Some(status) if status.success()=>return Ok(()),
                Some(_)=>return Err("installer program failed".into()),
                None if Instant::now()<deadline=>thread::sleep(Duration::from_millis(4)),
                None=>return Err("installer program exceeded its deadline".into()),
            }
        }
    })();
    if result.is_err(){let _=child.kill();let _=child.wait();}
    result
}

pub fn capture(cmd:Command,max:usize,timeout:Duration)->Result<Vec<u8>,String>{
    let mut bytes=Vec::new();stream(cmd,max as u64,timeout,|chunk|{bytes.extend_from_slice(chunk);Ok(())})?;Ok(bytes)
}
