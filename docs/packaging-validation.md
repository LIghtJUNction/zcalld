# Packaging verification

zcalld 0.1.0 was packaged, installed into an isolated destination and run as a
disposable user systemd service on 2026-11-20 at 03:13 Asia/Taipei.

## Environment and artifact scope

| Item | Observed value |
| --- | --- |
| Developer distribution | Omarchy / Arch Linux |
| Architecture | x86_64 |
| C library | glibc 2.44 |
| Compiler | rustc 1.100.0 |
| Installed CLI version | zcalld 0.1.0 |
| Tested local archive | zcalld-0.1.0-x86_64-unknown-linux-gnu.tar.gz |
| Local archive size at verification | 881,784 bytes |
| Executable size | 1,174,304 bytes |

This verification covers the locally built x86_64 artifact. The official
[release workflow](../.github/workflows/release.yml) builds its GNU/Linux
archives separately on Ubuntu 24.04, using native x86_64 and aarch64 runners.
Its distribution baseline and resulting archive bytes differ from this Arch
development build.

## Archive and staged installation

`scripts/package.sh --no-build` completed successfully. The archive was extracted
into a private temporary directory, then its own `install.sh` installed it with
`DESTDIR` pointing to a separate temporary staging tree and `PREFIX=/usr`.

Twelve expected installed files passed existence and mode checks:

| Installed files | Mode |
| --- | --- |
| Executable and both native plugin libraries | 0755 |
| System and user service units | 0644 |
| Manual page and MIT license | 0644 |
| README and architecture, plugin and release documentation | 0644 |
| Newly installed system configuration | 0644 |

The staged executable reported `zcalld 0.1.0` and matched the build's executable
bytes. `zcalld check` accepted the packaged example configuration. Both
`check` and `plugins` then loaded the staged aliases and policy shared libraries
with explicit local plugin configurations.

A second installation preserved an existing configuration's exact contents and
its deliberately restricted 0600 mode. Documentation is installed with its
`docs/` hierarchy so the packaged README resolves its architecture, plugin and
release-guide links. Additional project documents, including the performance
report, follow the same installation rule.

## Real systemd readiness and shutdown

The existing user service manager reported `running`. A uniquely named service
used the packaged user unit, a temporary configuration and the real optimized
gateway executable. Its configuration enabled the two staged native plugins.
A local mock upstream counted provider requests.

| Check | Result |
| --- | --- |
| Service type | notify |
| Startup state | active / running |
| Service result | success |
| Restarts | 0 |
| systemctl start duration | 29.30 ms |
| GET /healthz | HTTP 200 |
| Health response parsing | Valid JSON; status=ok, version=0.1.0 |
| systemctl stop duration | 7.05 ms |
| Stopped state | inactive / dead |
| Main process exit status | 0 |
| Requests reaching mock upstream | 0 |

The start command returned after the daemon's readiness notification. The
durations above are single observed service-operation durations, including
systemctl overhead. Throughput and memory measurements are documented
separately in the performance report.

The temporary unit, configuration, extracted archive and staging tree were
removed. The service was never enabled for boot.

## Release and AUR update checks

The release scripts passed ShellCheck, Python syntax checks and actionlint
v1.7.12. The AUR source metadata matched `makepkg --printsrcinfo`; binary metadata
uses makepkg's canonical field order.

The [release regression suite](../scripts/test_release.py) passes nine tests.
The original transaction checks cover version propagation, annotated tags,
existing-tag rejection, clean dry runs, failed-lockfile rollback, invalid
versions and exact-tag checkout validation.

The AUR tests execute clone, commit and push operations against temporary local
Git repositories. They verify that:

- A normal upgrade updates both package repositories.
- An older tag leaves a newer AUR version unchanged.
- A higher existing package release or epoch also prevents a downgrade.
- An unrecognized existing version stops the update before any remote changes.

The updater reads existing and generated `.SRCINFO` version fields before copying
or staging package files. It preserves Git history and skips an older requested
version. Real AUR publication is performed by the configured release workflow.
