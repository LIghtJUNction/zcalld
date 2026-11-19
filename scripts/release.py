#!/usr/bin/env python3
"""Version, artifact and AUR operations using only Python's standard library."""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
VERSION_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
ARCHES = ("x86_64", "aarch64")
TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin")
REPOSITORY = "LIghtJUNction/zcalld"


def run(*args: str, cwd: Path = ROOT, capture: bool = False) -> str:
    result = subprocess.run(args, cwd=cwd, text=True, check=True,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def manifest() -> dict:
    return tomllib.loads((ROOT / "Cargo.toml").read_text())


def version() -> str:
    result = manifest()["workspace"]["package"]["version"]
    if not VERSION_RE.fullmatch(result):
        raise ValueError("release version must be an unprefixed stable MAJOR.MINOR.PATCH")
    return result


def manifests() -> list[Path]:
    result = [ROOT / "Cargo.toml"]
    for member in manifest()["workspace"]["members"]:
        if member != ".":
            result.append(ROOT / member / "Cargo.toml")
    return result


def check() -> None:
    current = version()
    names = set()
    for path in manifests():
        data = tomllib.loads(path.read_text())
        names.add(data["package"]["name"])
        if data["package"].get("version") != {"workspace": True}:
            raise ValueError(f"{path.relative_to(ROOT)} must inherit workspace version")
        for name, dep in data.get("dependencies", {}).items():
            if isinstance(dep, dict) and "path" in dep:
                if dep.get("version") != "=" + current:
                    raise ValueError(f"{path.relative_to(ROOT)}: {name} version differs from workspace")
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    found = {p["name"]: p["version"] for p in lock["package"] if p["name"] in names}
    if found != {name: current for name in names}:
        raise ValueError("Cargo.lock workspace versions differ from Cargo.toml")
    print(f"Workspace and lockfile agree on {current}.")


def verify_ref(tag: str) -> None:
    if tag != "v" + version():
        raise ValueError(f"release ref {tag!r} does not match v{version()}")
    head = run("git", "rev-parse", "HEAD", capture=True)
    tagged = run("git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}", capture=True)
    if head != tagged:
        raise ValueError("release checkout is not the exact tagged commit")
    check()
    print(f"Verified {tag} at {head}.")


def next_version(kind: str, explicit: str | None) -> str:
    old = tuple(map(int, version().split(".")))
    if kind == "custom":
        if not explicit or not VERSION_RE.fullmatch(explicit):
            raise ValueError("custom requires --version MAJOR.MINOR.PATCH")
        new = tuple(map(int, explicit.split(".")))
    else:
        if explicit:
            raise ValueError("--version is only valid with custom")
        major, minor, patch = old
        new = {"major": (major + 1, 0, 0), "minor": (major, minor + 1, 0),
               "patch": (major, minor, patch + 1)}[kind]
    if new <= old:
        raise ValueError("new version must be greater than the current version")
    return ".".join(map(str, new))


def bump(args: argparse.Namespace) -> None:
    old, new = version(), next_version(args.kind, args.version)
    if args.tag and not args.commit:
        raise ValueError("--tag requires --commit")
    if args.dry_run:
        print(f"{old} -> {new}; tag v{new}; no files changed")
        return
    if run("git", "status", "--porcelain", capture=True):
        raise ValueError("bump requires a clean worktree; commit completed work first")
    if subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/tags/v{new}"],
                      cwd=ROOT).returncode == 0:
        raise ValueError(f"tag v{new} already exists")
    originals = {path: path.read_text() for path in manifests()}
    for extra in ("Cargo.lock", "CHANGELOG.md"):
        path = ROOT / extra
        originals[path] = path.read_text() if path.exists() else None
    try:
        for path in manifests():
            content = originals[path]
            if path == ROOT / "Cargo.toml":
                content, changed = re.subn(
                    r'(?m)^(version\s*=\s*")' + re.escape(old) + r'("\s*)$',
                    lambda m: m[1] + new + m[2], content, count=1)
                if changed != 1:
                    raise ValueError("workspace version assignment was not found")
            lines = []
            for line in content.splitlines(keepends=True):
                if re.match(r"\s*(?:zcalld-plugin-api|zcalld-compat-client)\s*=", line):
                    line, changed = re.subn(
                        r'(version\s*=\s*")=?' + re.escape(old) + r'(")',
                        lambda m: m[1] + "=" + new + m[2], line)
                    if changed != 1:
                        raise ValueError(f"cannot update SDK dependency in {path}")
                lines.append(line)
            path.write_text("".join(lines))
        run("cargo", "update", "--workspace", "--offline")
        date = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
        changelog = ROOT / "CHANGELOG.md"
        text = originals[changelog] or "# Changelog\n"
        previous = subprocess.run(["git", "describe", "--tags", "--abbrev=0"],
                                  cwd=ROOT, text=True, capture_output=True)
        revision = previous.stdout.strip() + "..HEAD" if previous.returncode == 0 else "HEAD"
        subjects = run("git", "log", revision, "--format=%s", capture=True).splitlines()
        entries = [s for s in subjects if not s.startswith("chore: release ")]
        if not entries:
            entries = ["Release the current verified workspace."]
        section = f"\n## {new} — {date}\n\n" + "".join(f"- {s}\n" for s in entries) + "\n"
        if text.startswith("# Changelog\n"):
            text = "# Changelog\n" + section + text[len("# Changelog\n"):].lstrip("\n")
        else:
            text = "# Changelog\n" + section + text
        changelog.write_text(text)
        check()
    except Exception:
        for path, text in originals.items():
            if text is None:
                path.unlink(missing_ok=True)
            else:
                path.write_text(text)
        raise
    paths = [str(path.relative_to(ROOT)) for path in originals]
    if args.commit:
        run("git", "add", "--", *paths)
        run("git", "commit", "-m", f"chore: release v{new}", "--", *paths)
    if args.tag:
        run("git", "tag", "-a", f"v{new}", "-m", f"zcalld v{new}")
    print(f"Bumped {old} -> {new}.")
    if args.tag:
        print("Publish the verified tagged commit with: make release")


