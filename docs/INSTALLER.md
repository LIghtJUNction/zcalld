# Complete native bundle installation

`zcalld install` and the standalone `zcalld-installer` executable use the same
Rust implementation. They install a verified **native bundle** into a dedicated
user directory: the core executable, its native plugins, the installer, packaged
runtime libraries, and the platform desktop helper when one is required.
Replacing only `zcalld` would leave plugins and helpers at the previous version;
the installer therefore switches the complete directory.

The implementation is in [`crates/zcalld-installer`](../crates/zcalld-installer).
The core adapter in [`src/installer.rs`](../src/installer.rs) handles command
output and a runtime lease. It does not maintain another extraction or rollback
implementation.

## Choose the installation owner

Package-managed installations remain owned by the package manager. On Arch, use
the installed `zcalld` or `zcalld-bin` package and its dependencies. The native
installer refuses managed locations such as `/usr`, `/opt/homebrew`, and Windows
Program Files. It does not turn a system installation into a user installation
in place.

For a first installation, or a standalone executable without an installer
receipt, use a new dedicated prefix. An existing directory with unrelated files
is rejected. The installer does not adopt a shared `bin` directory or replace
unrelated applications.

| Platform | Default dedicated prefix |
| --- | --- |
| Linux | `$XDG_DATA_HOME/zcalld/runtime`, or `$HOME/.local/share/zcalld/runtime` |
| macOS | `$HOME/Library/Application Support/zcalld/runtime` |
| Windows | `%LOCALAPPDATA%/zcalld/runtime` |

The result includes the absolute `binary_path`. A caller can use that path
directly; no PATH edit is necessary. The core's configuration, credentials,
history, models, and agent files live outside this prefix and are not part of
the installation transaction.

The optional compatibility package, Bun, and privacy model files have their own
installation requirements. A core bundle does not silently download them or
register notification permissions.

## Review and apply a fixed plan

The helper is published alongside each supported native archive, and the same
helper is included in that archive. Its asset name is
`zcalld-installer-VERSION-TARGET`, with `.exe` on Windows. Verify a downloaded
helper against the release's `SHA256SUMS` before executing it.

With a supported core already available:

```sh
zcalld install defaults --json
zcalld install plan --version X.Y.Z --out /absolute/private/plans/install.json --json
zcalld install apply --plan /absolute/private/plans/install.json --defer --json
```

For first installation, replace `zcalld install` with `zcalld-installer` in
these commands. Omit `--version` to resolve the latest published stable release,
or add `--prefix /absolute/new/runtime` to choose a different dedicated prefix.
The parent of an explicitly supplied plan file must be private; the helper can
create a new private parent directory.

