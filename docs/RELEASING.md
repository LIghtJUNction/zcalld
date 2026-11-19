# Release and packaging

zcalld releases contain a Linux executable, optional native plugins, systemd
units, a manual page and an example configuration. The release workflow builds
and tests native x86_64 and aarch64 artifacts from the exact version tag. It then
publishes GitHub assets and, when configured, updates the two AUR repositories.

The release scripts use Cargo, Make, Python's standard library, Git and GitHub
CLI. Runtime packages require glibc, the GCC runtime and system libcurl.

## Daily development

Run the local acceptance gate before committing a completed change:

```sh
make check
git add src crates plugins scripts docs Cargo.toml Cargo.lock
git commit -m 'feat: describe the completed change'
git push
```

The gate checks formatting, Clippy, workspace tests, an optimized build and the
local HTTP integration suite. It also checks workspace and lockfile versions.
The optional ShellCheck check runs when ShellCheck is installed. The **Check**
workflow can be dispatched manually; ordinary branch pushes and pull requests
do not start a remote test run.

## Release by bumping the version

Use the **Bump and release** workflow on the repository's default branch:

```sh
gh workflow run bump.yml -f bump=patch
# Or select a deliberate stable version:
gh workflow run bump.yml -f bump=custom -f version=0.2.0
# Inspect the next version without writing or publishing:
gh workflow run bump.yml -f bump=minor -f dry_run=true
```

The workflow updates the workspace version, exact internal SDK dependencies,
Cargo.lock and CHANGELOG.md. It creates a release commit and annotated tag,
pushes both atomically, and calls the reusable release workflow. This explicit
call is required because a tag pushed with the workflow's GITHUB_TOKEN does not
start another push-triggered workflow.

The local equivalent is:

```sh
make check
python3 scripts/release.py bump patch --dry-run
make bump BUMP=patch
make release
```

The bump requires a clean working tree. `make bump` commits and tags the version
change but does not push. `make release` verifies that HEAD is the exact tag on
`main`, then pushes that commit and tag together. A normal tag push starts the
release workflow.

Use `make bump BUMP=custom VERSION=0.2.0` for a specific version. The script
accepts stable `MAJOR.MINOR.PATCH` versions only and rejects an existing tag or
a version that does not increase.

## What the release publishes

| Artifact | Contents |
| --- | --- |
| `zcalld-VERSION-x86_64-unknown-linux-gnu.tar.gz` | x86_64 executable, both plugins, service units, config, manual, license, installer |
| `zcalld-VERSION-aarch64-unknown-linux-gnu.tar.gz` | The same files built on a native aarch64 runner |
| `SHA256SUMS` | SHA-256 hashes of the two archives |
| AUR `zcalld` | Source package built with the checked-in Cargo.lock |
| AUR `zcalld-bin` | Prebuilt release package selected for the machine's architecture |

The Linux archives are built on Ubuntu 24.04 and use the distribution's libcurl.
The executable and plugin files are stripped release builds. The shared libcurl
and its TLS backend remain system dependencies.

The AUR generator hashes the actual published source archive and each binary
archive. There are no checksum bypasses in the generated PKGBUILDs. Source
packaging fetches Rust dependencies during `prepare()` and builds/tests with
`--frozen`, which prevents network access or lockfile changes in those stages.

## One-time repository configuration

GitHub Actions needs permission to write repository contents for the release
and bump jobs. The workflows declare that permission. Branch protection, if
enabled later, must allow the chosen release mechanism to update `main`.

AUR publishing uses an SSH key already authorized by the AUR package maintainer:

| Repository setting | Purpose |
| --- | --- |
| Secret `AUR_SSH_KEY` | AUR-authorized private key |
| Secret `AUR_KNOWN_HOSTS` | Verified SSH host keys for `aur.archlinux.org` |
| Variable `AUR_PUBLISH=true` | Enables the AUR job after GitHub assets have been published |

