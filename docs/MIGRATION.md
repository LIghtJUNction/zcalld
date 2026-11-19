# Migrate from Magpie

The migration keeps the original Magpie installation available while zcalld takes over its local gateway address. It preserves public model IDs, full catalog metadata, configured routes, community packages, caller keys, privacy state and history. The service transaction is separate from copying the configuration: a prepared copy does not own the live port or refresh the original accounts.

The Linux handover uses [scripts/cutover.py](../scripts/cutover.py). It requires a user systemd session, Python 3.9 or newer with Linux pidfd support, the packaged zcalld user unit, and the compatibility helper. The configuration importer and gateway support other platforms; this transaction script is specifically for Linux and systemd.

## 1. Install the candidate and create a private copy

Install the core and compatibility packages before preparing the transaction. Their installed paths are:

| Component | Installed path |
| --- | --- |
| Gateway | /usr/bin/zcalld |
| Compatibility helper | /usr/libexec/zcalld/zcalld-pluginctl |
| Privacy worker | /usr/libexec/zcalld/zcalld-privacy |
| User service | zcalld.service |

Keep the original Magpie package and executable installed. Do not start the new user service yet, and do not rewrite existing agent configurations to perform this handover. Existing callers can continue using TCP port 3425; zcalld's own local clients prefer its private Unix socket.

First inspect the importer report:

~~~sh
zcalld migrate magpie --json
~~~

For a complete copy of the currently visible catalog, save Magpie's local metadata response in a private directory. This is a GET request and does not ask a provider to generate a response:

~~~sh
umask 077
zcalld_migration_files="$HOME/.local/state/zcalld/migration-input"
mkdir -p "$zcalld_migration_files"
curl --fail --silent --show-error \
  http://127.0.0.1:3425/v1/models \
  > "$zcalld_migration_files/live-models.json"

zcalld migrate magpie \
  --source "$HOME/.config/magpie" \
  --catalog "$zcalld_migration_files/live-models.json" \
  --config "$HOME/.config/zcalld/config.conf" \
  --apply --json
~~~

The destination must be new. The importer refuses to replace an existing configuration; keep an existing staged migration and review it rather than running another import over it. If the original providers, plugins, caller keys or relevant privacy settings have changed since the copy, create a fresh candidate before preparing the service transaction.

Inspect the report and check the actual configuration:

~~~sh
zcalld migrate verify --config "$HOME/.config/zcalld/config.conf" --json
zcalld check --config "$HOME/.config/zcalld/config.conf" --json
~~~

The service transaction requires zero unresolved models and zero reported compatibility limitations. A successful configuration check validates configuration and native plugin loading. It does not establish that every remote account can currently generate a response.

The copied privacy configuration must refer to the installed worker, and its key must remain the same as the original key. The community host configuration must reference existing Bun, host and Pi files. The importer materializes its runtime modules under the new configuration's extensions directory. A candidate created with a development helper may still point its privacy worker at a source checkout; update that path to the installed worker before preparation.

Google installed-application OAuth credentials, when required by an existing login, belong in the private migration input. They are not embedded in the public repository. An account without its matching OAuth client configuration needs that configuration before it can refresh.

The importer also copies library metadata and the complete instruction-set, instruction and skill bodies under the original library directory. Internal file and directory links are materialized as ordinary private files; links outside the library and directory cycles stop the import. Helper scripts keep owner-only execution permission, but migration never runs them. The copied data therefore remains available after the original application is removed.

Original MCP OAuth sign-in state is preserved in the private snapshot and in migration-archive/mcp-signins.json when present. Retaining that state does not activate an MCP authorization flow or claim that a new MCP host is already connected.

## 2. Prepare the recovery journal

Run the script as the desktop user who owns Magpie, without sudo. Choose the actual Magpie main PID that owns port 3425. The script checks the executable, UID, process start identity and open listening socket; it does not select a process merely by its name.

