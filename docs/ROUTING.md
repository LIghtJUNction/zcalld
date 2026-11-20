# Routing rules and trusted workspaces

The optional routing plugin evaluates a group's rules after request middleware and privacy filtering. Rules select existing public routes; they do not add upstream credentials, listeners or a second gateway. Simple groups stay entirely in Rust. Time windows, `where`, `folders` and `exceptFolders` use the optional compatibility helper for Go's RE2 engine and IANA time zone semantics.

Advanced rules require the compatibility component containing the version 1 routing worker protocol, introduced in zcalld-compat 0.1.2. The native routing plugin remains optional. The packaged helper is `/usr/libexec/zcalld/zcalld-pluginctl` on Linux.

## Configure a group

First create the direct routes that the group can select. Given existing routes named `fast` and `careful`, save this as `group-work.json`:

```json
{
  "members": ["fast", "careful"],
  "mode": "order",
  "affinity": "turn",
  "rules": [
    {
      "use": "careful",
      "time": {
        "from": "09:00",
        "to": "18:00",
        "days": ["mon", "tue", "wed", "thu", "fri"],
        "zone": "Europe/Berlin"
      },
      "where": [
        {"field": "last", "op": "matches", "value": "design|review|debug"},
        {"field": "tokens", "op": "gte", "value": "8k"}
      ]
    }
  ]
}
```

Apply it with the normal CLI:

```sh
zcalld groups set group/work --file group-work.json --config "$HOME/.config/zcalld/config.conf"
zcalld check --config "$HOME/.config/zcalld/config.conf"
```

The CLI discovers the helper from `ZCALLD_PLUGINCTL` when explicitly set, the installed layout relative to the executable, or the standard Linux libexec directory. It writes the absolute helper path into `routing.json` as `rules_worker`; a daemon started by systemd therefore does not depend on the shell's environment. Development builds also recognize their compatibility source checkout.

A successful edit reports `restart_required: true`. Restart the configured service to apply the new group to its worker state. For the packaged user unit:

```sh
systemctl --user restart zcalld.service
```

A definition file can provide `members`, `mode` or `routing`, and the original group settings `rules`, `affinity`, `effort`, `classifier`, `levels`, `intentSure` and `effortMaps`. Original settings may also appear under an `original` object. An existing group's omitted members and mode are retained. Explicit `--members` and `--mode` options override the file. Each rule's `use` and `more` routes must belong to that group; nested groups are rejected.

The complete candidate rule configuration is validated before either the INI file or `routing.json` is replaced. An invalid time zone, unsupported regular expression, unknown condition, or malformed member list fails the command and leaves both existing files intact. The Magpie migration planner performs the same validation and reports a compatibility gap instead of silently dropping an unknown rule.

## Rule evaluation

All conditions inside one rule are combined with AND. The first matching ordinary rule provides its preferred member order. A rule can combine the existing `tokens`, `images`, `effort`, `agents`, `intent` and `compact` fields with the advanced fields described here.

The advanced helper evaluates deterministic conditions. If a rule also has `intent`, the routing plugin combines that result with the configured classifier result. Both receive the request after privacy filtering. Sensitive text removed by the privacy plugin is therefore unavailable for a text match or an external classifier request.

`pause: true` temporarily excludes a member. Pause rules may use time, agent and folder conditions, including `where` conditions whose field is `agent`. They cannot use request text, token counts, images, effort, intent, compact mode, `more` or `useEffort`. Pause conditions are checked again for each request, including requests in a turn already bound by affinity. An unavailable group returns an error before sending a generation request.

### Time windows

`time` contains `from`, `to`, optional `days` and optional `zone`.

