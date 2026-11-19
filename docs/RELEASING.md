# Release and packaging

Release builds validate Linux x86_64/aarch64, Windows x86_64, and macOS Apple Silicon/Intel from an immutable commit or version tag. The CLI, five optional native plugin libraries, configuration, manual and licenses are packaged together. Linux adds systemd user/system units and sysusers/tmpfiles declarations.

## Development and version bumps

```sh
git submodule update --init --recursive
make check
git add <completed-files>
git commit -m 'feat: describe the change'
git push
```

Ordinary pushes and pull requests do not launch remote CI. Local checks cover formatting, Clippy, workspace unit tests, an optimized build, HTTP and management integration, release transactions and ShellCheck when available. Compatibility and GUI repositories own their independent native tests and versions.

```sh
python3 scripts/release.py bump patch --dry-run
make bump BUMP=patch
make release
# Repository workflow alternative:
gh workflow run bump.yml -f bump=patch
```

A bump requires a clean worktree. It updates the workspace version, exact SDK/client dependency pins, Cargo.lock and CHANGELOG.md as a recoverable operation. Local bump commits and tags; release verifies the exact tagged HEAD and pushes the commit and tag atomically. The GitHub bump workflow explicitly calls the reusable release workflow.

For a deliberate version, use `make bump BUMP=custom VERSION=0.2.0`. Stable increasing MAJOR.MINOR.PATCH versions are supported. Existing tags are never overwritten.

## Native candidate gate

```sh
gh workflow run release.yml -f tag="$(git rev-parse HEAD)" -f candidate=true
```

Candidates require a full 40-character commit SHA. They retain build artifacts but do not publish a release or update AUR. A native candidate is useful for platform FFI, service, path and packaging changes that cannot be exercised on the Linux development machine.

| Artifact | Contents |
| --- | --- |
| Linux `*-unknown-linux-gnu.tar.gz` | CLI, five native plugins, services, config, documentation and installer |
| macOS `*-apple-darwin.tar.gz` | CLI, five dylib plugins, config, documentation and installer |
| Windows `*-pc-windows-msvc.zip` | CLI, five DLL plugins, libcurl dependencies and license notices |
| `zcalld-VERSION-TARGET[.exe]` | Standalone update executable; existing runtime dependencies remain required |
| `zcalld-VERSION-source.tar.gz` | Complete tracked root/submodule sources and SOURCE.json |
| `SHA256SUMS` | Hashes of release archives and update executables |
| AUR `zcalld` / `zcalld-bin` | Pinned source build / architecture-specific binary package |

Linux archives build on Ubuntu 24.04 and use its libcurl ABI baseline. macOS uses system libcurl. Windows builds the pinned vcpkg curl port with its `ssl` feature, which selects Schannel for the desktop target. The GUI and compatibility packages are separate optional installations.

Source packaging archives only committed files and recursively follows each gitlink. It rejects a submodule worktree whose HEAD differs from the recorded commit. Private configurations, ignored build outputs and untracked files cannot enter that source archive.

## Publishing and recovery

The publisher verifies the requested version/tag relationship and uploads immutable assets. Missing assets can be added on a resumed release; different bytes under an existing asset name are rejected. Create a new version when code or published bytes must change.

```sh
gh workflow run release.yml -f tag=v0.2.0
```

AUR publication follows successful GitHub publication. Repository settings are:

| Setting | Purpose |
| --- | --- |
| Secret AUR_SSH_KEY | Existing maintainer-authorized SSH key |
| Secret AUR_KNOWN_HOSTS | Verified aur.archlinux.org host keys |
| Variable AUR_PUBLISH=true | Enable AUR updates |

Secrets are written into private temporary files and removed after use. Forks may leave AUR publication disabled. AUR recipes use hashes of the actual published source and binary archives; no checksum bypass is generated. Metadata comparison prevents an old release from downgrading a newer AUR version.

```sh
python3 scripts/release.py aur --download
cd _release/aur/zcalld
makepkg --printsrcinfo
makepkg --cleanbuild --syncdeps
```

Source preparation fetches dependencies before frozen build/test stages. Both packages declare the configuration template as a pacman backup file. Package installation does not start or enable a service.

## Services and credentials

The CLI and daemon both read the private `credentials` file beside config.conf. Environment variables take precedence; neither shell evaluation nor systemd EnvironmentFile parsing is required. JSON-quoted assignments preserve unusual characters:

```text
ZCALLD_GATEWAY_TOKEN="your-local-gateway-token"
CLOUD_API_KEY="your-provider-key"
```

For a Linux user service:

```sh
zcalld init
# Configure upstreams, then protect credentials with mode 0600.
zcalld check
systemctl --user daemon-reload
systemctl --user enable --now zcalld.service
journalctl --user -u zcalld.service
```

The system service uses a dedicated static zcalld identity and private /var/lib/zcalld state. /etc/zcalld/config.conf remains a package-managed template:

```sh
sudo systemd-sysusers /usr/lib/sysusers.d/zcalld.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/zcalld.conf
sudo -u zcalld zcalld init --config /var/lib/zcalld/config.conf
# Configure providers and private credentials as the zcalld user.
sudo -u zcalld zcalld check --config /var/lib/zcalld/config.conf
sudo systemctl enable --now zcalld.service
```

Choose one service scope per listening address. Both units notify readiness after successful startup, use private runtime directories/sockets, and allow 60 seconds for shutdown. KillMode=mixed lets the main daemon drain its compatibility host so an in-progress credential refresh can be persisted. The system unit restricts home access; place its resources in its state directory or packaged library paths.

macOS service commands manage a per-user launchd agent. Windows uses a per-user scheduled task. Status queries the actual platform manager and gateway transport. Configuration changes require restart; no hot-reload contract is advertised.

## Updating installed programs

Package-owned executables are updated by their package manager. zcall-gui opens an interactive paru/yay terminal on Arch and reports the command's exit result. It does not overwrite pacman-owned files.

Unmanaged updates verify SHA256SUMS and the downloaded executable's reported version, preserve a backup and replace the binary. Windows queues a same-directory helper; if a running gateway keeps the destination locked, the verified download is retained and the status explains that the gateway must stop. No request is killed merely to force a self-update.

For a Magpie replacement, follow [MIGRATION.md](MIGRATION.md), including current credential synchronization before rollback.