Planning resolves only the fixed official repository,
[`LIghtJUNction/zcalld`](https://github.com/LIghtJUNction/zcalld/releases).
It records the release version, target, peeled source commit, archive asset ID,
archive size and checksum, and checksum asset ID. Applying the saved plan uses
those recorded values. It does not silently resolve a newer release during
installation. Drafts, prereleases, duplicate checksum entries, and missing
helper or archive assets are rejected.

For an explicitly supplied local archive, use:

```sh
zcalld-installer plan --archive /absolute/bundle.tar.gz \
  --sha256 ARCHIVE_SHA256 --source SOURCE_COMMIT --version X.Y.Z \
  --prefix /absolute/new/runtime --out /absolute/private/plans/local.json
```

This mode verifies the supplied archive against the supplied identities. Its
result says `verified_source: "local-sha256"`; it does not claim that the helper
independently fetched or authenticated an official GitHub release.

## What is validated before activation

[`scripts/package.py`](../scripts/package.py) creates `INSTALL.json` after
assembling all platform files. The manifest identifies the version, target,
source commit, required native components, and each regular file's path, size,
SHA-256, and mode. The installer rejects a mismatch with the saved plan, omitted
required components, extra files, changed installed files, and duplicate JSON
members.

The system GNU tar or bsdtar enumerates entries, but it never writes extraction
paths into the filesystem. The helper reads bounded file data from tar's stdout
and creates its own exclusive files beneath a private staging directory. It
rejects archive links, special entries, traversal, absolute paths, Windows drive
and alternate-stream syntax, reserved device names, and case-insensitive name
collisions. It verifies every resulting file and directory before activation.
The archive's `install.sh` is never executed.

The implementation has explicit bounds: a 256 MiB archive, 512 MiB total
unpacked data, 128 MiB per file, 2,048 manifest files, 1 MiB metadata, and bounded
listing output. Owned download and tar processes have deadlines and bounded
stdout. These are file/output limits, not a promise about the operating
system's internal buffering or process memory.

The staged `zcalld --version` and `zcalld-installer --version` must match the
selected release before the directory is switched. These version probes do not
load user configuration or contact a provider.

On Unix, private directories use mode 0700 and journal files use 0600. Windows
objects created by the installer receive a protected DACL for the current SID
and SYSTEM before payload writes. Existing private state is inspected without
changing its owner or ACL; a different owner, inherited/broader access, or an
unrecognized ACE is rejected. File type, reparse points, hard links, and lock
identity are checked separately by the installer.

## Activation, pending exit, and recovery

A stable sibling control directory holds the installation lock, runtime lock,
and per-transaction journal. It is outside the directory being replaced. Only
one installer transaction can mutate a prefix at a time.

Core processes launched from an owned bundle hold a shared runtime lease for
their lifetime. Activation needs the exclusive lease, so a running gateway
continues to use its complete old bundle, including plugins it has not loaded
yet. The installer neither terminates the gateway nor restarts a service.

The directory switch first preserves the old cohort and then activates the
staged cohort. These are two journaled renames, not one atomic exchange. A
failure restores the old cohort when possible; an interruption can be resumed
from the persisted phase. A recovery conflict is reported as `needs_attention`
and retains the involved directories instead of discarding evidence.

| Result field or phase | Meaning |
| --- | --- |
| `planned` | Fixed release identities have been saved; installation has not happened. |
| `pending_exit` | The complete candidate is staged, but the old runtime or a Windows file handle still prevents activation. |
| `helper_started: true` | A verified helper outside the replaceable directory was started to wait; this is not installation success. |
| `committed: true` | This transaction completed the validated directory switch. |
| `rollback_pending` | Rollback is waiting for the runtime lease. |
| `rolled_back` | The transaction's previous cohort was restored, or its first installation was removed from the active prefix. |
| `failed` / `needs_attention` | Preparation or recovery needs inspection; the journal contains a static error category. |

`ok` reports whether the command itself completed. A successful status read can
describe a pending or failed transaction. Callers must inspect `phase` and
`committed`, and must not display a pending operation as installed. A committed
journal is historical evidence for that transaction; a later transaction may
have installed another version.

`--defer` starts the verified external helper only when activation is pending.
It waits at most 120 seconds for the runtime lease. If the gateway continues to
run, the operation remains pending. After explicitly closing the relevant
runtime, inspect or resume it:

```sh
zcalld-installer status --transaction /absolute/transaction
zcalld-installer resume --transaction /absolute/transaction --wait-ms 120000
zcalld-installer rollback --transaction /absolute/transaction
```

Completed transactions retain their previous cohort and journal. The helper
refuses more than 64 retained transaction directories rather than deleting an
unreviewed backup. Failed or modified recovery data is not automatically
discarded. Keep required rollback records when managing disk space.

`zcalld update check` continues to report package ownership and available
version. On an unmanaged installation, `update apply` uses this full archive
transaction. An older manually copied executable stays in its original
location; the result identifies the new dedicated executable rather than
claiming the old launcher has been changed.

## Verification

Rust tests exercise whole-cohort update and rollback, lock exclusion, a live
runtime lease, externally changed files, interrupted activation, and a failure
after the new directory was activated. The native
[`scripts/test-installer.py`](../scripts/test-installer.py) suite uses the actual
core and installer executables in isolated homes, with synthetic archives and
no provider calls. It verifies malicious archive rejection, private-state
read-only checks, deferred activation, CLI dispatch, and the assembled archive
when supplied with `--archive-dir dist`.

The release workflow runs that assembled-archive check on each native target.
Local Linux tests and Windows cross-compilation do not replace the actual
Windows/macOS runtime results; release acceptance records must name which
native jobs completed.
