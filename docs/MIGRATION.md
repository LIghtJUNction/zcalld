# Migrate from Magpie

The migration keeps the original Magpie installation available while zcalld takes over its local gateway address. It preserves public model IDs, full catalog metadata, configured routes, community packages, caller keys, privacy state and history. The service transaction is separate from copying the configuration: a prepared copy does not own the live port or refresh the original accounts.

The Linux handover uses [scripts/cutover.py](../scripts/cutover.py). It requires a user systemd session, Python 3.9 or newer with Linux pidfd support, a libc/filesystem providing renameat2, the packaged zcalld user unit, and the compatibility helper. The configuration importer and gateway support other platforms; this transaction script is specifically for Linux and systemd.

Advanced time, text, agent and folder conditions are preserved and validated during planning. See [Routing rules and trusted workspaces](ROUTING.md) for the condition syntax, compatibility helper requirement and verified workspace bindings. A rejected condition remains a compatibility gap until corrected; migration does not silently drop it.

## 1. Install the candidate and create a private copy

Install the core and compatibility packages before preparing the transaction. Their installed paths are:

| Component | Installed path |
| --- | --- |
| Gateway | /usr/bin/zcalld |
| Compatibility helper | /usr/lib/zcalld-compat/zcalld-pluginctl |
| Privacy worker | /usr/lib/zcalld-compat/zcalld-privacy |
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

On Arch Linux, compatibility executables and their bundled resources live in
`/usr/lib/zcalld-compat`. The older libexec layout is still recognized. A saved
privacy or routing worker path is redirected only when it exactly names the old
default, that path is absent, and the new installed helper is executable. This
lookup does not rewrite private configuration or completed recovery journals;
custom paths and existing filesystem nodes remain authoritative.

## 2. Prepare the recovery journal

Run the script as the desktop user who owns Magpie, without sudo. Choose the actual Magpie main PID that owns port 3425. The script checks the executable, UID, process start identity and open listening socket; it does not select a process merely by its name.

~~~sh
magpie_main_pid=REPLACE_WITH_VERIFIED_MAIN_PID
zcalld_cutover_state="$HOME/.local/state/zcalld/cutover-$(date +%Y%m%d-%H%M%S)"

python3 scripts/cutover.py prepare \
  --source "$HOME/.config/magpie" \
  --config "$HOME/.config/zcalld/config.conf" \
  --binary /usr/bin/zcalld \
  --pluginctl /usr/lib/zcalld-compat/zcalld-pluginctl \
  --magpie-pid "$magpie_main_pid" \
  --state "$zcalld_cutover_state"
~~~

The state directory must not exist yet. Preparation creates it with mode 0700 and writes journal files with mode 0600. It also saves a complete executable recovery.py copy with mode 0700 and records its SHA256 in the journal; moving or removing the source checkout does not remove the recovery program. It records the original launch, process identity, binary hashes, service state, catalog and credential fingerprints. When an enabled, assigned library entry uses Magpie's exact local /mcp/magpie/tools endpoint on port 3425, it also saves the negotiated MCP protocol and complete tool definitions in private expected-mcp.json. An empty tool directory is a valid baseline. The original launch environment can contain secrets: keep the whole journal private and do not commit it or attach it to a public issue.

If Magpie is already managed by a user service, add its exact unit with --old-unit. If a private native Codex owner manifest has been recorded, add --external-owners with its absolute path. That manifest identifies independent processes by boot ID, PID and start time. They are excluded from the old gateway subtree and are not stopped by the transaction.

Preparation checks all of the following:

- The source and destination are separate, and the candidate records the expected source and migration snapshot.
- The new user service is installed and inactive.
- The candidate preserves TCP port 3425, explicitly trusts local callers and enables a Unix socket.
- The live Magpie catalog and candidate catalog contain identical complete model objects, including metadata fields.
- Source topology, including the MCP library, and relevant privacy settings still match the copied snapshot.
- An assigned Magpie MCP entry completes initialization, ping and tool listing against the still-running original service. Other URLs, ports and paths are not probed or rebound.
- Candidate community credentials have not been used or changed since import.
- The candidate privacy history still matches its snapshot, its effective history path is private and expected, and active privacy key identities match.
- Runtime files and installed binaries exist, and the configuration check succeeds.

