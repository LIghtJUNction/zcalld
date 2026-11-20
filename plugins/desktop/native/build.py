#!/usr/bin/env python3
"""Build optional OS-identity notification helpers; never register or notify."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess


def batch_environment(comspec, batch, env):
    """Run a trusted batch with cmd quoting, not MSVCRT argv quoting."""
    # Popen(list) uses list2cmdline on Windows, escaping embedded quotes as \".
    # cmd.exe does not use MSVCRT parsing. Give /s /c one explicitly quoted
    # command string; /s removes the outer pair and preserves CALL's path pair.
    if any(c in str(comspec) + str(batch) for c in '\r\n\0"&|<>^%'):
        raise SystemExit("installed MSVC command path contains invalid characters")
    command = f'"{comspec}" /d /u /s /c "call "{batch}" -no_logo -arch=x64 -host_arch=x64 >nul && set"'
    configured = subprocess.run(command, executable=str(comspec), env=env,
                                check=False, capture_output=True, timeout=30)
    if configured.returncode:
        # Never log stdout: on a late failure it may contain the full SET dump.
        # Preserve bounded stderr so native CI can diagnose VsDevCmd failures.
        raw = configured.stderr[:16384]
        codec = "utf-16-le" if b"\0" in raw else "utf-8"
        diagnostic = raw.decode(codec, errors="replace").strip()[:8192]
        raise SystemExit(f"MSVC environment failed (exit {configured.returncode}): "
                         + (diagnostic or "no stderr was produced"))
    try:
        return configured.stdout.decode("utf-16-le", errors="strict")
    except UnicodeError as error:
        raise SystemExit("MSVC environment returned invalid UTF-16 output") from error


def msvc_environment():
    """Find the installed compiler; never install or download a toolchain."""
    env = {key.upper(): value for key, value in os.environ.items()}
    if shutil.which("cl", path=env.get("PATH", "")):
        return env
    installer = Path(env.get("PROGRAMFILES(X86)", r"C:\Program Files (x86)")) / "Microsoft Visual Studio" / "Installer" / "vswhere.exe"
    if not installer.is_file():
        raise SystemExit("MSVC compiler is unavailable; install the C++ build tools first")
    found = subprocess.run([str(installer), "-latest", "-products", "*", "-requires",
                            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-find", r"Common7\Tools\VsDevCmd.bat"],
                           check=True, capture_output=True, text=True, timeout=15, env=env)
    paths = [line.strip() for line in found.stdout.splitlines() if line.strip()]
    if len(paths) != 1 or not Path(paths[0]).is_file() or any(c in paths[0] for c in '\r\n"&|<>^%'):
        raise SystemExit("installed MSVC environment could not be identified")
    comspec = env.get("COMSPEC", str(Path(env.get("SYSTEMROOT", r"C:\Windows")) / "System32" / "cmd.exe"))
    # VsDevCmd is the trusted installed batch entry point. /u makes SET output
    # UTF-16 regardless of the user's code page; never print that environment.
    configured = batch_environment(comspec, paths[0], env)
    for line in configured.splitlines():
        key, sep, value = line.partition("=")
        if sep and key and not any(c in key for c in "\r\n\0"):
            env[key.upper()] = value
    if not shutil.which("cl", path=env.get("PATH", "")):
        raise SystemExit("installed MSVC environment did not provide cl.exe")
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    here = Path(__file__).resolve().parent
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=True)
    system = platform.system()
    if system == "Darwin":
        bundle = out / "zcalld-desktop-notify.app" / "Contents"
        (bundle / "MacOS").mkdir(parents=True, exist_ok=True)
        binary = bundle / "MacOS" / "zcalld-desktop-notify"
        subprocess.run(["clang", "-O2", "-fobjc-arc", "-fblocks", "-Wall", "-Wextra", "-Werror", "-mmacosx-version-min=12.0",
                        "-framework", "Foundation", "-framework", "UserNotifications", str(here / "macos-notify.m"), "-o", str(binary)], check=True)
        shutil.copyfile(here / "Info.plist", bundle / "Info.plist")
    elif system == "Windows":
        binary = out / "zcalld-desktop-notify.exe"
        # Windows CreateProcess does not search env["PATH"] for argv[0].
        # Resolve against the discovered toolchain environment before spawning.
        compiler_env = msvc_environment()
        compiler = shutil.which("cl", path=compiler_env.get("PATH", ""))
        if compiler is None:
            raise SystemExit("installed MSVC compiler is unavailable")
        # The Windows SDK provides C++/WinRT; no downloaded SDK/package is used.
        subprocess.run([compiler, "/nologo", "/std:c++20", "/EHsc", "/O2", "/MT", "/W4", "/WX", "/DUNICODE", "/D_UNICODE",
                        str(here / "windows-notify.cpp"), "/Fe:" + str(binary), "/Fo:" + str(out / "notify.obj"),
                        "/link", "windowsapp.lib", "runtimeobject.lib", "ole32.lib", "shell32.lib", "propsys.lib"], check=True, cwd=out, env=compiler_env)
    else:
        print(json.dumps({"platform": system, "helper_required": False}))
        return
    if args.self_test:
        result = subprocess.run([str(binary), "--self-test"], check=True, capture_output=True, text=True, timeout=10,
                                env={k: v for k, v in os.environ.items() if k.upper() in {"SYSTEMROOT", "WINDIR", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "HOME", "TMPDIR"}})
        if result.stdout != "self-test passed\n":
            raise SystemExit("native helper self-test did not confirm success")
    print(json.dumps({"platform": system, "binary": str(binary), "registered": False, "notifications_sent": 0}))


if __name__ == "__main__":
    main()