Set secrets through GitHub's encrypted-secret interface. Do not commit keys or
put credentials in workflow files. The job writes its key into a private
temporary directory, requires the configured host key and removes the directory
on completion. A fork can leave AUR_PUBLISH unset and still build and publish its
own GitHub release.

No crates.io token is required for these releases. AUR source builds the SDK from
the workspace, and the binary package installs the release archives.

## Retry an interrupted release

Dispatch **Release** with the existing tag:

```sh
gh workflow run release.yml -f tag=v0.1.0
```

Every job checks that the version in Cargo.toml matches the requested tag and
that HEAD points to that exact commit. The publisher resumes a draft or adds
missing files. If an existing asset has different bytes, publishing stops;
create a new version rather than replacing that asset.

An AUR failure leaves the already published GitHub release available. After
repairing the AUR connection, rerun the AUR job from the failed workflow run.
The AUR updater skips a package whose metadata already matches the generated
version. Before changing a package it compares the existing and generated
`.SRCINFO` epoch, version and package release. Rerunning an older release skips
an AUR package that is already newer. Unrecognized version fields stop the
update before files are staged.

## Build and inspect packages locally

```sh
make package
tar -tzf dist/zcalld-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The archive contains `install.sh`. Run it from the extracted directory;
`PREFIX` defaults to `/usr` and `DESTDIR` supports package staging. Existing
`/etc/zcalld/config.conf` is preserved. Installation does not start or enable
a service.

After the release is public, generate the exact AUR metadata:

```sh
python3 scripts/release.py aur --download
cd _release/aur/zcalld
makepkg --printsrcinfo
makepkg --cleanbuild --syncdeps
```

The source package and binary package both declare
`backup=('etc/zcalld/config.conf')`, so pacman manages upgrades without
overwriting a modified configuration.

## systemd operation

For a user service, run `zcalld init` and edit the generated configuration.
If it uses `token_env` or `api_key_env`, create
`${XDG_CONFIG_HOME:-$HOME/.config}/zcalld/credentials` with mode 0600.
Use plain, shell-compatible assignments for the variables named in your config:

```sh
ZCALLD_TOKEN='your-local-gateway-token'
CLOUD_API_KEY='your-provider-key'
```

The packaged user unit reads this file through `EnvironmentFile`. A user
service does not inherit exports from the terminal that starts it. The CLI
loads configuration and validates required variables too, so for an interactive
check load your own trusted, shell-compatible file into that shell:

```sh
chmod 600 "${XDG_CONFIG_HOME:-$HOME/.config}/zcalld/credentials"
set -a
. "${XDG_CONFIG_HOME:-$HOME/.config}/zcalld/credentials"
set +a
zcalld check
systemctl --user daemon-reload
systemctl --user enable --now zcalld.service
journalctl --user -u zcalld.service
```

For a local upstream without credentials, the credential-file steps are
unnecessary. Stop any foreground `zcalld serve` on the same address and port
before enabling the service. When using a custom `XDG_CONFIG_HOME`, ensure the
user service manager has the same setting as the CLI.

For a system service, edit `/etc/zcalld/config.conf` and put any required
provider environment variables in `/etc/zcalld/credentials`. Protect that
credential file with mode 0600. Then:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now zcalld.service
sudo systemctl status zcalld.service
sudo journalctl -u zcalld.service
```

Both units use `Type=notify`. zcalld sends readiness only after startup
succeeds. The system unit uses a dynamic unprivileged account, a read-only system
filesystem and no home-directory access. Put system-service plugins in
`/usr/lib/zcalld/plugins` and their configuration in `/etc/zcalld`.
Choose one service scope for a given listening address and port.

Configuration changes take effect after a service restart. The units do not
advertise a reload or watchdog contract.

## References

- [GitHub: triggering workflows from workflows](https://docs.github.com/en/actions/concepts/security/github_token)
- [GitHub: reusing workflows](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows)
- [Arch Linux: Rust package guidelines](https://wiki.archlinux.org/title/Rust_package_guidelines)
- [Arch Linux: AUR submission guidelines](https://wiki.archlinux.org/title/AUR_submission_guidelines)
- [systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)