Preparation writes the private journal and performs local checks. It leaves Magpie running and does not start a candidate daemon or compatibility worker. If preparation fails, correct the reported condition and use a new state directory; a failed preparation has not switched the service.

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
| synchronized | Copy the final privacy audit history, community authentication state and usage tail; run a fresh, local import-builtin using the installed helper. |
| starting_new | Add a reversible user-service drop-in for the chosen binary and configuration, then start zcalld. |
| active | Verify TCP and Unix health, socket ownership and mode 0600, every model object's full metadata and, when retained, the MCP protocol and complete tool definitions over both transports; then enable the user service and mark the migration complete. |

The wait is bounded, with a default of 90 seconds and an optional preparation --timeout from 60 through 600 seconds. The script does not send a forced kill to the recorded gateway or credential workers. Its service drop-in uses KillMode=mixed, a 60-second stop timeout and SendSIGKILL=no so an incomplete drain remains visible.

There is a short interruption between releasing and rebinding port 3425. Requests already in progress depend on the old application's shutdown behavior. The transaction does not replay them or send a test generation request. Its verification requests are limited to local health, model metadata and the retained MCP endpoint's initialization, initialized notification, ping and tools/list. They never execute tools/call. The MCP check follows session headers and catalog pagination, accepts empty catalogs and compares every tool definition independently of list order. It currently requires finite JSON metadata responses; an incompatible response, rejection, protocol change or tool metadata mismatch fails activation and enters rollback.

A failed apply attempts rollback automatically. An interrupted apply is recovered with the explicit rollback command below; do not start a second transaction against the same partially switched state.

## Privacy history during handover

Privacy history is an audit record, not the dictionary that restores placeholders. Magpie stores placeholder metadata in daily files; original values and surrounding request text remain in process memory. The active `redact.key` makes placeholder labels stable. Copying history cannot reconstruct values that an exited process held only in memory. These distinctions follow the upstream [record format and writer](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/maskrec/maskrec.go).

The compatibility worker's optional `history_dir` must resolve to the candidate's `privacy/history`. When the setting is absent or empty, it is derived from the directory containing its settings file. Preparation checks that effective path and records the worker binding's fingerprint. Continuing audit collection requires a compatibility worker with audit support; the older 0.1.2 worker preserved the imported archive but did not produce new records.

| Entry | Transaction handling |
| --- | --- |
| `YYYY-MM-DD.jsonl` | Preserve complete JSON-object lines and their original byte order. Record keys can be out of order when concurrent requests finish in a different order. |
| `checked-YYYY-MM-DD.json` | Preserve the whole counter object. These files are replaced atomically, rather than appended. |
| A historical `redact.key` inside the history directory | Preserve and fingerprint it as an immutable, opaque file. It is separate from the candidate's active `privacy/redact.key`. |
| A stable sibling lock, such as `privacy/.history.zcalld-audit.lock` | Validate it and acquire it without blocking. It stays outside the exchanged directory and is never part of the copied history. |

The original application can append a completed request, create a new day, prune an old day or clear and recreate a day. Its [checked counters](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/maskrec/checked.go) use a timer and whole-file replacement. The transaction therefore compares complete file manifests; it does not concatenate every file or rewrite records into a guessed common schema.

Preparation requires the candidate history to match the migration snapshot, and the source, snapshot and active candidate key to match. The original application may continue updating its own history while it runs. After its recorded processes have drained, apply captures the final complete history and publishes it to the candidate. It rejects a candidate that changed under another writer.

Rollback uses that final handover history as its base. After the new worker drains, it captures the candidate's complete final history. The original directory must still equal the recorded base, or already equal the result of this same interrupted rollback. If both applications' copies changed independently, the script stops for review instead of overwriting one side or guessing a merge. Accepted single-owner changes include appended records, new days, whole-day pruning or clearing, and checked-counter replacement.

After drain, synchronization creates a private sibling lock when needed and holds both source and destination locks through staging, exchange, post-checks and the durable journal update. A writer that opened the lock before an exchange and a writer opening it afterwards therefore contend on the same inode. Preparation only checks existing locks and creates no source state.

Publication uses Linux `renameat2`: a complete private staging directory is exchanged atomically with an existing history directory, or published without replacement when no directory existed. The journal is saved before the exchange. The displaced directory remains a private recovery copy, so an interruption does not leave a half-copied live history. A retry recognizes the before/after fingerprints and does not append the same records again. Recovery copies normally stay inside the transaction directory; if that directory is on a different filesystem, an adjacent private directory is recorded in the journal to keep the exchange atomic. Retain those recorded recovery directories together with the journal.

