# Usage history bounds and export validation

Date: 2026-11-20 (Asia/Taipei)

## Result

The usage CLI now skips an oversized history record before allocating its full contents.
A failed attempt to open the source history no longer creates an empty export destination.

The fix is published in [`2d19cb884425c8f9d365f23765ad64df31827ce6`](https://github.com/LIghtJUNction/zcalld/commit/2d19cb884425c8f9d365f23765ad64df31827ce6).
The [machine-readable evidence](../evidence/usage-bounds-20261120.json) contains the baseline failures,
candidate test output, build identity and environment. The implementation and regressions are in
[src/maintenance.rs](../../src/maintenance.rs) and [scripts/test-control.py](../../scripts/test-control.py).

## Why the changes were needed

The old summary loop used `BufRead::lines()`, which reads and allocates a complete line.
Its subsequent 1 MiB length check therefore limited what was parsed, but did not limit
what was allocated. A single malformed or unusually large history record could exhaust
the CLI's memory before the skip decision.

The new loop reads at most 1 MiB plus two bytes for CRLF. If that bounded prefix has no
newline, it discards the remainder through the next newline without retaining it. The
following record can then be parsed normally. It retains the existing 1 MiB content
limit, LF/CRLF handling, a final record without a newline, legacy token field names and
errors for invalid UTF-8 in retained records. Blank or invalid JSON records remain ignored.

The export path previously created its destination before opening `usage.jsonl`.
A missing source left an empty destination that blocked a later retry because exports
use `create_new`. Opening the source first removes that failure side effect.
Existing destinations remain protected, exported bytes remain unchanged and newly
created files retain Unix mode `0600`.

## Fixed inputs

| Input | Identity |
| --- | --- |
| Baseline source | `5cdfcaedab4a2d02b7d3b341528606c47a92e1bf` |
| Final validation source | `1aaff74a64fefa9db3bcf8b7b085024cf33da0a4` plus the two-file usage patch |
| Published parent | `b46cb34f91358305e633e96289d86a6aafd2ce4a` |
| Published fix | `2d19cb884425c8f9d365f23765ad64df31827ce6` |
| Candidate binary SHA-256 | `e3392ab9e2d8056e9acde4cb9010ec479f11c18d5350f2df95dfe0f440de644b` |
| Platform | Linux x86_64 |
| Toolchain | Rust 1.100.0; Python 3.14.10 |

The final main update between validation and publication added only the two existing
Codex acceptance evidence files. It did not change the tested code or test harness.
The baseline's usage implementation was unchanged by the intervening Codex work.

## Reproduction and verification

The five new regressions were first run against the baseline binary. Three passed and
two failed, matching the two intended fixes:

| Regression | Baseline | Candidate |
| --- | --- | --- |
| 128 MiB history line, with a 96 MiB child address-space limit | Aborted with return code `-6`; allocation of 134217728 bytes failed | Completes and counts the following valid record |
| Missing source before export | Fails but leaves an empty target | Fails without creating a target; a subsequent export to that path succeeds |
| Content at 1 MiB − 1, 1 MiB, and 1 MiB + 1, with LF, CRLF or EOF | Passes all nine combinations | Passes all nine combinations |
| Invalid UTF-8 in a retained record | Returns an error | Returns an error |
| Export destination already exists | Refuses to overwrite it | Refuses to overwrite it |

The memory fixture writes synthetic history in 1 MiB chunks. Only its CLI child receives
the 96 MiB address-space limit; core dumps are disabled for that child. The fixture also
checks that a following valid record is counted and that an oversized final record is
discarded. This is a bound check, not an RSS benchmark.

Final local gates:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo build --bin zcalld` | Pass |
| `ZCALLD_TEST_BINARY=target/debug/zcalld python3 scripts/test-control.py -v` | 18 / 18 pass |
| `cargo test --workspace --all-targets` | 108 / 108 pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `git diff --check` | Pass |

An independent read-only review checked the size boundary, EOF and CRLF cases and the
position of `skip_until`; it found no blocking defect. In particular, a newline already
consumed by the bounded read does not cause the following record to be skipped.

## Scope and practical limits

All CLI fixtures use disposable homes and synthetic history. The existing control suite's
provider-discovery fixture uses a local HTTP server. No real model provider or account
quota endpoint was contacted, and the user's Codex configuration was not edited.

The address-space regression is Linux-only. The portable boundary and export tests were
executed on Linux; this report makes no claim of a new Windows or macOS validation run.
Summary time remains proportional to the number of bytes scanned.

The export fix covers failure to open the source before creating the destination.
A failure after copying starts may still leave a partial output file; this change does
not claim transactional export semantics.

The installed service was checked separately and remained active/running at version
`0.3.1`, with health `{"status":"ok","version":"0.3.1"}`. This iteration publishes source
and validation evidence; it does not install a new binary, restart the service or create a release tag.