~~~sh
magpie_main_pid=REPLACE_WITH_VERIFIED_MAIN_PID
zcalld_cutover_state="$HOME/.local/state/zcalld/cutover-$(date +%Y%m%d-%H%M%S)"

python3 scripts/cutover.py prepare \
  --source "$HOME/.config/magpie" \
  --config "$HOME/.config/zcalld/config.conf" \
  --binary /usr/bin/zcalld \
  --pluginctl /usr/libexec/zcalld/zcalld-pluginctl \
  --magpie-pid "$magpie_main_pid" \
  --state "$zcalld_cutover_state"
~~~

The state directory must not exist yet. Preparation creates it with mode 0700 and writes journal files with mode 0600. It also saves a complete executable recovery.py copy with mode 0700 and records its SHA256 in the journal; moving or removing the source checkout does not remove the recovery program. It records the original launch, process identity, binary hashes, service state, catalog and credential fingerprints. The original launch environment can contain secrets: keep the whole journal private and do not commit it or attach it to a public issue.

If Magpie is already managed by a user service, add its exact unit with --old-unit. If a private native Codex owner manifest has been recorded, add --external-owners with its absolute path. That manifest identifies independent processes by boot ID, PID and start time. They are excluded from the old gateway subtree and are not stopped by the transaction.

Preparation checks all of the following:

- The source and destination are separate, and the candidate records the expected source and migration snapshot.
- The new user service is installed and inactive.
- The candidate preserves TCP port 3425, explicitly trusts local callers and enables a Unix socket.
- The live Magpie catalog and candidate catalog contain identical complete model objects, including metadata fields.
- Source topology and relevant privacy settings still match the copied snapshot.
- Candidate community credentials have not been used or changed since import.
- Runtime files and installed binaries exist, and the configuration check succeeds.

Preparation writes the private journal and performs local checks. It leaves Magpie running. If preparation fails, correct the reported condition and use a new state directory; a failed preparation has not switched the service.

Review the state before proceeding:

~~~sh
python3 "$zcalld_cutover_state/recovery.py" status --state "$zcalld_cutover_state"
~~~

The phase must be prepared. A binary or configuration update after this point requires a fresh preparation, because apply verifies the recorded hashes again.

## 3. Apply the handover

The apply command performs the authorized service switch:

~~~sh
python3 "$zcalld_cutover_state/recovery.py" apply --state "$zcalld_cutover_state"
~~~

It records each transition before making the corresponding change:

| Phase | Action |
| --- | --- |
| pausing_old | Back up the original autostart entry, install a reversible Hidden=true override, and disable an explicitly supplied old unit. Send SIGTERM only to the verified Magpie main process. |
| old_stopped | Wait for the recorded gateway subtree, including its credential worker, to exit. Confirm that TCP 3425 is free. |
| synchronized | Copy the final community authentication state and usage tail; run a fresh, local import-builtin using the installed helper. |
| starting_new | Add a reversible user-service drop-in for the chosen binary and configuration, then start zcalld. |
| active | Verify TCP and Unix health, socket ownership and mode 0600, and every catalog object's full metadata; enable the user service and mark the migration complete. |

The wait is bounded, with a default of 90 seconds and an optional preparation --timeout from 60 through 600 seconds. The script does not send a forced kill to the recorded gateway or credential workers. Its service drop-in uses KillMode=mixed, a 60-second stop timeout and SendSIGKILL=no so an incomplete drain remains visible.

There is a short interruption between releasing and rebinding port 3425. Requests already in progress depend on the old application's shutdown behavior. The transaction does not replay them or send a test generation request. Its verification requests are limited to local health and model metadata.

A failed apply attempts rollback automatically. An interrupted apply is recovered with the explicit rollback command below; do not start a second transaction against the same partially switched state.

## Credential refresh ownership

A refresh token may change when it is used. Two independently refreshing copies can leave one application with an invalid token, so account ownership is part of the handover.

