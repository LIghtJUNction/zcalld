# Agent library

`zcalld library` keeps shared instructions, skill files, and MCP definitions in the
zcalld configuration directory. Editing the library does **not** change an agent's
files. Apply the selected content explicitly, inspect the result, and start a new
agent session when that agent reads instructions or skills only at startup.

The gateway does not read this state, watch these files, run MCP servers, or keep a
library worker alive. These commands run only when invoked.

## Start with a preview

```sh
zcalld library list --json
zcalld library targets --json
zcalld library apply --agent claude --dry-run --json
zcalld library apply --agent claude --json
```

`apply` selects entries assigned to the requested agent. `--kind mcp`, `--kind
instructions`, or `--kind skills` restricts the operation. The default is `all`.
The result lists changed paths and counts, never MCP environment or header values.
`--dry-run` validates the complete operation without writing agent files or an
ownership record. `targets` reports the supported categories and instruction
overrides; it does not claim an application is installed or its service reachable.

All management commands accept the existing `--config PATH` and `--json` options.
The previously supported `library list`, `import`, `export`, `add-mcp`, and `apply`
commands remain available.

## Instructions

Instructions consist of a selected named set, an agent assignment list, and optional
extra text for each agent. The default set always exists conceptually and may be
empty.

```sh
zcalld library instructions set default --file ./shared.md --agents claude,codex
zcalld library instructions create research --name Research --file ./research.md
zcalld library instructions rename research --name 'Research work'
zcalld library instructions use research
zcalld library instructions extra --agent claude --file ./claude-extra.md
zcalld library instructions show research --json
zcalld library instructions show --agent claude --json
zcalld library instructions list --json
zcalld library instructions apply --agent claude --dry-run
zcalld library instructions apply --agent claude
```

`assign --agents claude,codex` replaces the assignment list. `enable --agent NAME`
adds one agent and `disable --agent NAME` removes it. Apply afterwards to update
that agent. An empty file passed to `extra` clears its extra text.

`link ID --file PATH` reads a user-maintained source file on each apply. It does
not copy the file or grant permission to modify it. A later `set ID --file PATH`
copies text into the library and detaches the link. Removing a linked set never
deletes its external source. `remove ID` deletes a nondefault set; removing the
active set selects `default`. To clear the default set, set it from an empty file.

Applied text is placed between `zcalld:library:begin` and `zcalld:library:end`
comments. Text outside those comments remains byte-for-byte unchanged. The next
apply checks that the prior managed block has not been edited. A conflicting edit,
malformed marker, higher-priority `AGENTS.override.md`, missing linked source, or
source that is also the destination stops the operation. Existing Magpie instruction
blocks require explicit review before another writer is added.

## Skills

A skill is a directory containing `SKILL.md` and its supporting files. Installation
copies the entire directory, including binary resources and executable scripts;
no script is run. Only Git's `.git` metadata is excluded. Internal symbolic links
are materialized as ordinary files. Links outside the source, cycles, special
files, and excessive trees are rejected.

The `files` array returned by `skills show --json` contains relative resource
names separated by `/` on every platform; pass those names unchanged to
`skills read/write --file`. Absolute source and destination paths use the native
filesystem format.

```sh
zcalld library skills install review --dir ./skills/review --agents claude,codex
zcalld library skills create notes --file ./SKILL.md --agents claude
zcalld library skills list --json
zcalld library skills show review --json
zcalld library skills read review --file SKILL.md --json
zcalld library skills write review --file references/style.md --input ./style.md
zcalld library skills update review
zcalld library skills assign review --agents claude,opencode
zcalld library skills apply --agent claude --dry-run
zcalld library skills apply --agent claude
```

`update` reads the remembered local source, or a new `--dir PATH`. For a Git-backed
skill, check out and review the desired revision first, then pass that skill's
directory. This command does not fetch a moving branch, install dependencies, or
execute package lifecycle scripts.

`disable NAME` preserves the stored files and assignments while excluding that
skill from subsequent applies. `enable NAME` restores its eligibility. `remove
NAME` removes the library's entry and its stored files; applying to an agent then
removes only the previously managed copy. An unrelated agent skill is left alone.
An extra file, edit, executable-bit change, or replacement link in a managed agent
copy is a conflict, so apply and revert stop before changing any targets.

Each tree is bounded to 32 MiB, 2,048 files and 32 directory levels; an individual
file is limited to 8 MiB. One operation is bounded to 64 MiB and 4,096 output files.
Exceeding a bound is an error, never a partial skill import.

## MCP definitions

MCP entries use the imported Magpie metadata shape:

