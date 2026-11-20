#!/usr/bin/env python3
"""Private Linux bootstrap: real local bundles, fake managers, no running service."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


class ServiceInstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.version = subprocess.check_output([str(BINARY), "--version"], text=True).strip().split()[1]
        cls.target = json.loads(subprocess.check_output([str(INSTALLER), "defaults"], text=True))["target"]
        cls.core, cls.installer = BINARY.read_bytes(), INSTALLER.read_bytes()
        cls.analyze = shutil.which("systemd-analyze")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-service-", dir=TEMP_ROOT)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir(mode=0o700)
        self.configuration_home = self.home / "settings $value %h"
        self.configuration_home.mkdir(mode=0o700)
        self.unit_dir = self.configuration_home / "systemd/user"
        self.unit = self.unit_dir / "zcalld.service"
        self.vendor = self.root / "vendor"
        self.vendor.mkdir(mode=0o700)
        self.bin_dir = self.root / "commands"
        self.bin_dir.mkdir(mode=0o700)
        self.log, self.manager = self.root / "commands.jsonl", self.root / "manager.json"
        self.set_manager()
        proxy = """#!PYTHON
import json,sys
from pathlib import Path
root=Path(ROOT)
with (root/'commands.jsonl').open('a') as f:f.write(json.dumps({'program':Path(sys.argv[0]).name,'args':sys.argv[1:]})+'\\n')
v=json.loads((root/'manager.json').read_text())
if Path(sys.argv[0]).name=='systemd-analyze' and sys.argv[1:]==['--user','unit-paths']:
 print('\\n'.join(v['paths']));sys.exit(0)
if Path(sys.argv[0]).name=='systemctl':
 if sys.argv[1:]==['--user','is-active','zcalld.service']:print('inactive');sys.exit(3)
 if sys.argv[1:]==['--user','show','--no-pager','--property=LoadState','--property=FragmentPath','--property=DropInPaths','zcalld.service']:
  if v.get('unavailable'):sys.exit(1)
  print('LoadState='+v['state']);print('FragmentPath='+v['fragment']);print('DropInPaths='+v['drop_ins']);sys.exit(v.get('exit_code',0))
