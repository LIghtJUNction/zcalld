"""Release transaction regression tests; all Git writes stay in temporary repositories."""
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

SCRIPT = Path(__file__).with_name("release.py")


class ReleaseTransactionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="zcalld-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        (self.root / "scripts").mkdir()
        shutil.copyfile(SCRIPT, self.root / "scripts" / "release.py")
        (self.root / "Cargo.toml").write_text(
            '[workspace]\n'
            'members = [".", "crates/zcalld-plugin-api", "crates/zcalld-compat-client", "plugins/aliases", "plugins/policy"]\n'
            'resolver = "3"\n\n'
            '[workspace.package]\nversion = "0.1.0"\nedition = "2024"\n\n'
            '[package]\nname = "zcalld"\nversion.workspace = true\nedition.workspace = true\n\n'
            '[dependencies]\n'
            'zcalld-plugin-api = { version = "=0.1.0", path = "crates/zcalld-plugin-api" }\n', encoding="utf-8", newline="\n"
        )
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8", newline="\n")
        for path, name in [
            ("crates/zcalld-plugin-api", "zcalld-plugin-api"),
            ("crates/zcalld-compat-client", "zcalld-compat-client"),
            ("plugins/aliases", "zcalld-plugin-aliases"),
            ("plugins/policy", "zcalld-plugin-policy"),
        ]:
            directory = self.root / path
            (directory / "src").mkdir(parents=True)
            text = (
                f'[package]\nname = "{name}"\n'
                'version.workspace = true\nedition.workspace = true\n'
            )
            if name == "zcalld-plugin-policy":
                text += (
                    '\n[dependencies]\n'
                    'zcalld-compat-client = { path = "../../crates/zcalld-compat-client", version = "=0.1.0" }\n'
                )
            elif name != "zcalld-plugin-api":
                text += (
                    '\n[dependencies]\n'
                    'zcalld-plugin-api = { path = "../../crates/zcalld-plugin-api", version = "=0.1.0" }\n'
                )
            (directory / "Cargo.toml").write_text(text, encoding="utf-8", newline="\n")
            (directory / "src/lib.rs").write_text("#![no_std]\npub fn marker() {}\n", encoding="utf-8", newline="\n")
        self.command("cargo", "generate-lockfile", "--offline")
        self.command("git", "-c", "init.defaultBranch=main", "init", "-q")
        for key, value in [
            ("user.name", "Release Test"),
            ("user.email", "release-test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ]:
            self.command("git", "config", key, value)
        self.command("git", "add", ".")
        self.command("git", "commit", "-qm", "feat: fixture gateway")

    def command(self, *args, env=None, success=True):
        test_env = dict(os.environ if env is None else env)
        # Fixtures only resolve metadata; do not wait on a developer compiler wrapper.
        test_env["RUSTC"] = shutil.which("rustc")
        test_env["RUSTC_WRAPPER"] = ""
        test_env["RUSTC_WORKSPACE_WRAPPER"] = ""
        test_env["GIT_TERMINAL_PROMPT"] = "0"
        result = subprocess.run(args, cwd=self.root, env=test_env, text=True,
                                capture_output=True, timeout=30, encoding="utf-8")
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def release(self, *args, **kwargs):
        return self.command(sys.executable, "scripts/release.py", *args, **kwargs)

    def snapshot(self):
        return {
            str(path.relative_to(self.root)): path.read_bytes()
            for path in self.root.rglob("*")
            if path.is_file() and ".git" not in path.parts
        }

    def test_bump_updates_all_versions_and_creates_exact_annotated_tag(self):
        self.release("bump", "patch", "--commit", "--tag")
        self.release("verify-ref", "v0.1.1")
        for path in self.root.rglob("Cargo.toml"):
            data = tomllib.loads(path.read_text(encoding="utf-8"))
            if path == self.root / "Cargo.toml":
                self.assertEqual(data["workspace"]["package"]["version"], "0.1.1")
            for dependency in data.get("dependencies", {}).values():
                if "path" in dependency:
                    self.assertEqual(dependency["version"], "=0.1.1")
        self.assertEqual(self.command("git", "cat-file", "-t", "v0.1.1").stdout.strip(), "tag")
        self.assertEqual(self.command("git", "status", "--porcelain").stdout, "")
        self.assertIn("feat: fixture gateway", (self.root / "CHANGELOG.md").read_text(encoding="utf-8"))

    def test_lockfile_failure_restores_worktree_without_commit_or_tag(self):
        before = self.snapshot()
        head = self.command("git", "rev-parse", "HEAD").stdout
        with tempfile.TemporaryDirectory(prefix="zcalld-failing-cargo-") as temp:
            executable = Path(temp).resolve() / "cargo"
            executable.write_text("#!/bin/sh\nexit 27\n", encoding="utf-8", newline="\n")
            executable.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = temp + os.pathsep + env["PATH"]
            self.release("bump", "patch", "--commit", "--tag", env=env, success=False)
        self.assertEqual(before, self.snapshot())
        self.assertEqual(head, self.command("git", "rev-parse", "HEAD").stdout)
        self.assertEqual(self.command("git", "tag", "--list").stdout, "")
        self.assertEqual(self.command("git", "status", "--porcelain").stdout, "")

    def test_existing_tag_is_rejected_before_mutation(self):
        self.command("git", "tag", "v0.1.1")
        before = self.snapshot()
        self.release("bump", "patch", "--commit", "--tag", success=False)
        self.assertEqual(before, self.snapshot())
        self.assertEqual(self.command("git", "status", "--porcelain").stdout, "")

    def test_dry_run_does_not_write_files_or_tags(self):
        before = self.snapshot()
        result = self.release("bump", "minor", "--dry-run")
        self.assertIn("0.1.0 -> 0.2.0", result.stdout)
        self.assertEqual(before, self.snapshot())
        self.assertEqual(self.command("git", "tag", "--list").stdout, "")

    def test_nonincreasing_or_invalid_version_is_rejected(self):
        before = self.snapshot()
        for proposed in ("0.1.0", "0.0.9", "0.01.1", "v0.2.0", "0.2.0-rc.1"):
            with self.subTest(version=proposed):
                self.release("bump", "custom", "--version", proposed, success=False)
        self.assertEqual(before, self.snapshot())

    def test_verify_ref_rejects_a_checkout_after_the_tag(self):
        self.command("git", "tag", "v0.1.0")
        (self.root / "change.txt").write_text("later commit\n", encoding="utf-8", newline="\n")
        self.command("git", "add", "change.txt")
        self.command("git", "commit", "-qm", "feat: later commit")
        self.release("verify-ref", "v0.1.0", success=False)

    def aur_fixture(self, existing_version, existing_rel="1", existing_epoch="0"):
        directory = Path(tempfile.mkdtemp(prefix="aur-fixture-", dir=self.root))
        remotes = directory / "remotes"
        incoming = directory / "incoming"
        remotes.mkdir()
        before = {}
        for name in ("zcalld", "zcalld-bin"):
            seed = directory / ("seed-" + name)
            self.command("git", "init", "-q", "-b", "master", str(seed))
            for key, value in [
                ("user.name", "Release Test"),
                ("user.email", "release-test@example.invalid"),
                ("commit.gpgsign", "false"),
                ("core.hooksPath", "/dev/null"),
            ]:
                self.command("git", "-C", str(seed), "config", key, value)
            (seed / "PKGBUILD").write_text("# Existing package\n", encoding="utf-8", newline="\n")
            (seed / ".SRCINFO").write_text(
                f"pkgbase = {name}\n\tpkgver = {existing_version}\n"
                f"\tpkgrel = {existing_rel}\n\tepoch = {existing_epoch}\n\npkgname = {name}\n", encoding="utf-8", newline="\n"
            )
            self.command("git", "-C", str(seed), "add", ".")
            self.command("git", "-C", str(seed), "commit", "-qm", "Existing AUR version")
            remote = remotes / (name + ".git")
            self.command("git", "clone", "-q", "--bare", str(seed), str(remote))
            before[name] = self.command("git", "--git-dir", str(remote),
                                        "rev-parse", "master").stdout.strip()
            target = incoming / name
            target.mkdir(parents=True)
            (target / "PKGBUILD").write_text("# Generated package 0.1.0\n", encoding="utf-8", newline="\n")
            (target / ".SRCINFO").write_text(
                f"pkgbase = {name}\n\tpkgver = 0.1.0\n\tpkgrel = 1\n\npkgname = {name}\n", encoding="utf-8", newline="\n"
            )
        env = os.environ.copy()
        env.update({
            "GIT_CONFIG_COUNT": "3",
            "GIT_CONFIG_KEY_0": "url." + remotes.as_uri() + "/.insteadOf",
            "GIT_CONFIG_VALUE_0": "ssh://aur@aur.archlinux.org/",
            "GIT_CONFIG_KEY_1": "commit.gpgsign",
            "GIT_CONFIG_VALUE_1": "false",
            "GIT_CONFIG_KEY_2": "protocol.file.allow",
            "GIT_CONFIG_VALUE_2": "always",
        })
        return incoming, remotes, before, env

    def aur_heads(self, remotes):
        return {
            name: self.command("git", "--git-dir", str(remotes / (name + ".git")),
                               "rev-parse", "master").stdout.strip()
            for name in ("zcalld", "zcalld-bin")
        }

    def test_aur_older_tag_never_downgrades_version_pkgrel_or_epoch(self):
        for ver, rel, epoch in [("0.2.0", "1", "0"), ("0.1.0", "2", "0"), ("0.0.9", "1", "1")]:
            with self.subTest(version=ver, pkgrel=rel, epoch=epoch):
                incoming, remotes, before, env = self.aur_fixture(ver, rel, epoch)
                result = self.release("publish-aur", "--directory", str(incoming), env=env)
                self.assertEqual(before, self.aur_heads(remotes))
                self.assertEqual(result.stdout.count("Skipped "), 2)

    def test_aur_normal_upgrade_pushes_new_metadata_to_both_repositories(self):
        incoming, remotes, before, env = self.aur_fixture("0.0.9")
        self.release("publish-aur", "--directory", str(incoming), env=env)
        after = self.aur_heads(remotes)
        for name in ("zcalld", "zcalld-bin"):
            self.assertNotEqual(before[name], after[name])
            metadata = self.command(
                "git", "--git-dir", str(remotes / (name + ".git")),
                "show", "master:.SRCINFO").stdout
            self.assertIn("pkgver = 0.1.0", metadata)

    def test_aur_unrecognized_version_stops_without_changing_remote(self):
        incoming, remotes, before, env = self.aur_fixture("development")
        result = self.release("publish-aur", "--directory", str(incoming),
                              env=env, success=False)
        self.assertIn("cannot safely compare AUR version", result.stderr)
        self.assertEqual(before, self.aur_heads(remotes))


if __name__ == "__main__":
    unittest.main()