```json
{
  "name": "example",
  "transport": "stdio",
  "command": "example-mcp",
  "args": ["--read-only"],
  "env": {"TOKEN": "${EXAMPLE_TOKEN}"},
  "agents": ["claude", "codex"]
}
```

For a remote server use `transport: "http"` or `"sse"`, `url`, and optional
`headers`. The library preserves unknown metadata, selected header accounts,
assignments and disabled state. Selected account headers replace matching base
header names case-insensitively. Header accounts and the definitions themselves
are private files; listing or showing an MCP entry prints metadata only.

```sh
zcalld library mcp add --file ./server.json
zcalld library mcp set example --file ./replacement.json
zcalld library mcp update example --file ./replacement.json
zcalld library mcp show example --json
zcalld library mcp show example --file ./private-copy.json
zcalld library mcp assign example --agents claude,codex
zcalld library mcp disable example
zcalld library mcp enable example
zcalld library mcp remove example
zcalld library mcp apply --agent codex --dry-run
zcalld library mcp apply --agent codex
```

`set` is an upsert of a complete definition, `add` requires a new name, and `update`
requires an existing name. `add-mcp --file PATH` remains an alias for `mcp set`.
An explicit empty `agents` array selects no agents. For compatibility with the
earlier zcalld CLI, an entry with no `agents` member is eligible for an explicitly
requested agent. Imported Magpie entries already have assignment arrays.

Transport conversion is explicit: Gemini uses `httpUrl` for streamable HTTP and
`url` for SSE; Antigravity uses `serverUrl`; OpenCode uses `local`/`remote` and an
array command; Zed uses `context_servers`. Codex uses `[mcp_servers."NAME"]`
tables, `http_headers`, `env_http_headers`, `bearer_token_env_var`, and `env_vars`.
Environment references stay references: the CLI does not resolve secrets into
new agent files. Unsupported interpolation or transport combinations fail before
any files are written. Codex startup timeouts and disabled-tool lists are supported;
other agents' disabled-tool permission formats are reported as unsupported rather
than silently discarded.

## Application matrix

These are file-format adapters. No agent is installed or started by the library.
Paths below are the normal user locations; supported native directory overrides
are honored. `library targets` is the machine-readable capability report.

| Agent | Instructions | Skills | MCP destination / format |
| --- | --- | --- | --- |
| Claude Code | `.claude/CLAUDE.md` | `.claude/skills` | `.claude.json`, `mcpServers` |
| Codex | `.codex/AGENTS.md` | `.codex/skills` | `.codex/config.toml`, TOML tables |
| Gemini CLI | `.gemini/GEMINI.md` | `.gemini/skills` | `.gemini/settings.json`, `mcpServers` |
| Qwen Code | `.qwen/QWEN.md` | `.qwen/skills` | `.qwen/settings.json`, Gemini format |
| Antigravity / agy | `.gemini/config/GEMINI.md` | `.gemini/config/skills` | `.gemini/config/mcp_config.json` |
| OpenCode | config directory `AGENTS.md` | config directory `skills` | `opencode.jsonc` or `opencode.json`, v1/v2 MCP layout |
| Copilot CLI | `.copilot/copilot-instructions.md` | `.copilot/skills` | `.copilot/mcp-config.json` |
| Cursor | No user-wide file adapter | `.cursor/skills` | `.cursor/mcp.json` |
| Crush | config directory `CRUSH.md` | config directory `skills` | `crush.json`, `mcp` |
| Grok Build | `.grok/AGENTS.md` | `.grok/skills` | `.grok/config.toml`, TOML tables |
| Pi | agent directory context file | agent directory `skills` | Native `mcp.json`; detected MCP adapter/extension settings |
| OmO | `.omo/agent` context file | `.omo/agent/skills` | `.omo/agent/mcp.json`, native Pi format |
| omp | `.omp/agent/AGENTS.md` | `.omp/agent/skills` | `.omp/agent/mcp.json` |
| Zed | No adapter | No adapter | native settings directory `settings.json`, `context_servers` |
| VS Code | No adapter | No adapter | native user directory `mcp.json`, `servers` |
| Claude Desktop | No adapter | No adapter | native config directory `claude_desktop_config.json`, stdio only |
| Kiro CLI | `.kiro/steering/AGENTS.md` | `.kiro/skills` | `.kiro/settings/mcp.json` |
| DeepSeek Harness | `.dsh/AGENTS.md` | `.dsh/skills` | No native YAML profile-patch adapter |
| Shared agent directory | `.agents/AGENTS.md` | `.agents/skills` | No adapter |