print('unexpected manager mutation',file=sys.stderr);sys.exit(97)
""".replace("PYTHON", sys.executable).replace("ROOT", repr(str(self.root)))
        for name in ("systemctl", "systemd-analyze"):
            path = self.bin_dir / name
            path.write_text(proxy, encoding="utf-8")
            path.chmod(0o700)
        self.env = {key: os.environ[key] for key in ("PATH", "LD_LIBRARY_PATH") if key in os.environ}
        self.env.update(HOME=str(self.home), XDG_CONFIG_HOME=str(self.configuration_home),
                        XDG_DATA_HOME=str(self.home / "data"), XDG_STATE_HOME=str(self.home / "state"),
                        XDG_RUNTIME_DIR=str(self.home / "runtime"), TMPDIR=str(self.root))
        self.env["PATH"] = str(self.bin_dir) + os.pathsep + self.env.get("PATH", "")
        parent = self.home / "native $value %h"
        parent.mkdir(mode=0o700)
        self.prefix = parent / "runtime"
        self.executable = self.prefix / "bin/zcalld"
        self.config = self.configuration_home / 'zcalld/config $value %h "quoted" back\\slash.conf'
        self.create_bundle()

    def tearDown(self):
        self.temp.cleanup()

    def set_manager(self, **updates):
        value = {"state": "not-found", "fragment": "", "drop_ins": "",
                 "paths": [str(self.unit_dir), str(self.vendor)]}
        value.update(updates)
        self.manager.write_text(json.dumps(value), encoding="utf-8")

    def invoke(self, program, *args, ok=True, cwd=None):
        result = subprocess.run([str(program), *map(str, args)], env=self.env,
                                cwd=cwd, text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def cli(self, *args, ok=True, config=None, program=None, cwd=None):
        return self.invoke(program or self.executable, *args, "--config",
                           config if config is not None else self.config, "--json", ok=ok, cwd=cwd)

    def create_bundle(self):
        required = ["bin/zcalld", "bin/zcalld-installer"] + [
            "lib/zcalld/plugins/lib" + name + ".so" for name in
            ("zcalld_aliases", "zcalld_policy", "zcalld_redact", "zcalld_community",
             "zcalld_plugin_routing", "zcalld_desktop")]
        payload = {name: b"synthetic component; never loaded\n" for name in required}
        payload.update({"bin/zcalld": self.core, "bin/zcalld-installer": self.installer})
        manifest = {"schema": 1, "product": "zcalld", "version": self.version,
                    "target": self.target, "source": "a" * 40, "required": required,
                    "files": [{"path": name, "size": len(data),
                               "sha256": hashlib.sha256(data).hexdigest(),
                               "mode": 0o755 if name.startswith("bin/") else 0o644}
                              for name, data in sorted(payload.items())]}
        payload["INSTALL.json"] = json.dumps(manifest).encode()
        archive = self.root / "local-fixture.tar.gz"
        with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as out:
            for name, data in payload.items():
                info = tarfile.TarInfo(f"zcalld-{self.version}-{self.target}/" + name)
                info.size, info.mode = len(data), 0o644
                out.addfile(info, io.BytesIO(data))
        plan = self.root / "plan" / "plan.json"
        self.invoke(INSTALLER, "plan", "--archive", archive, "--sha256",
                    hashlib.sha256(archive.read_bytes()).hexdigest(), "--source", "a" * 40,
                    "--version", self.version, "--prefix", self.prefix, "--out", plan)
        self.assertEqual(self.invoke(INSTALLER, "apply", "--plan", plan)["phase"], "committed")

    def configure(self):
        self.config.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        self.config.write_text(
            "[server]\nlisten=127.0.0.1:1\nunix_socket=none\n"
            "[provider fixture]\nbase_url=http://127.0.0.1:1/v1\nprotocol=chat\nauth=none\n"
            "[route fixture]\nprovider=fixture\nmodel=fixture\n", encoding="utf-8")
        self.config.chmod(0o600)

    def assert_no_manager_mutation(self):
        if self.log.exists():
            for row in map(json.loads, self.log.read_text().splitlines()):
                self.assertIn(row["args"][1], ("show", "is-active", "unit-paths"))

    def test_status_distinguishes_missing_from_existing_invalid_nodes(self):
        value = self.cli("status")
        self.assertFalse(value["configuration_exists"])
        self.assertFalse(value["configured"])
        self.assertEqual(value["configuration_path"], str(self.config))
        self.assertTrue(value["service_install_supported"])
        self.assertFalse(value["service_installed"])
        self.config.parent.mkdir(parents=True, mode=0o700)
        for kind in ("invalid", "directory", "dangling"):
            with self.subTest(kind=kind):
                if kind == "invalid":
                    self.config.write_bytes(b"invalid configuration canary\n")
                elif kind == "directory":
                    self.config.mkdir()
                else:
                    self.config.symlink_to(self.root / "missing-target")
                result = self.cli("status")
                self.assertTrue(result["configuration_exists"])
                self.assertFalse(result["configured"])
                if kind == "directory":
                    self.config.rmdir()
                else:
                    self.config.unlink()
        parent_file = self.root / "not-a-directory"
        parent_file.write_bytes(b"keep")
        self.assertTrue(self.cli("status", config=parent_file / "config")["configuration_exists"])
        relative = Path("relative-missing.conf")
        self.assertEqual(self.cli("status", config=relative, cwd=self.home)["configuration_path"],
                         str(self.home / relative))
        self.assert_no_manager_mutation()

    def test_explicit_install_is_atomic_private_idempotent_and_does_not_start(self):
        self.configure()
        before = self.config.read_bytes()
        first = self.cli("service", "install")
        self.assertTrue(first["created"])
        self.assertFalse(first["service_started"])
        self.assertEqual(first["unit_path"], str(self.unit))
        text = self.unit.read_text()
        self.assertIn("ExecStart=:\"", text)
        self.assertIn("native $value %%h", text)
        self.assertIn('config $value %%h \\"quoted\\" back\\\\slash.conf', text)
        self.assertEqual(self.unit.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.unit.stat().st_nlink, 1)
        identity = (self.unit.stat().st_ino, self.unit.stat().st_mtime_ns)
        self.assertFalse(self.cli("service", "install")["created"])
        self.assertEqual(identity, (self.unit.stat().st_ino, self.unit.stat().st_mtime_ns))
        self.assertEqual(self.config.read_bytes(), before)
        self.assertEqual(list(self.unit_dir.glob(".zcalld-unit-*")), [])
        self.assert_no_manager_mutation()
        if self.analyze:
            # Offline verification only; it never executes ExecStart.
            result = subprocess.run([self.analyze, "verify", "--man=no", str(self.unit)],
                                    env=self.env, capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_existing_foreign_unit_and_symlink_are_never_replaced(self):
        self.configure()
        self.unit_dir.mkdir(parents=True, mode=0o700)
        canary = b"existing installation must remain\n"
        self.unit.write_bytes(canary)
        self.cli("service", "install", ok=False)
        self.assertEqual(self.unit.read_bytes(), canary)
        self.unit.unlink()
        other = self.root / "other-unit"
        other.write_bytes(canary)
        self.unit.symlink_to(other)
        self.cli("service", "install", ok=False)
        self.assertTrue(self.unit.is_symlink())
        self.assertEqual(other.read_bytes(), canary)
        self.assert_no_manager_mutation()

    def test_vendor_mask_dropin_and_unavailable_manager_block_install(self):
        self.configure()
        cases = (
            ("vendor", {}),
            ("type-dropin", {}),
            ("masked", {"state": "masked", "fragment": "/dev/null"}),
            ("dropin", {"drop_ins": "/synthetic/override.conf"}),
            ("unavailable", {"unavailable": True}),
            ("other-loaded", {"state": "loaded", "fragment": "/synthetic/another.service"}),
        )
        for name, options in cases:
            with self.subTest(kind=name):
                self.set_manager(**options)
                if name == "vendor":
                    (self.vendor / "zcalld.service").write_bytes(b"package-owned")
                if name == "type-dropin":
                    (self.vendor / "service.d").mkdir()
                    (self.vendor / "service.d/global.conf").write_bytes(b"[Service]\nEnvironment=KEEP=1\n")
                self.cli("service", "install", ok=False)
                self.assertFalse(self.unit.exists())
                if name == "vendor":
                    self.assertEqual((self.vendor / "zcalld.service").read_bytes(), b"package-owned")
                    (self.vendor / "zcalld.service").unlink()
                if name == "type-dropin":
                    self.assertEqual((self.vendor / "service.d/global.conf").read_bytes(),
                                     b"[Service]\nEnvironment=KEEP=1\n")
                    (self.vendor / "service.d/global.conf").unlink()
                    (self.vendor / "service.d").rmdir()
        self.assert_no_manager_mutation()

    def test_tampered_bundle_and_unowned_copy_cannot_register_service(self):
        self.configure()
        copied = self.root / "bare"
        shutil.copy2(self.executable, copied)
        self.assertFalse(self.cli("status", program=copied)["service_install_supported"])
        self.cli("service", "install", program=copied, ok=False)
        plugin = self.prefix / "lib/zcalld/plugins/libzcalld_aliases.so"
        plugin.write_bytes(b"tampered")
        self.cli("service", "install", ok=False)
        self.assertFalse(self.unit.exists())
        self.assert_no_manager_mutation()

    def test_existing_directory_links_permissions_and_system_scope_are_preserved(self):
        self.configure()
        self.unit_dir.parent.mkdir(mode=0o700)
        other = self.root / "different-directory"
        other.mkdir(mode=0o700)
        self.unit_dir.symlink_to(other, target_is_directory=True)
        self.cli("service", "install", ok=False)
        self.assertEqual(list(other.iterdir()), [])
        self.unit_dir.unlink()
        self.unit_dir.mkdir(mode=0o777)
        self.unit_dir.chmod(0o777)
        self.cli("service", "install", ok=False)
        self.assertEqual(self.unit_dir.stat().st_mode & 0o777, 0o777)
        self.assertFalse(self.unit.exists())
        self.cli("service", "install", "--system", ok=False)
        self.assert_no_manager_mutation()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--installer", required=True, type=Path)
    parser.add_argument("--temp-root", type=Path)
    args, remaining = parser.parse_known_args()
    if not sys.platform.startswith("linux"):
        parser.error("this focused fixture covers the Linux user service action")
    global BINARY, INSTALLER, TEMP_ROOT
    BINARY, INSTALLER = args.binary.resolve(), args.installer.resolve()
    TEMP_ROOT = str(args.temp_root.resolve()) if args.temp_root else None
    unittest.main(argv=[__file__, *remaining])


if __name__ == "__main__":
    main()