Community and gateway-owned builtin accounts are copied again only after the original gateway and its recorded worker have exited. The final builtin import uses the installed helper, so its embedded adapters match the compatibility package.

An existing native Codex login remains externally managed. Its adapter reads the current native auth file for each request and does not refresh that account independently. Existing native Codex sessions keep running, and their default provider and model settings are unchanged. They do not block installation or the gateway switch.

Externally managed ownership is an explicit delivered state. If its current access token expires, the original native owner must refresh it, or ownership must be handed over deliberately after those owners exit. The gateway does not automatically claim ownership in the background.

The separate handoff command requires the account selector and its private owner manifest:

~~~sh
/usr/libexec/zcalld/zcalld-pluginctl \
  --root "$HOME/.config/zcalld/extensions" \
  handoff --account 'builtin:codex#ACCOUNT_SLOT' \
  --owners /absolute/private/codex-owner-manifest.json \
  --json
~~~

Replace the selector with the actual imported key. Handoff checks the source account, reads the latest native authentication state and verifies that the recorded processes have exited using their full process identities. The transaction itself does not invoke handoff. Automatic rollback also skips externally managed Codex accounts.

## 4. Roll back while preserving new state

Keep the journal for as long as rollback may be needed:

~~~sh
python3 "$zcalld_cutover_state/recovery.py" rollback --state "$zcalld_cutover_state"
~~~

Rollback first stops zcalld and waits for the service's recorded processes and credential worker to drain. Only then does it synchronize credentials and history, restore the original launch files and service enablement, and restart Magpie.

The rollback data handling is deliberate:

| Data | Rollback behavior |
| --- | --- |
| Gateway-owned builtin authentication | The helper validates source fingerprints and writes back the current compatible tokens. |
| Externally managed native Codex authentication | Skipped; the native auth file and owner remain unchanged. |
| Community authentication | Copy current account values back only if the source fingerprint is still the one recorded at handover, or already equals the desired value from an interrupted rollback. |
| Existing usage | Preserve the original imported JSONL prefix byte for byte. |
| Usage recorded by zcalld | Append converted Magpie rows using t, in, out and ms, retaining the complete original row under zcalld_record and a transaction/offset marker. Unknown cache accounting stays explicit. |
| Autostart and unit overrides | Restore the saved files only if their current content still matches the transaction's own replacement. |
| Original executable | Verify that its contents still match the recorded binary before launching it. |

Restoring an old directory snapshot alone is insufficient after token rotation: that snapshot may contain an expired refresh token. Rollback therefore synchronizes current credentials before restarting the original application. Keep original packages installed until the migration has been accepted.

For an original desktop launch, the script restores the recorded executable, arguments, working directory and environment through a temporary user systemd unit. It also restores the autostart file for future logins. For an explicitly supplied old unit, it restores that unit's enablement and starts it again.

Rollback is resumable. If a credential source changed under another owner, a process did not drain, a usage ledger no longer has the expected prefix, or a launch file was edited independently, the script records recovery_required and stops before the conflicting overwrite or restart. Inspect the private journal and commands.log, resolve that specific conflict, and rerun rollback with the same state directory. Do not replace current account files with their stale snapshots.

A completed rollback reports rolled_back. Repeating it is a no-op.

## Verification and scope

Run the isolated transaction tests with:

~~~sh
python3 scripts/test-cutover.py
~~~

These tests use temporary files and a fake service controller. They cover final source-state synchronization, a failed activation with automatic rollback, credential changes completed during drain, external Codex preservation, concurrent source changes, interrupted rollback recovery, credential changes while acquiring the handover lock, a private recovery copy, launch-file conflict detection and complete catalog comparisons. They do not stop a real service or access a real account.

The migration implementation is in [src/migration.rs](../src/migration.rs); the packaged service definition is in [packaging/systemd/user/zcalld.service](../packaging/systemd/user/zcalld.service). The service transaction can validate local startup, permissions and catalog equivalence without exercising a remote provider. Record real provider availability separately when it has actually been checked.