def push() -> None:
    tag = "v" + version()
    if run("git", "status", "--porcelain", capture=True):
        raise ValueError("release push requires a clean worktree")
    if run("git", "branch", "--show-current", capture=True) != "main":
        raise ValueError("release push requires the main branch")
    verify_ref(tag)
    run("git", "push", "--atomic", "origin", "HEAD:refs/heads/main", f"refs/tags/{tag}")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def archive_name(ver: str, arch: str) -> str:
    return f"zcalld-{ver}-{arch}-unknown-linux-gnu.tar.gz"


def artifact_names() -> list[str]:
    ver = version()
    names = [f"zcalld-{ver}-source.tar.gz"]
    for target in TARGETS:
        stem = f"zcalld-{ver}-{target}"
        names.extend([stem + (".zip" if "windows" in target else ".tar.gz"), stem + (".exe" if "windows" in target else "")])
    return names


def checksums(directory: Path) -> None:
    names = artifact_names()
    for name in names:
        if not (directory / name).is_file():
            raise ValueError(f"missing release asset: {name}")
    target = directory / "SHA256SUMS"
    target.write_text("".join(f"{digest(directory / name)}  {name}\n" for name in names))
    print(target)


def publish(args: argparse.Namespace) -> None:
    verify_ref(args.tag)
    directory = args.directory.resolve()
    checksums(directory)
    names = artifact_names() + ["SHA256SUMS"]
    result = subprocess.run(
        ["gh", "release", "view", args.tag, "--repo", args.repo, "--json", "assets,isDraft"],
        cwd=ROOT, text=True, capture_output=True)
    if result.returncode:
        # An auth/network failure must not be mistaken for a missing release.
        lookup = subprocess.run(["gh", "api", f"repos/{args.repo}/releases/tags/{args.tag}"],
                                cwd=ROOT, text=True, capture_output=True)
        if lookup.returncode == 0 or "HTTP 404" not in lookup.stderr:
            raise ValueError("could not read release state; refusing to create blindly")
        run("gh", "release", "create", args.tag, "--repo", args.repo, "--verify-tag",
            "--draft", "--generate-notes", "--title", f"zcalld {args.tag}")
        metadata = {"assets": [], "isDraft": True}
    else:
        metadata = json.loads(result.stdout)
    existing = {asset["name"] for asset in metadata["assets"]}
    with tempfile.TemporaryDirectory(prefix="zcalld-release-") as temp:
        for name in names:
            if name in existing:
                run("gh", "release", "download", args.tag, "--repo", args.repo,
                    "--pattern", name, "--dir", temp)
                if digest(Path(temp) / name) != digest(directory / name):
                    raise ValueError(f"published {name} differs; bump the version instead of replacing it")
            else:
                run("gh", "release", "upload", args.tag, str(directory / name),
                    "--repo", args.repo)
    if metadata["isDraft"]:
        run("gh", "release", "edit", args.tag, "--repo", args.repo, "--draft=false")
    print(f"Published {args.tag}; existing assets are never overwritten.")