Unknown entries, symlinks, hard links, non-private files, incomplete JSONL, an active writer lock or a changed key stop the operation. The parser is bounded to 4,096 files, 512 MiB in total, 16 MiB per JSONL record and 8 MiB per checked-counter file. No history contents or key bytes are printed.

The transaction can preserve only state that the original application wrote to disk. Magpie's in-flight records and timer-held counters may not be persisted when it exits; waiting for process termination does not reconstruct those in-memory values.

## Credential refresh ownership

A refresh token may change when it is used. Two independently refreshing copies can leave one application with an invalid token, so account ownership is part of the handover.

Community and gateway-owned builtin accounts are copied again only after the original gateway and its recorded worker have exited. The final builtin import uses the installed helper, so its embedded adapters match the compatibility package.

An existing native Codex login remains externally managed. Its adapter reads the current native auth file for each request and does not refresh that account independently. Existing native Codex sessions keep running, and their default provider and model settings are unchanged. They do not block installation or the gateway switch.

Externally managed ownership is an explicit delivered state. If its current access token expires, the original native owner must refresh it, or ownership must be handed over deliberately after those owners exit. The gateway does not automatically claim ownership in the background.

The separate handoff command requires the account selector and its private owner manifest:

~~~sh
/usr/lib/zcalld-compat/zcalld-pluginctl \
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
| Privacy keys | Keep their recorded identities; a changed active or archived fallback key requires review. No key is regenerated or overwritten. |
| Privacy audit history | Publish the new worker's final complete history only while the original still matches the handover base or this rollback's already-applied result. Preserve the displaced tree as a private recovery copy. |
| Existing usage | Preserve the original imported JSONL prefix byte for byte. |
| Usage recorded by zcalld | Append converted Magpie rows using t, in, out and ms, retaining the complete original row under zcalld_record and a transaction/offset marker. Unknown cache accounting stays explicit. |
| Autostart and unit overrides | Restore the saved files only if their current content still matches the transaction's own replacement. |
| Original executable | Verify that its contents still match the recorded binary before launching it. |

Restoring an old directory snapshot alone is insufficient after token rotation: that snapshot may contain an expired refresh token. Rollback therefore synchronizes current credentials before restarting the original application. Keep original packages installed until the migration has been accepted.

For an original desktop launch, the script restores the recorded executable, arguments, working directory and environment through a temporary user systemd unit. It also restores the autostart file for future logins. For an explicitly supplied old unit, it restores that unit's enablement and starts it again.

Rollback is resumable. If a credential or privacy-history source changed under another owner, a key changed, a process did not drain, a usage ledger no longer has the expected prefix, or a launch file was edited independently, the script records recovery_required and stops before the conflicting overwrite or restart. Inspect the private journal and commands.log, resolve that specific conflict, and rerun rollback with the same state directory. Do not replace current account files with their stale snapshots.

A completed rollback reports rolled_back. Repeating it is a no-op.

## Verification and scope

Run the isolated transaction tests with:

~~~sh
python3 scripts/test-cutover.py
~~~

These tests use temporary files, a fake service controller and temporary loopback/Unix HTTP servers. They cover final source-state synchronization, a failed activation with automatic rollback, credential changes completed during drain, external Codex preservation, concurrent source changes, interrupted rollback recovery, credential changes while acquiring the handover lock, a private recovery copy, launch-file conflict detection and complete catalog comparisons. MCP fixtures cover preparation without starting the candidate, TCP/Unix parity with an empty catalog, full tool metadata and protocol mismatches, HTTP/JSON-RPC rejection with rollback, private baseline drift, session propagation, pagination, reply IDs and duplicate tool names. Privacy fixtures additionally cover the observed 947-byte JSONL tail, final writes during drain, checked counters, whole-day clearing and pruning, active and fallback keys, unsafe nodes, held writer locks, a writer racing an atomic exchange, failed activation with new audit rows, and process interruption both during private copying and immediately after exchange. Repeated rollback must preserve each row exactly once. These tests do not stop a real service, access a real account or execute a tool.

The migration implementation is in [src/migration.rs](../src/migration.rs); the packaged service definition is in [packaging/systemd/user/zcalld.service](../packaging/systemd/user/zcalld.service). The service transaction can validate local startup, permissions and catalog equivalence without exercising a remote provider. Record real provider availability separately when it has actually been checked.
