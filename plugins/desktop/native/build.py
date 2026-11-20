#!/usr/bin/env python3
"""Build optional OS-identity notification helpers; never register or notify."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess


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
    command = f'call "{paths[0]}" -no_logo -arch=x64 -host_arch=x64 >nul && set'
    configured = subprocess.run([comspec, "/d", "/u", "/s", "/c", command], env=env,
                                check=True, capture_output=True, encoding="utf-16-le", errors="strict", timeout=30)
    for line in configured.stdout.splitlines():
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
        # The Windows SDK provides C++/WinRT; no downloaded SDK/package is used.
        subprocess.run(["cl", "/nologo", "/std:c++20", "/EHsc", "/O2", "/MT", "/W4", "/WX", "/DUNICODE", "/D_UNICODE",
                        str(here / "windows-notify.cpp"), "/Fe:" + str(binary), "/Fo:" + str(out / "notify.obj"),
                        "/link", "windowsapp.lib", "runtimeobject.lib", "ole32.lib", "shell32.lib", "propsys.lib"], check=True, cwd=out, env=msvc_environment())
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
