# Optional desktop events

The desktop plugin connects existing gateway activity, quota observations and privacy events to the operating system. It adds five notification categories—usage, balance, reset, turn completion and newly masked values—and optional activity-based sleep inhibition. It is disabled unless explicitly configured. Loading the library, checking a configuration or showing settings does not start a notification worker or an operating-system action.

It is an optional native library, not a GUI or a separate daemon. It makes no provider requests, opens no browser and does not refresh accounts. Quota notifications consume successful observations produced by operations that already took place.

## Configure and inspect

Use the same `--config` path as the intended gateway:

```sh
zcalld desktop show --config /absolute/path/config.conf --json
zcalld desktop configure --file /absolute/path/desktop-settings.json --config /absolute/path/config.conf --json
zcalld desktop disable --config /absolute/path/config.conf --json
```

`show` describes saved configuration and static capabilities. Its `scope` explicitly says that these are not live daemon counters. Changes return `restart_required: true`; these commands do not restart the gateway or register a desktop notification identity.

To preserve existing Magpie preferences, first inspect a plan and then apply its exact hash:

```sh
zcalld desktop plan --source /absolute/path/magpie-config --config /absolute/path/config.conf --json
zcalld desktop sync --source /absolute/path/magpie-config --expect-plan PLAN_HASH --config /absolute/path/config.conf --json
```

`plan` is read-only. `sync` rejects a changed plan. Neither command starts the worker, asks for notification permission or performs a quota request.

The native plugin accepts this JSON configuration. Paths must be absolute; `state_dir` and `inbox` must differ. The CLI supplies its managed paths when omitted from a CLI configuration file.

```json
{
  "enabled": true,
  "keep_awake": true,
  "keep_display": false,
  "usage_alert": 90,
  "balance_alert": 0,
  "reset_reminder_hours": 24,
  "turn_done_seconds": 30,
  "mask_alert": true,
  "language": "en",
  "state_dir": "/absolute/private/desktop-state",
  "inbox": "/absolute/private/desktop-inbox",
  "backend": "native"
}
```

A zero numeric threshold disables that category. `usage_alert` is a percentage from 0 to 100; `balance_alert` is a nonnegative amount in the explicitly observed unit. `reset_reminder_hours` is bounded to 8,784 hours, and `turn_done_seconds` to 86,400 seconds. `keep_display` applies only where the backend supports it. An optional absolute `notification_helper` selects the Windows or macOS helper described below. Unknown configuration fields are rejected.

Notification wording is deliberately limited to category, approved type labels, counts, percentages, balance/unit and elapsed time. It does not contain account names, provider credentials, prompts, restored values or arbitrary upstream errors. English wording is the fallback; Chinese title labels are provided. This component does not claim complete localization of every notification body.

## What triggers each action

| Action | Required evidence | Suppression and rearming |
| --- | --- | --- |
| Keep awake | An active gateway request lease, or a counted request that ended within the previous ten minutes | The last lease releases its activity even after an early rejection, cancellation or failed retry. Disabling or stopping the plugin releases its OS inhibitor. Metadata, health and unrelated HTTP routes do not create generation leases. |
| Usage warning | A fresh, successful observation with a real `used_percent` at or above the configured threshold | Once per source/window/reset period. A fresh reading below the threshold rearms it. Failed, stale or cached observations do not rearm it. |
| Balance warning | A fresh, successful observation with an explicit finite amount and unit, at or below the threshold | Once until a fresh balance rises above the threshold. Unknown units are omitted rather than inferred from a credits field. |
| Reset reminder | Either an observed window of at least 24 hours, below 85% used and resetting within the configured lead; or verified available reset credits expiring within that lead | The two branches use separate durable marks. Five-hour windows do not trigger renewal reminders. Missing window span is treated as unknown, not inferred from time remaining. Non-account auxiliary windows are excluded except an explicitly identified weekly pool. |
| Turn complete | A recognized agent and private conversation identity; a successful final response delivered to the client; no client tool continuation, truncation or incomplete/error outcome; all requests in that conversation finished | Wait four quiet seconds, then require the configured minimum turn duration. A later request cancels the pending completion. Unknown protocols, oversized observations and unproven completion suppress the notification. Conversation state expires after 30 minutes of inactivity. |
| Newly masked values | A privacy producer confirms distinct new placeholders after its durable audit operation, and publishes only approved kinds/counts | Repeated event IDs are suppressed. Counts are gathered for three seconds and notifications are limited to one per minute. Regex occurrence count, warning-only matches and restored response text are not treated as new values. |