def download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": "zcalld-release"})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=60) as response, destination.open("wb") as output:
                shutil.copyfileobj(response, output)
            return
        except (urllib.error.URLError, TimeoutError):
            destination.unlink(missing_ok=True)
            if attempt == 3:
                raise
            time.sleep(2 * (attempt + 1))


def aur(args: argparse.Namespace) -> None:
    ver = version()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        raise ValueError("repository must be owner/name")
    directory = args.directory.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    source = directory / f"zcalld-{ver}-source.tar.gz"
    source_url = f"https://github.com/{args.repo}/releases/download/v{ver}/zcalld-{ver}-source.tar.gz"
    if args.download:
        download(source_url, source)
        for arch in ARCHES:
            name = archive_name(ver, arch)
            run("gh", "release", "download", "v" + ver, "--repo", args.repo,
                "--pattern", name, "--dir", str(directory), "--clobber")
    paths = [source] + [directory / archive_name(ver, arch) for arch in ARCHES]
    for path in paths:
        if not path.is_file():
            raise ValueError(f"missing artifact {path}; use --download after publishing the release")
    values = {
        "VERSION": ver, "REPOSITORY": args.repo, "SOURCE_SHA256": digest(source),
        "X86_64_SHA256": digest(paths[1]), "AARCH64_SHA256": digest(paths[2]),
    }
    for name in ("zcalld", "zcalld-bin"):
        output = args.output.resolve() / name
        output.mkdir(parents=True, exist_ok=True)
        for filename in ("PKGBUILD", ".SRCINFO"):
            text = (ROOT / "packaging" / "aur" / name / (filename + ".in")).read_text()
            for key, value in values.items():
                text = text.replace("@" + key + "@", value)
            if re.search(r"@[A-Z0-9_]+@", text):
                raise ValueError(f"unresolved template value in {name}/{filename}")
            (output / filename).write_text(text)
        print(output)
    print("AUR recipes contain hashes of the downloaded release bytes.")


def aur_version(path: Path) -> tuple[int, tuple[int, ...], tuple[int, ...]]:
    """Read the stable version fields we publish, without executing a PKGBUILD."""
    fields = {}
    for line in path.read_text().splitlines():
        if line.strip().startswith("pkgname ="):
            break
        match = re.fullmatch(r"\s*(epoch|pkgver|pkgrel)\s*=\s*(\S+)\s*", line)
        if match:
            name, value = match.groups()
            if name in fields:
                raise ValueError(f"duplicate {name} in {path}")
            fields[name] = value
    ver = fields.get("pkgver", "")
    rel = fields.get("pkgrel", "")
    epoch = fields.get("epoch", "0")
    if (not VERSION_RE.fullmatch(ver)
            or not re.fullmatch(r"[0-9]+(?:\.[0-9]+)*", rel)
            or not re.fullmatch(r"[0-9]+", epoch)):
        raise ValueError(f"cannot safely compare AUR version fields in {path}")
    return int(epoch), tuple(map(int, ver.split("."))), tuple(map(int, rel.split(".")))


