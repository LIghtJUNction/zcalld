#!/usr/bin/env python3
"""Exercise the real installer in private temporary homes, with no network calls."""
import argparse
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import tarfile
import tempfile
import time
import unittest
import zipfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def required(target):
    windows, mac = "windows" in target, "darwin" in target
    suffix = ".exe" if windows else ""
    paths = ["bin/zcalld" + suffix, "bin/zcalld-installer" + suffix]
    for name in ("zcalld_aliases", "zcalld_policy", "zcalld_redact", "zcalld_community",
                 "zcalld_plugin_routing", "zcalld_desktop"):
        paths.append("lib/zcalld/plugins/" + (name + ".dll" if windows else
                     "lib" + name + (".dylib" if mac else ".so")))
    if windows:
        paths.append("libexec/zcalld/zcalld-desktop-notify.exe")
    if mac:
        paths.extend(("libexec/zcalld/zcalld-desktop-notify.app/Contents/Info.plist",
                      "libexec/zcalld/zcalld-desktop-notify.app/Contents/MacOS/zcalld-desktop-notify"))
    return paths


def snapshot(root):
    return {p.relative_to(root).as_posix(): digest(p.read_bytes())
            for p in root.rglob("*") if p.is_file()} if root.exists() else {}


class InstallerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.version = subprocess.check_output([str(BINARY), "--version"], text=True).strip().split()[1]
        cls.default = json.loads(subprocess.check_output([str(INSTALLER), "defaults"], text=True))
        cls.target = cls.default["target"]
        cls.core = BINARY.read_bytes()
        cls.helper = INSTALLER.read_bytes()
        cls.dlls = {}
        if os.name == "nt":
            directories = [BINARY.parent]
            if os.environ.get("LIBCURL_DLL_DIR"):
                directories.append(Path(os.environ["LIBCURL_DLL_DIR"]))
            for directory in directories:
                cls.dlls.update({"bin/" + path.name: path.read_bytes() for path in directory.glob("*.dll")})
            if not any("curl" in name.lower() for name in cls.dlls):
                raise AssertionError("native Windows fixture requires the real packaged curl DLL directory")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zci-")
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir(mode=0o700)
        self.prefix = self.root / "runtime"
        allowed = {"PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "TMP", "TEMP", "TMPDIR",
                   "PROGRAMFILES", "PROGRAMFILES(X86)", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"}
        self.env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home),
                        XDG_CONFIG_HOME=str(self.home / ".config"), XDG_DATA_HOME=str(self.home / ".local/share"),
                        LOCALAPPDATA=str(self.home / "Local"), APPDATA=str(self.home / "Roaming"))
        self.serial = 0
        self.children = []

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        self.temp.cleanup()

    def call(self, *args, ok=True, executable=None):
        result = subprocess.run([str(executable or INSTALLER), *map(str, args)], env=self.env,
                                capture_output=True, text=True, timeout=35)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        value = json.loads(result.stdout)
        self.assertEqual(value["ok"], ok)
        return value

    def bundle(self, marker="one", change=None, extras=()):
        self.serial += 1
        suffix = ".exe" if os.name == "nt" else ""
        payload = {name: b"synthetic optional component\n" for name in required(self.target)}
        payload["bin/zcalld" + suffix] = self.core
        payload["bin/zcalld-installer" + suffix] = self.helper
        payload.update(self.dlls)
        payload["share/doc/zcalld/README.md"] = marker.encode()
        payload["etc/zcalld/config.conf"] = b"# packaged example; never the active configuration\n"
        files = [dict(path=name, size=len(data), sha256=digest(data),
                      mode=0o755 if name.startswith("bin/") else 0o644)
                 for name, data in sorted(payload.items())]
        manifest = dict(schema=1, product="zcalld", version=self.version, target=self.target,
                        source="a" * 40, files=files, required=required(self.target))
        if change:
            change(manifest, payload)
        payload["INSTALL.json"] = json.dumps(manifest, separators=(",", ":")).encode()
        root = f"zcalld-{self.version}-{self.target}"
        archive = self.root / (f"fixture-{self.serial}" + (".zip" if os.name == "nt" else ".tar.gz"))
        if os.name == "nt":
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as out:
                for name, data in payload.items():
                    out.writestr(root + "/" + name, data)
                for name, kind, data in extras:
                    info = zipfile.ZipInfo(name.replace("{root}", root))
                    info.create_system = 3
                    info.external_attr = ((stat.S_IFLNK | 0o777) if kind != "file" else
                                          (stat.S_IFREG | 0o644)) << 16
                    out.writestr(info, data)
        else:
            with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as out:
                for name, data in payload.items():
                    info = tarfile.TarInfo(root + "/" + name)
                    info.size, info.mode = len(data), 0o644
                    out.addfile(info, io.BytesIO(data))
                for name, kind, data in extras:
                    info = tarfile.TarInfo(name.replace("{root}", root))
                    if kind != "file":
                        info.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
                        info.linkname = data.decode()
                        out.addfile(info)
                    else:
                        info.size = len(data)
                        out.addfile(info, io.BytesIO(data))
        return archive

    def plan(self, archive, prefix=None, sha256=None):
        self.serial += 1
        plan = self.root / f"private-plan-{self.serial}" / "plan.json"
        self.call("plan", "--archive", archive, "--sha256", sha256 or digest(archive.read_bytes()),
                  "--version", self.version, "--source", "a" * 40,
                  "--prefix", prefix or self.prefix, "--out", plan)
        return plan

    def install(self, marker="one"):
        result = self.call("apply", "--plan", self.plan(self.bundle(marker)))
        self.assertEqual(result["phase"], "committed")
        return result

    def test_complete_cohort_update_rollback_and_config_preservation(self):
        config = self.home / "personal.conf"
        config.write_bytes(b"user config canary\n")
        self.install()
        before = snapshot(self.prefix)
        result = self.install("two")
        self.assertNotEqual(snapshot(self.prefix), before)
        self.assertEqual(self.call("rollback", "--transaction", result["transaction"])["phase"], "rolled_back")
        self.assertEqual(snapshot(self.prefix), before)
        self.assertEqual(config.read_bytes(), b"user config canary\n")

    def test_running_gateway_keeps_entire_old_tree_until_exit(self):
        self.install()
        old = snapshot(self.prefix)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = self.home / "fixture.conf"
        config.write_text(f"[server]\nlisten = 127.0.0.1:{port}\nworkers = 1\nqueue = 2\ntrust_loopback = true\n"
                          "[provider fixture]\nprotocol = openai-chat\nbase_url = http://127.0.0.1:1\n"
                          "[route fixture]\nprovider = fixture\nmodel = fixture\n")
        executable = self.prefix / "bin" / ("zcalld.exe" if os.name == "nt" else "zcalld")
        child = subprocess.Popen([str(executable), "serve", "--config", str(config)], env=self.env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(child)
        deadline = time.monotonic() + 10
        while True:
            self.assertIsNone(child.poll(), "isolated gateway exited before holding its lease")
            try:
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=0.2)
                connection.request("GET", "/healthz")
                response = connection.getresponse()
                response.read(8192)
                ready = response.status == 200
                connection.close()
                if ready:
                    break
            except OSError:
                pass
            if time.monotonic() >= deadline:
                self.fail("isolated gateway did not become ready")
            time.sleep(0.02)
        result = self.call("apply", "--plan", self.plan(self.bundle("two")), "--defer")
        self.assertEqual(result["phase"], "pending_exit")
        self.assertFalse(result["committed"])
        self.assertTrue(result["helper_started"])
        self.assertEqual(snapshot(self.prefix), old)
        child.terminate()
        child.wait(timeout=10)
        deadline = time.monotonic() + 15
        while True:
            state = self.call("status", "--transaction", result["transaction"])
            if state["phase"] == "committed":
                break
            self.assertEqual(state["phase"], "pending_exit")
            if time.monotonic() >= deadline:
                self.fail("deferred installer did not complete after the owned runtime exited")
            time.sleep(0.05)
        self.assertNotEqual(snapshot(self.prefix), old)

    def test_unknown_and_modified_prefixes_are_not_overwritten(self):
        self.prefix.mkdir()
        (self.prefix / "keep.txt").write_bytes(b"not installer owned")
        plan = self.plan(self.bundle())
        self.call("apply", "--plan", plan, ok=False)
        self.assertEqual((self.prefix / "keep.txt").read_bytes(), b"not installer owned")
        shutil.rmtree(self.prefix)
        self.call("apply", "--plan", plan)
        (self.prefix / "share/doc/zcalld/README.md").write_bytes(b"edited outside installer")
        before = snapshot(self.prefix)
        self.call("apply", "--plan", self.plan(self.bundle("two")), ok=False)
        self.assertEqual(snapshot(self.prefix), before)

    def test_archive_digest_and_manifest_identity_are_checked(self):
        archive = self.bundle()
        self.call("apply", "--plan", self.plan(archive, sha256="0" * 64), ok=False)
        self.assertFalse(self.prefix.exists())
        for field, value in (("version", "99.98.97"), ("target", "wrong-platform"), ("source", "b" * 40)):
            with self.subTest(field=field):
                archive = self.bundle(change=lambda m, p: m.update({field: value}))
                self.call("apply", "--plan", self.plan(archive), ok=False)
                self.assertFalse(self.prefix.exists())

    def test_required_cohort_sizes_and_file_hashes_are_enforced(self):
        def missing(manifest, payload):
            name = required(self.target)[2]
            manifest["files"] = [x for x in manifest["files"] if x["path"] != name]
            manifest["required"].remove(name)
            payload.pop(name)
        def altered(manifest, payload):
            payload["share/doc/zcalld/README.md"] = b"tampered"
        def huge(manifest, payload):
            manifest["files"][0]["size"] = 128 * 1024 * 1024 + 1
        for change in (missing, altered, huge):
            with self.subTest(case=change.__name__):
                self.call("apply", "--plan", self.plan(self.bundle(change=change)), ok=False)
                self.assertFalse(self.prefix.exists())

    def test_archive_links_escape_names_and_duplicates_are_rejected(self):
        cases = [("../escape", "file", b"bad"), ("/absolute", "file", b"bad"),
                 ("{root}/a/../../escape", "file", b"bad"),
                 ("{root}/bin/CON.txt", "file", b"bad"),
                 ("{root}/share/doc/zcalld/README.md", "file", b"duplicate"),
                 ("{root}/link", "symlink", b"../../../escape"),
                 ("{root}/hard", "hardlink", b"bin/zcalld")]
        for extra in cases:
            with self.subTest(case=extra[0]):
                self.call("apply", "--plan", self.plan(self.bundle(extras=[extra])), ok=False)
                self.assertFalse(self.prefix.exists())
                self.assertFalse((self.root / "escape").exists())

    def test_state_privacy_validation_is_read_only(self):
        result = self.install()
        state = Path(result["transaction"]) / "state.json"
        before = state.read_bytes()
        if os.name == "nt":
            subprocess.run(["icacls", str(state), "/grant", "*S-1-1-0:R"], check=True, capture_output=True)
            acl_before = subprocess.check_output(["icacls", str(state)])
            self.call("status", "--transaction", result["transaction"], ok=False)
            self.assertEqual(subprocess.check_output(["icacls", str(state)]), acl_before)
        else:
            state.chmod(0o644)
            self.call("status", "--transaction", result["transaction"], ok=False)
            self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o644)
        self.assertEqual(state.read_bytes(), before)

    def test_core_cli_uses_the_same_installer_and_strict_arguments(self):
        result = self.call("install", "defaults", "--json", executable=BINARY)
        self.assertEqual(result["target"], self.target)
        self.assertTrue(result["full_archive"])
        self.call("install", "defaults", "--prefix", self.prefix, "--json", executable=BINARY, ok=False)
        self.call("install", "defaults", "--json", "--json", executable=BINARY, ok=False)
        self.assertFalse(self.prefix.exists())

    def test_published_archive_has_matching_manifest_and_embedded_helper(self):
        if ARCHIVE_DIR is None:
            # The local source gate still checks a real native core + helper.
            self.install()
            return
        suffix = ".zip" if os.name == "nt" else ".tar.gz"
        archive = ARCHIVE_DIR / f"zcalld-{self.version}-{self.target}{suffix}"
        root = f"zcalld-{self.version}-{self.target}"
        if suffix == ".zip":
            with zipfile.ZipFile(archive) as package:
                raw = package.read(root + "/INSTALL.json")
        else:
            with tarfile.open(archive) as package:
                raw = package.extractfile(root + "/INSTALL.json").read()
        manifest = json.loads(raw)
        self.serial += 1
        plan = self.root / f"private-plan-{self.serial}" / "plan.json"
        self.call("plan", "--archive", archive, "--sha256", digest(archive.read_bytes()),
                  "--version", self.version, "--source", manifest["source"],
                  "--prefix", self.prefix, "--out", plan)
        result = self.call("apply", "--plan", plan)
        self.assertEqual(result["phase"], "committed")
        helper = self.prefix / "bin" / ("zcalld-installer.exe" if os.name == "nt" else "zcalld-installer")
        self.assertEqual(digest(helper.read_bytes()), digest(self.helper))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    build = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release"
    parser.add_argument("--binary", type=Path, default=Path(os.environ.get("ZCALLD_TEST_BINARY", build / ("zcalld" + suffix))))
    parser.add_argument("--installer", type=Path, default=build / ("zcalld-installer" + suffix))
    parser.add_argument("--archive-dir", type=Path)
    args, rest = parser.parse_known_args()
    BINARY, INSTALLER = args.binary.resolve(), args.installer.resolve()
    ARCHIVE_DIR = args.archive_dir.resolve() if args.archive_dir else None
    unittest.main(argv=[__file__, *rest])
