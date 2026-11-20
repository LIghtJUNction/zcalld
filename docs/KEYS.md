# Caller API keys

Caller keys authenticate clients to zcalld. They are separate from provider
credentials. These commands use the same
[zcall caller-key parser and mutation API](https://github.com/LIghtJUNction/zcall/blob/ba2ee0327aeebc3153d891e2832f70e49ecf11a8/src/server/auth.rs)
as the gateway.

## Commands

| Command | Effect |
| --- | --- |
| `zcalld keys list --json` | Return IDs, names, LAN permission and workspace metadata; never secrets. |
| `zcalld keys add ID --api-key-stdin` | Add a caller-supplied secret with a new explicit ID. |
| `zcalld keys rotate ID --api-key-stdin` | Replace that ID's secret; retain its name, LAN and workspace metadata. |
| `zcalld keys revoke ID` | Remove one explicit ID from the saved key file. |

All commands accept `--config PATH`. Management output is JSON, with or without
`--json`. Unknown arguments, repeated options and missing values are errors.
`add` and `rotate` require `--api-key-stdin`; there is no secret argument,
generated-key output or implicit random replacement.

The key arrives as one UTF-8 line, optionally followed by LF or CRLF, up to
16 KiB without surrounding whitespace. Supply it from an existing private
credential source:

```sh
zcalld keys add workstation --api-key-stdin \
  --name "Workstation client" \
  --workspace /absolute/project/path \
  < /secure/path/new-caller-key.txt
```

`add` also accepts `--lan`. Without it, a key authorizes loopback clients only.
The workspace is an absolute, server-owned scope, not a client request header.
Use a separate explicit ID for each independently revocable client.

## Saved state and running requests

A successful mutation reports `saved: true`, `applied: false` and
`restart_required: true`. It does not restart the gateway. Apply the saved change
with an explicit restart after reviewing the result:

```sh
zcalld keys list --json
zcalld service restart
```

A running daemon retains the keys it loaded at startup. Rotation and revocation
take effect for subsequent authentication after that restart; they do not
retroactively cancel an accepted request. `list` describes saved metadata and
reports `runtime_state: "unknown"`, rather than claiming the daemon has reloaded.

On the first `add`, an existing gateway configuration without
`caller_keys_file` is bound to `caller-keys.json` beside that configuration.
An existing file at that default path is parsed and preserved before insertion.
Create a valid gateway configuration with `zcalld init` first.

When local trust is disabled, adding the first configured caller key changes
previously unrestricted clients to key authentication after restart. Explicit
`trust_loopback = true` retains the existing local-trust policy. A supplied valid
key still carries its verified ID and workspace.

A non-loopback listener must retain a LAN-capable caller key or a valid configured
`token_env`. Revoking the last LAN key without that alternative is rejected
before saved files are changed. Startup validation also checks the rest of the
configuration; it does not start a plugin host or contact a provider.

## Files, locking and recovery

The existing JSON array format is retained:
`id`, `name`, `secret`, `lan` and `workspace`. Optional legacy metadata remains
optional. Anonymous legacy entries are listed with `id: null` and
`manageable: false`; changing named keys preserves them. There is no ambiguous
positional deletion of an anonymous key.

Key files and stable lock files must be regular files without symlinks. On Unix,
secret files and locks require private permissions and current-user ownership;
hard links and directories writable by other users are rejected. Writes use mode
0600. Windows writes restrict the file to the current Windows account and SYSTEM
before writing its contents. Windows listing does not certify the DACL of an
arbitrary existing file; a mutation creates a newly protected replacement.

A configuration-directory lock serializes caller-key management operations. A second lock
beside the key file also protects a file shared by multiple configurations.
Locks are nonblocking: a competing operation fails clearly and can be retried
after the first operation finishes. Stable lock files are not removed on exit.

Changes use private backups under the configuration's `backups` directory and
same-directory atomic replacement. Validation failures preserve existing files.
A reported write failure restores previous bytes, including the case where
rename completed before a durability error. If restoration itself fails, the
command reports the private backup needed for recovery. These are atomic file
replacements with checked rollback, not a claim of a crash-atomic transaction
across two files.

Backups contain previous secrets and retain private permissions. Normal output
and error messages do not include submitted or stored secrets.

## Verification

The isolated fixture suite accepts an explicit binary:

```sh
python3 scripts/test-keys.py --binary /absolute/path/to/zcalld
```

It checks stdin-only handling, metadata-only output, default binding,
rotation/revocation, anonymous entries, preserved data on rejection, competing
file locks and private backup permissions. On Windows the same suite reads the
actual DACL of key, backup and lock files. Rust unit tests additionally inject a
failure after rename to verify rollback of both the changed file and a newly
created key file. No fixture uses real accounts, starts a service or sends a
provider request.