def aur_version_text(value: tuple[int, tuple[int, ...], tuple[int, ...]]) -> str:
    epoch, ver, rel = value
    prefix = f"{epoch}:" if epoch else ""
    return prefix + ".".join(map(str, ver)) + "-" + ".".join(map(str, rel))


def publish_aur(args: argparse.Namespace) -> None:
    for name in ("zcalld", "zcalld-bin"):
        source = args.directory.resolve() / name
        for filename in ("PKGBUILD", ".SRCINFO"):
            if not (source / filename).is_file():
                raise ValueError(f"missing {source / filename}")
        with tempfile.TemporaryDirectory(prefix=f"{name}-aur-") as temp:
            checkout = Path(temp) / name
            run("git", "clone", f"ssh://aur@aur.archlinux.org/{name}.git", str(checkout))
            incoming = aur_version(source / ".SRCINFO")
            if (checkout / ".SRCINFO").exists():
                current = aur_version(checkout / ".SRCINFO")
                if incoming < current:
                    print(f"Skipped {name}: AUR {aur_version_text(current)} is newer "
                          f"than requested {aur_version_text(incoming)}.")
                    continue
            for filename in ("PKGBUILD", ".SRCINFO"):
                shutil.copyfile(source / filename, checkout / filename)
            run("git", "add", "PKGBUILD", ".SRCINFO", cwd=checkout)
            changed = subprocess.run(["git", "diff", "--cached", "--quiet"], cwd=checkout).returncode
            if changed == 0:
                print(f"{name} already matches v{version()}.")
                continue
            if changed != 1:
                raise ValueError(f"could not inspect staged AUR changes for {name}")
            run("git", "-c", "user.name=github-actions[bot]", "-c",
                "user.email=41898282+github-actions[bot]@users.noreply.github.com",
                "commit", "-m", f"Update {name} to {version()}", cwd=checkout)
            run("git", "push", "origin", "HEAD:master", cwd=checkout)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    sub = result.add_subparsers(dest="command", required=True)
    sub.add_parser("version")
    sub.add_parser("check")
    verify = sub.add_parser("verify-ref")
    verify.add_argument("tag")
    change = sub.add_parser("bump")
    change.add_argument("kind", choices=("patch", "minor", "major", "custom"))
    change.add_argument("--version")
    change.add_argument("--dry-run", action="store_true")
    change.add_argument("--commit", action="store_true")
    change.add_argument("--tag", action="store_true")
    sub.add_parser("push")
    sums = sub.add_parser("checksums")
    sums.add_argument("--directory", type=Path, default=ROOT / "dist")
    release = sub.add_parser("publish")
    release.add_argument("tag")
    release.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", REPOSITORY))
    release.add_argument("--directory", type=Path, default=ROOT / "dist")
    generate = sub.add_parser("aur")
    generate.add_argument("--directory", type=Path, default=ROOT / "dist")
    generate.add_argument("--output", type=Path, default=ROOT / "_release" / "aur")
    generate.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", REPOSITORY))
    generate.add_argument("--download", action="store_true")
    upload = sub.add_parser("publish-aur")
    upload.add_argument("--directory", type=Path, default=ROOT / "_release" / "aur")
    return result


def main() -> None:
    args = parser().parse_args()
    if args.command == "version":
        print(version())
    elif args.command == "check":
        check()
    elif args.command == "verify-ref":
        verify_ref(args.tag)
    elif args.command == "bump":
        bump(args)
    elif args.command == "push":
        push()
    elif args.command == "checksums":
        checksums(args.directory.resolve())
    elif args.command == "publish":
        publish(args)
    elif args.command == "aur":
        aur(args)
    elif args.command == "publish-aur":
        publish_aur(args)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"release: {error}", file=sys.stderr)
        sys.exit(1)
