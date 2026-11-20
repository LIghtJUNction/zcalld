#!/usr/bin/env python3
"""Caller-key CLI fixtures; no service startup or provider requests."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

parser = argparse.ArgumentParser()
parser.add_argument("--binary", default=os.environ.get("ZCALLD_TEST_BINARY", "target/debug/zcalld"))
OPTIONS, TEST_ARGS = parser.parse_known_args()
BINARY = str(Path(OPTIONS.binary).resolve())


class Keys(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zk-")
        self.home = Path(self.temporary.name).resolve()
        self.home.chmod(0o700)
        self.config = self.home / "config.conf"
        self.config.write_bytes(
            b"# Retain this comment\n[server]\nlisten = 127.0.0.1:19415\n"
            b"[provider local]\nbase_url = http://127.0.0.1:9\nprotocol = responses\n"
            b"[route demo]\nprovider = local\nmodel = demo\n"
        )
        self.config.chmod(0o600)
        self.keyfile = self.home / "caller-keys.json"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("ZCALLD_")}
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home),
                        XDG_CONFIG_HOME=str(self.home / "xdg"), APPDATA=str(self.home / "appdata"))
        self.addCleanup(self.temporary.cleanup)

    def invoke(self, *args, secret=None, ok=True):
        if isinstance(secret, str):
            secret = secret.encode()
        result = subprocess.run(
            [BINARY, "--config", str(self.config), "keys", *args, "--json"],
            input=secret, capture_output=True, env=self.env, timeout=10
        )
        if secret and len(secret) > 3:
            canary = secret.strip().split(b"\n", 1)[0]
            self.assertNotIn(canary, result.stdout + result.stderr)
        self.assertEqual(result.returncode == 0, ok, (result.stdout, result.stderr))
        return json.loads(result.stdout)

    def add(self, *options):
        return self.invoke("add", "first", "--api-key-stdin", *options, secret="before-canary")

    def test_binding_output_rotation_and_revocation(self):
        self.assertFalse(self.invoke("list")["configured"])
        workspace = str(self.home / "workspace")
        saved = self.add("--name", "Workspace key", "--lan", "--workspace", workspace)
        self.assertTrue(saved["restart_required"])
        self.assertFalse(saved["applied"])
        self.assertIn(b"caller_keys_file = caller-keys.json", self.config.read_bytes())
        self.assertTrue(self.config.read_bytes().startswith(b"# Retain this comment\n"))
        config = self.config.read_bytes()
        listing = self.invoke("list")
        self.assertEqual(listing["runtime_state"], "unknown")
        self.assertEqual(listing["keys"][0]["workspace"], workspace)
        self.assertNotIn("secret", json.dumps(listing))
        self.invoke("rotate", "first", "--api-key-stdin", secret="after-canary\r\n")
        self.assertEqual(json.loads(self.keyfile.read_bytes())[0], {
            "id": "first", "name": "Workspace key", "secret": "after-canary",
            "lan": True, "workspace": workspace,
        })
        self.invoke("revoke", "first")
        self.assertEqual(json.loads(self.keyfile.read_bytes()), [])
        self.assertEqual(self.config.read_bytes(), config)
        check = subprocess.run([BINARY, "check", "--config", str(self.config)],
                               capture_output=True, env=self.env, timeout=10)
        self.assertEqual(check.returncode, 0, check.stderr)

    def test_unknown_and_duplicate_arguments_preserve_files(self):
        self.add()
        before = (self.config.read_bytes(), self.keyfile.read_bytes())
        cases = [
            ("add", "new", "--api-key-stdin", "--api-key-stdin"),
            ("add", "new", "--api-key-stdin", "--name", "A", "--name", "B"),
            ("add", "new", "--api-key-stdin", "--lan", "--lan"),
            ("add", "new", "--secret", "argument-canary"),
            ("rotate", "first", "--api-key-stdin", "--lan"),
            ("revoke", "first", "--api-key-stdin"),
            ("list", "extra"),
            ("list", "--json"),
            ("list", "--config", str(self.config)),
        ]
        for args in cases:
            with self.subTest(args=args):
                output = self.invoke(*args, secret="stdin-canary", ok=False)
                self.assertNotIn("argument-canary", json.dumps(output))
                self.assertEqual((self.config.read_bytes(), self.keyfile.read_bytes()), before)

    def test_invalid_stdin_never_leaks_or_changes_configuration(self):
        before = self.config.read_bytes()
        for secret in [b"", b"private-canary\nsecond", b" private-canary",
                       b"private-canary\n\n", b"x" * 16385, b"\xff"]:
            with self.subTest(length=len(secret)):
                self.invoke("add", "new", "--api-key-stdin", secret=secret, ok=False)
                self.assertEqual(self.config.read_bytes(), before)
                self.assertFalse(self.keyfile.exists())

    def test_lan_guard_and_anonymous_legacy_preservation(self):
        self.config.write_bytes(self.config.read_bytes().replace(b"127.0.0.1:19415", b"0.0.0.0:19415"))
        self.add("--lan")
        before = (self.config.read_bytes(), self.keyfile.read_bytes())
        self.assertIn("last LAN", self.invoke("revoke", "first", ok=False)["error"])
        self.assertEqual((self.config.read_bytes(), self.keyfile.read_bytes()), before)
        self.env["ZCALLD_KEYS_FIXTURE_SHARED"] = "fixture-shared-token"
        self.config.write_bytes(self.config.read_bytes().replace(
            b"[server]\n", b"[server]\ntoken_env = ZCALLD_KEYS_FIXTURE_SHARED\n"))
        existing = json.loads(self.keyfile.read_bytes())
        existing.append({"name": "legacy", "secret": "legacy-canary", "lan": False})
        self.keyfile.write_bytes(json.dumps(existing).encode())
        self.keyfile.chmod(0o600)
        self.invoke("revoke", "first")
        self.assertEqual(json.loads(self.keyfile.read_bytes()), [existing[1]])
        listing = self.invoke("list")["keys"][0]
        self.assertIsNone(listing["id"])
        self.assertFalse(listing["manageable"])

    @contextlib.contextmanager
    def hold_lock(self, path):
        with path.open("r+b") as lock:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                yield
            finally:
                if os.name == "nt":
                    lock.seek(0)
                    msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    fcntl.flock(lock, fcntl.LOCK_UN)

    def test_both_locks_reject_concurrent_mutation(self):
        self.add()
        before = (self.config.read_bytes(), self.keyfile.read_bytes())
        for name in [".caller-keys.lock", ".caller-keys.json.zcalld-keys.lock"]:
            with self.subTest(lock=name), self.hold_lock(self.home / name):
                output = self.invoke("rotate", "first", "--api-key-stdin", secret="new-canary", ok=False)
                self.assertIn("another caller key operation", output["error"])
                self.assertEqual((self.config.read_bytes(), self.keyfile.read_bytes()), before)
        self.invoke("rotate", "first", "--api-key-stdin", secret="new-canary")

    def test_key_backup_and_lock_files_are_private(self):
        self.add()
        self.invoke("rotate", "first", "--api-key-stdin", secret="after-canary")
        files = [self.keyfile, *self.home.glob("backups/**/*.original"), *self.home.glob(".*.lock")]
        self.assertTrue(list(self.home.glob("backups/**/*.original")))
        if os.name != "nt":
            for path in files:
                self.assertEqual(path.stat().st_mode & 0o077, 0, path.name)
            return
        powershell = shutil.which("powershell.exe") or shutil.which("pwsh")
        self.assertIsNotNone(powershell, "Windows ACL fixture requires PowerShell")
        script = (
            "$ErrorActionPreference='Stop';"
            "$a=Get-Acl -LiteralPath $env:ZCALLD_TEST_ACL_FILE;"
            "$sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value;"
            "$rules=@($a.GetAccessRules($true,$true,[System.Security.Principal.SecurityIdentifier])"
            "|ForEach-Object { @{sid=$_.IdentityReference.Value;kind=$_.AccessControlType.ToString();"
            "inherited=$_.IsInherited;rights=[int]$_.FileSystemRights} });"
            "@{protected=$a.AreAccessRulesProtected;current=$sid;rules=$rules}|ConvertTo-Json -Depth 5"
        )
        for path in files:
            result = subprocess.run(
                [powershell, "-NoProfile", "-NonInteractive", "-Command", script],
                capture_output=True, text=True, env=dict(self.env, ZCALLD_TEST_ACL_FILE=str(path)),
                timeout=10, check=True
            )
            acl = json.loads(result.stdout)
            self.assertTrue(acl["protected"])
            self.assertEqual({rule["sid"] for rule in acl["rules"]}, {acl["current"], "S-1-5-18"})
            self.assertTrue(all(rule["kind"] == "Allow" and not rule["inherited"] for rule in acl["rules"]))


if __name__ == "__main__":
    unittest.main(argv=[__file__, *TEST_ARGS])