- The start is inclusive; the end is exclusive.
- A window crossing midnight belongs to its starting day. Monday 22:00–02:00 includes early Tuesday until 02:00.
- Equal start and end represent an entire selected day. A window covering every hour of every day is rejected as an empty condition.
- Omitting `days` means every day. Use canonical weekday values `sun`, `mon`, `tue`, `wed`, `thu`, `fri` and `sat`.
- `zone` is an IANA identifier such as `Asia/Shanghai`, `Europe/Berlin` or `UTC`. An omitted zone uses the helper's local time zone.
- The helper embeds the time zone database and does not change the daemon's `TZ` environment. DST transitions follow the selected location's wall clock.

Canonical `HH:MM` values are recommended. The compatibility parser also accepts Magpie's existing hour and compact clock spellings.

### Structured conditions

`where` is an array of objects with `field`, `op` and, except for subagent checks, `value`.

| Field | Operations | Value and interpretation |
| --- | --- | --- |
| `tokens` | `gte`, `gt`, `lt`, `lte` | Nonnegative threshold as a string, including decimal `k` and `m` suffixes. Uses the routing plugin's request token estimate. |
| `effort` | `is`, `not`, `gte`, `lte` | `off`, `on`, or a supported effort level. Comparisons require a level. |
| `agent` | `is`, `not` | Agent identifiers separated by commas, vertical bars or spaces. |
| `subagent` | `is`, `not` | Boolean condition; no value is required. |
| `message` | `contains`, `excludes`, `starts`, `matches` | The routing plugin's current user text after privacy filtering. |
| `system` | `contains`, `excludes`, `starts`, `matches` | System or developer text blocks in the incoming protocol. |
| `last` | `contains`, `excludes`, `starts`, `matches` | Text blocks of the last user message. A tool-only last user message has no text. |

Text predicates are case insensitive. `starts` ignores leading and trailing outer whitespace. `matches` uses Go RE2 syntax; backreferences and lookbehind are rejected during validation. A pattern enclosed in slashes is accepted for compatibility. System and last-message predicates examine each text block independently: a positive predicate needs a matching block, while `excludes` requires every block to exclude the text.

Effort comparison orders `low`, `medium`, `high`, `xhigh`, `max` and `ultra`. Disabled reasoning is below every level. Reasoning enabled without a known level does not satisfy a numeric level comparison. This only controls the predicate; the selected provider's supported output effort levels are still handled by the routing plugin.

Agent and subagent metadata are useful routing hints, not authentication. In particular, an `X-Agent-Name` header does not grant a workspace. Known CLI user-agent products are normalized to identifiers such as `codex`, `claude` and `opencode`.

### Folders and exclusions

`folders` matches when the trusted working directory equals a listed directory or is below it at a path component boundary. `exceptFolders` rejects a match under any listed directory. The two lists can be combined.

```json
{
  "use": "careful",
  "folders": ["/home/alice/src"],
  "exceptFolders": ["/home/alice/src/sandbox"],
  "where": [{"field": "last", "op": "contains", "value": "review"}]
}
```

Paths are normalized lexically, including separator differences, home-relative paths, Windows drive spelling and compatible WSL paths. Prefixes respect directory boundaries, so `/work/app` does not include `/work/apple`. Windows drive paths and macOS/Windows comparisons are case insensitive. The implementation does not resolve a client-supplied path through filesystem symlinks.

If no trusted directory is available, a positive `folders` condition does not match. An `exceptFolders` list alone does not exclude an unknown directory. A directory rule is a routing preference; use gateway authentication and private configuration to control access.

## Where trusted directory information comes from

Request headers and JSON fields such as `X-Cwd`, `X-OpenCode-Directory`, `X-Zcalld-Workspace` or embedded `<cwd>` text cannot establish a trusted directory. They may remain available as compatibility hints to community middleware, but the advanced routing helper does not receive them as directory evidence.

The gateway uses these sources, in order:

1. An explicit server-owned workspace bound to the **verified caller key**.
2. On Linux, the direct local client's process working directory, verified from kernel socket information.
3. Otherwise, an unknown directory.