The reset behavior follows both branches of the pinned Magpie implementation: [`dueReminders` at `1e174f8`](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/provider/quota_alert.go). That source defines the 24-hour minimum, 85% ceiling, fresh-observation rule and separate `renews` / `expires` cases. It also defines the ten-minute tolerance for comparing reset times and the forty-day retention for notification marks. The plugin preserves those semantics without adopting Magpie's background quota polling.

Sleep-inhibition and turn behavior are based on the same pinned [`awake`](https://github.com/yetone/magpie/tree/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/awake) and [`turndone.go`](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/gateway/turndone.go) sources. The implementation counts requests observed by this gateway; it does not invent activity for work that occurs entirely outside it.

## Platform capabilities

| Platform | Inhibition | Notifications and setup |
| --- | --- | --- |
| Linux | A held logind `Inhibit("idle:sleep", ..., "block")` Unix file descriptor | Dynamically loads `libsystemd.so.0` and sends `org.freedesktop.Notifications.Notify` to the user's local session bus. No helper executable is needed. A notification service and logind authorization must exist. |
| Windows | `SetThreadExecutionState` on the dedicated plugin worker; optional display flag | The native `zcalld-desktop-notify.exe` uses Windows toast APIs and an explicitly registered `dev.zcalld.desktop` Start Menu identity. Missing identity is reported, not registered automatically. |
| macOS | IOKit assertions for system idle sleep and, when requested, display idle sleep | The native `zcalld-desktop-notify.app` bundle uses UserNotifications with bundle identity `dev.zcalld.desktop`. Normal delivery requires existing authorization. |

Linux's block inhibitor may prevent more than idle suspend, depending on logind and the desktop. It does not guarantee that the screen stays lit; `display_inhibition` remains false there. Windows and macOS display inhibition is a separate native capability. Administrative overrides, OS policy and user decisions can still prevent inhibition or notification delivery.

Linux accepts only single local Unix D-Bus addresses. `DBUS_SESSION_BUS_ADDRESS`, or the user runtime bus, supplies the notification bus. `DBUS_SYSTEM_BUS_ADDRESS` may select a local system bus; its default is `/run/dbus/system_bus_socket`. TCP and multi-address fallback strings are rejected. This also permits private-bus testing without contacting the user's desktop. Each D-Bus method call has a 500 ms timeout.

The Windows helper exposes explicit `--register`; the macOS helper exposes explicit `--request-permission`. These are operator actions, not startup/configuration side effects. The plugin only invokes `--notify-stdin`, passes at most 2 KiB of structured text, and gives the owned child a 750 ms deadline before killing and waiting for that child. It does not pass provider/API environment variables. Toasts have no launch URL or action handler. On macOS, running a raw executable outside its correctly identified application bundle does not substitute for an installed identity.

Build and validate the platform helper without registering or sending a notification:

```sh
python3 plugins/desktop/native/build.py --out-dir /absolute/build-output --self-test
```

Linux reports that no helper is required. Windows needs MSVC and the Windows SDK C++/WinRT headers; macOS needs the platform clang, Foundation and UserNotifications frameworks. No helper downloads a runtime package. `--self-test` checks JSON/XML handling only.

## Event boundary and bounded work

`dev.zcalld.events` is a separate optional ABI v1. It leaves the existing observer ABI unchanged. `configure` and `status` are passive. The host activates the shared controller after all gateway workers initialize, or an actual request/event activates it lazily. Instances with the same settings share one controller; conflicting settings for the same state directory are rejected. A worker never owns the final strong reference to its controller.

A request receives one owned `dev.zcalld.request-events` lease. Its three operations are response head, delivered body and finish. Early returns and retry failures still drop that lease. Held retry responses do not enter its body stream. Body observations occur after protocol conversion and privacy restoration, so a successful notification corresponds to what the client received. The observer never changes response bytes. A failed optional observer is released while generation continues.

The parser uses the shared zconv stream representation for the six supported wire protocols. Each SSE frame is bounded to 256 KiB; a non-streaming JSON observation to 1 MiB; parser storage to 8,192 JSON tokens and 1,024 decoded events. Large, unknown or malformed input disables completion observation for that request. Transient parsing buffers are not written to disk.

The shared worker has a queue of 128 events. A queue collision or overflow drops the optional notification, not the gateway request. Conversation tracking is bounded to 1,024 entries. The durable ledger has at most 2,048 notification marks, 2,048 seen event IDs and 512 source timestamps, with a 2 MiB serialized limit. These are application storage bounds, not a claim about total process memory.

### Cross-process events

Privacy and maintenance helpers can atomically publish to the configured private inbox:

```json
{
  "kind": 2,
  "event": {
    "version": 1,
    "event_id": "opaque-stable-event-id",
    "agent": "codex",
    "counts": [{"kind": "TOKEN", "new_values": 2}]
  }
}
```

Kind 1 is a fresh quota observation. It uses an opaque source ID and observation ID, an observation timestamp, `fresh` / `success`, a bounded list of windows, and optional explicitly known `balance` and `reset_credits`. A window may omit `span_seconds`; it can then support a usage threshold but cannot support a renewal reminder. Reset-credit expiry is accepted only from a producer with the documented provider evidence; the consumer does not fetch or infer that evidence.

Kind 2 contains only new-value counts. `record_key` is not required. Kind 0 is reserved for host activation and is rejected in inbox files.

The publisher uses a random 32-hex-digit `.json` name and atomically renames a private `.pending-*` file after writing. Each envelope is at most 16 KiB; queued, claimed and pending files together are bounded to 128 entries and 2 MiB. The consumer claims at most 32 files per pass, scans at most 256 directory entries, and checks the inbox every two seconds while enabled. Both publisher and consumer use the same nonblocking `.publish.lock` for publication and claim renames. A rename therefore cannot disappear from a concurrent capacity calculation. Removing a durably consumed file only reduces the count.

The consumer commits its ledger before deleting a claim. A leftover claim after restart is replayed against durable deduplication. Delivery is a best-effort, at-most-once attempt after that commit: it is not a guarantee that an OS notification was displayed. In particular, a crash between committing a mark and asking the OS may lose a notification; it must not repeat a sensitive operation or a model request.

Created directories use private permissions and created files are private. Unix files are checked for ownership, links and broad permissions. Windows writes apply the existing shared private-DACL implementation before writing; this is not a claim that arbitrary preexisting file ACLs have been audited. State and payload reads are bounded, and symlinks/reparse paths are rejected. The ledger stores opaque identifiers and thresholds, not prompts, credentials or restored values.

## Diagnostics and verification

The plugin status ABI reports activation, current activity, notification/inhibition capabilities, accepted notification count and dropped/invalid/suppressed counts. Static capability strings are `unknown`, `available`, `unavailable`, `denied`, `identity_required`, `failed` and `record_only`. `notifications_accepted` means the native API or record backend accepted the request; it does not mean the user saw it. A saved CLI setting is not proof that an already-running gateway loaded it.

`backend: "record"` is for isolated fixtures. It never invokes native notification/inhibition APIs. It records bounded JSONL category/text entries, inhibition transitions and changed activity snapshots. It rotates at approximately 1 MiB into one previous log. State snapshots include `active_requests`, `completed` and `suppressed`; completion is the conservative semantic proof used by eligible turn observations, not an accounting substitute for gateway usage logs.

The focused test commands are:

```sh
cargo test --locked -p zcalld-plugin-desktop --lib
cargo clippy --locked -p zcalld-plugin-desktop --all-targets -- -D warnings
python3 scripts/test-desktop.py --binary /absolute/zcalld --desktop-plugin /absolute/libzcalld_desktop.so
```

The Python suite contains five portable cases and two Linux-specific native API cases. Linux requires a C compiler, pkg-config, libsystemd development headers and dbus-daemon. One case validates dynamic symbol signatures with an isolated mock library; the other uses the actual libsystemd library and a private D-Bus peer, including Unix-FD transfer and release. Neither contacts the real notification service or logind. The portable cases cover passive configuration, all five notification categories, stale/fresh deduplication, cross-process publication locking and graceful shutdown.

Host integration is additionally covered by `scripts/test-desktop-host.py`, including privacy-restored output, early rejection, internal retry, cancellation and semantic completion. These tests use synthetic local providers and private state. Native helper compilation and self-tests belong in each platform's release gate; Linux execution and Windows cross-compilation alone do not establish macOS or Windows runtime behavior.
