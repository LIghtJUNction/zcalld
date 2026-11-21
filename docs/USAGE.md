# Native usage records

The routing plugin records each completed upstream attempt when its configuration
contains `usage_file`. This includes ordinary model routes while the plugin is
enabled; a routing group is not required. The observer preserves response bytes.

## Enable and disable recording

```sh
zcalld usage enable --json
zcalld usage disable --json
```

Both commands save settings only. A changed setting returns
`{"ok":true,"recording_configured":true,"restart_required":true}` when enabling,
or `recording_configured:false` when disabling. Restart the gateway separately
to apply the saved configuration consistently. The commands never restart it.
Repeating the same setting returns `restart_required:false`; that means this
invocation made no change, not that the running service has been inspected.

Enabling first validates the main configuration and loads the installed native
routing plugin against staged settings. If validation fails, the existing
configuration stays unchanged. A successful update atomically replaces
`routing.json`, preserves its existing permissions and routing groups, and saves
a private backup. A newly created configuration uses private permissions.

New installations use `usage.jsonl` beside `config.conf`. Existing absolute
`usage_file` paths are retained. Disabling moves the path to `usage_history_file`
so summary and export can still read it; enabling reuses that file. Neither
command deletes or rewrites history. Managed recording works with an empty
`groups` array. An explicit routing plugin using the same native library and
configuration file is reused, preventing duplicate usage rows.

Each row contains `time` (Unix seconds), the requested `model`, selected public
route in `upstream`, HTTP `status`, `elapsed_ms`, byte counts and these token
fields:

| Field | Source |
| --- | --- |
| `input_tokens` | Provider-reported input, prompt or `promptTokenCount` |
| `output_tokens` | Provider-reported output, completion or `candidatesTokenCount` |
| `cache_read_tokens` | Cached input details, Anthropic cache reads or Gemini cached content |
| `cache_write_tokens` | Anthropic cache creation, including separate 5-minute and 1-hour fields |
| `reasoning_tokens` | OpenAI reasoning details or Gemini thoughts |

Input and output retain their existing provider-reported meaning. Providers
differ in whether those totals include cached input or reasoning. Cache and
reasoning fields therefore describe separate reported counters; adding all five
columns does not produce a portable total. No price or cost is inferred.

## Bounded collection

Usage is collected from top-level `usage` / `usageMetadata`, Responses
`response.usage`, and Anthropic `message.usage`. The scanner skips text and tool
arguments without retaining them, so a large response does not hide its usage.
It stores at most 64 KiB for a usage object, 128 bytes for a JSON member name,
and 128 levels of nesting. Oversized usage objects are ignored. SSE `data:`
fields support optional spaces, CRLF and multiple data lines; cumulative totals
are merged by maximum within an upstream attempt. Each new attempt resets them.

## Summary and export

```sh
zcalld usage summary --json
zcalld usage summary --model group/work --since 1767312000 --until 1767398400 --json
zcalld usage export --file ./usage.jsonl
```

The time range includes `--since` and excludes `--until`; both accept Unix
seconds. `--model` matches the requested route, including migrated Magpie `req`.
Native Unix timestamps and migrated Magpie RFC 3339 `t` timestamps are supported.
Records without a readable timestamp are excluded only when filtering by time.

The summary retains `requests`, `failed`, `input_tokens`, `output_tokens` and
`source`, and adds `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`
and `recording_configured`. The last field describes the saved recording
setting only; it does not report whether a running service has loaded it.
Old rows using `in`, `out`, `cache_read`, `cache_write` and `reasoning` are read
alongside native rows. A missing token field contributes zero; historical rows
are never rewritten to invent missing measurements. Requests count recorded
upstream attempts, so a refusal followed by a retry contributes two rows.

Summary reading is bounded to one 1 MiB record at a time. Export preserves the
original bytes and creates a new private destination; it will not overwrite an
existing file.

## Verification

`cargo test -p zcalld-plugin-routing` covers protocol fields, chunk boundaries,
large text and SSE events, bounded usage objects and attempt resets.
`scripts/test-usage.py --binary PATH --routing-plugin PATH` exercises the native
gateway against loopback fixtures and checks its persisted rows and summary.
It also enables recording without groups or an explicit plugin, verifies one row
per attempt with explicit and automatic bindings, then disables recording and
checks that the retained history no longer grows after a restart.
The usage cases in `scripts/test-control.py` cover mixed native/Magpie history,
time and model filters, large-record bounds, private export, idempotent settings,
configuration preflight failures and preservation of groups, history and modes.