Windows and macOS use their native application-data locations for Zed, VS Code,
and Claude Desktop. Crush uses its local application-data directory on Windows
for MCP/skills and its XDG configuration directory for instructions. omp honors
`OMP_PROFILE`/`PI_PROFILE`, `PI_CONFIG_DIR`, and `PI_CODING_AGENT_DIR`; OmO honors
`OMO_CODING_AGENT_DIR` and `SENPI_CODING_AGENT_DIR`.
Codex/Pi/OmO instruction overrides are checked. Gemini/Qwen
custom context filenames are read from their settings. Pi native MCP requires a
version that implements it (0.99 or newer); the CLI does not infer an installed
binary's version from a directory. A detected `pi-mcp-adapter` uses
`mcp-adapter.json`; `pi-mcp-extension` uses its documented default-home file.
Conflicting extension registrations or disabled native MCP without an adapter
are reported explicitly.

## Ownership, conflicts, and rollback

`library-managed.json` records only what zcalld applied. JSON/JSONC MCP updates
preserve surrounding keys and comments. TOML servers occupy marked blocks;
unrelated tables remain unchanged. Skill copies record each file's SHA-256 and
executable bit. Instruction blocks retain the exact last written text.

An existing entry with a different definition is a conflict. An identical existing
JSON MCP entry is left unowned and reported in `unowned_equal_servers`. To explicitly
transfer responsibility for an identical entry, apply with `--adopt-existing`.
That permits future zcalld updates/removal of that entry. Existing unowned TOML
tables are not adopted by guessing their syntax or ownership.

```sh
zcalld library revert --agent claude --dry-run
zcalld library revert --agent claude
zcalld library mcp revert --agent codex
```

Revert withdraws managed content without deleting the stored library. It preserves
unrelated manual content and refuses externally changed managed content. Apply
and revert prepare every change before writing, take a private backup, and restore
earlier files if a write fails. A per-library process lock rejects concurrent CLI
writers; a changed target detected during preparation also stops the operation.
Other applications do not necessarily honor that lock, so close their settings
editors before applying a reviewed change.

## Migration and portable copies

The metadata remains `library.json`. Body files use the source layout:

- `library/instructions.md`
- `library/instruction-sets/ID.md`
- `library/instructions/AGENT.md`
- `library/skills/NAME/**`

The Magpie migration copies this body directory as well as metadata, safely
materializes internal links, and privately archives `mcp-signins.json`. It does
not apply the imported library to live agents or claim ownership of their files.
Linked external instruction sources retain their explicit paths.

```sh
zcalld library export --file ./private-library.json --assets ./library-assets
zcalld library import --file ./private-library.json --assets ./library-assets
```

Without `--assets`, import/export transfers metadata only. A full export can contain
MCP header/environment credentials; it is written privately and is not printed.
The imported Magpie `applied` record is preserved as source metadata. It is not
accepted as proof that zcalld owns a live agent file.

## Boundaries retained from the source audit

Magpie also provides market discovery, fetching/updating remote skills, WSL target
mapping, profile-specific DSH YAML patches, MCP OAuth sign-in, an MCP hub, skill
routing, cloud/peer synchronization, hooks, commands, subagents and rule automation.
Those execution paths are not implemented by this native library module. Imported
metadata and archived credentials are retained. Unsupported application is an
explicit error; enabled metadata-only automation flags appear in apply warnings.
This implementation is not a claim of complete Magpie library parity.

The audit of the development machine found six MCP entries (two stdio, four HTTP),
no skills or shared instruction assignments, no MCP OAuth/header accounts, and no
enabled library hub/router/shared-skill/automatic-update settings. No live files
were modified to run the tests.

## Validation and source references

```sh
cargo test -p zcalld maintenance::agent_library::tests --locked
cargo build -p zcalld --locked
python3 scripts/test-library.py
python3 scripts/test-control.py
```

Set `ZCALLD_TEST_BINARY` to test a specific built executable. The integration suite
uses temporary homes and synthetic MCP definitions; it does not connect to MCP
servers, start agents, run skill scripts, or make model requests. It covers CRUD,
assignment, transport shapes, native-file preservation, dry runs, removal,
conflicts, private exports, executable resources, link containment and locking.

Behavior was checked against the pinned upstream source
[`yetone/magpie` at `1e174f8f9f613a920203a4658d259456fcab5d0d`](https://github.com/yetone/magpie/tree/1e174f8f9f613a920203a4658d259456fcab5d0d):
[library schema](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/library.go),
[instructions](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/instructions.go),
[skills](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/skills.go),
[target paths](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/targets.go),
[MCP encoding](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/mcp.go),
[environment references](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/envref.go),
and [Pi MCP selection](https://github.com/yetone/magpie/blob/1e174f8f9f613a920203a4658d259456fcab5d0d/internal/library/pimcp.go).