For Linux TCP, the helper requires a loopback connection, finds the reversed client socket tuple and inode in procfs, verifies same-user process ownership and process start identity, and rechecks the socket after reading cwd. Ambiguous owners with different directories yield unknown. For Linux Unix sockets, the gateway supplies `SO_PEERCRED` PID/UID and process start identity, which the helper verifies again before using cwd. A local proxy's connection identifies that proxy process, so use an explicit binding when a stable workspace must survive such an intermediary.

Other platforms use the explicit key binding. They do not infer a trusted workspace from unverified process names or headers. The operating system may also restrict procfs access on Linux; this produces an unknown directory without weakening the evidence requirements.

### Bind a caller key to a workspace

Enable a private caller-key file in the gateway configuration:

```ini
[server]
caller_keys_file = caller-keys.json
```

The file contains the existing key metadata plus an optional absolute `workspace`:

```json
[
  {
    "id": "project-a",
    "name": "Project A",
    "lan": false,
    "secret": "replace-with-a-private-random-caller-key",
    "workspace": "/home/alice/src/project-a"
  }
]
```

Use mode `0600` on Unix. Workspace bindings require a nonempty, unique `id`, an absolute path and a valid secret. Use the platform's native absolute path spelling; JSON Windows backslashes must be escaped.

The client supplies that key using the gateway's supported authentication header. The gateway checks the entire secret and the key's LAN/loopback scope before selecting its workspace or exposing its nonsecret identifier to an optional HTTP plugin. Neither the key ID by itself, the generic gateway token, nor `trust_loopback=true` selects this binding. No inbound secret is included in the rule-worker protocol.

## Process and memory behavior

A configuration with no advanced conditions does not require or start the rule helper, even when an irrelevant `rules_worker` path is absent.

An advanced configuration is validated once in a short-lived helper per distinct configuration and executable revision. Validation exits without retaining a worker. The first request selecting an advanced group starts a shared worker for that configured routing plugin. Later requests reuse its compiled expressions and time zones. It exits with the gateway's plugin lifecycle and owns no authentication state.

Input sizes, group/rule counts, expression lengths and procfs traversal time are bounded. The worker uses local stdin/stdout RPC, logs no request text or directories, and makes no network requests. A worker failure holds the request back with the routing-unavailable error; it does not bypass the configured conditions.

## Verification

The Go tests in `extensions/compat/routing` cover time and DST boundaries, RE2 rejection, structured predicates, folder normalization and actual Linux socket/process evidence. The native end-to-end fixture runs the real gateway, routing plugin and privacy worker against local mock upstreams:

```sh
python3 scripts/test-routing-advanced.py \
  --binary /absolute/path/to/zcalld \
  --pluginctl /absolute/path/to/zcalld-pluginctl \
  --routing-plugin /absolute/path/to/routing-library \
  --redact-plugin /absolute/path/to/privacy-library \
  --privacy-worker /absolute/path/to/zcalld-privacy
```

`ZCALLD_TEST_BINARY` and `ZCALLD_TEST_PLUGINCTL` can supply the first two arguments. The portable cases cover the actual Rust↔Go protocol, caller-key bindings, spoofed directory hints, privacy ordering, pause/affinity behavior, CLI atomic failure and migration validation. Only kernel PID/cwd cases are skipped outside Linux. Native execution evidence for a release belongs to that release's platform gate; cross-compilation alone is not native runtime verification.

## Compatibility reference

The semantic reference is Magpie commit [1e174f8f9f613a920203a4658d259456fcab5d0d](https://github.com/yetone/magpie/tree/1e174f8f9f613a920203a4658d259456fcab5d0d), especially `internal/provider/grouprule.go`, `rulecond.go`, `rulefolder.go` and `ruleparse.go`. The compatibility source retains the upstream MIT attribution in `extensions/compat/NOTICE_ROUTING.md`. Directory provenance is deliberately explicit here: request hints cannot substitute for a verified binding or kernel process evidence.
