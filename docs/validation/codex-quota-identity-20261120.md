# Codex quota identity isolation

Date: 2026-11-20 (Asia/Taipei)

## Result

The subscription-credit guard now binds quota state to the selected workspace and
the credentials actually used to check it. One workspace member can no longer reuse
another member's cached allowance, pending lookup or observed hold. A credential
change during the check stops generation before it is sent.

The compatibility fix is [739ecef91bcec41d44b2388d7c346fe2bad934a1](https://github.com/LIghtJUNction/zcalld-compat/commit/739ecef91bcec41d44b2388d7c346fe2bad934a1).
It preserves the first-hit privacy work already merged in its parent,
`7043aa157462fa445163849c3f3156a0f95b3904`. The focused native acceptance uses
core `6aea363671a9eb6a210cdf88dff22f34d1c829f8` with this compatibility commit.
Mainline later incorporated the fix through compatibility pin
`812f8f4b6b3700d8fca371407e527b17c7bb55c5` in
[core 7ec8ecf](https://github.com/LIghtJUNction/zcalld/commit/7ec8ecf8e83e57716158a7a393cda53546a1d15c).
That subsequent desktop integration has its own validation; the matrix below
records the focused acceptance inputs.

[Raw validation evidence](../evidence/codex-quota-identity-20261120.json) records
the source identities, baseline failures, candidate outputs and native results.
The [quota fixture](https://github.com/LIghtJUNction/zcalld-compat/blob/739ecef91bcec41d44b2388d7c346fe2bad934a1/community/builtin/tests/codex-credits.test.mjs)
contains the five new regressions.

## Cause and correction

The old guard used the ChatGPT account ID as its cache and concurrent-lookup key.
That field identifies a workspace. The official
[Codex 0.170.0 token contract](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/login/src/token_data.rs)
separately defines the user and workspace identifiers. Two members of one workspace
therefore need separate quota state.

The same gap affected the check immediately before generation: it compared only
workspace IDs. Credentials could change from member A to member B while A's usage
request was in progress, and B's generation could then proceed using A's allowance.

The fix makes three related changes:

1. A SHA-256 digest of the workspace and resolved access token identifies quota
   state. Cached windows, pending lookups, revision checks and observed response
   holds all use that same identity. Optional JWT user claims are not required,
   so opaque tokens are also isolated.
2. The usage request receives one frozen snapshot of the resolved credentials.
   The metadata path cannot reread a changed external authentication file and
   place another member's result under the original identity.
3. Immediately before generation, current authentication is resolved again and
   compared with that snapshot. Generation and response observation then use
   the original verified snapshot.

The cache still has a one-second lifetime and a 64-identity bound. Existing
deadline, byte-limit, cancellation and waiter behavior remains in place.
The stored per-account policy is independent of the short-lived credential cache.

## Reproduced behavior

Both original reproduction scripts use the actual adapter loader with fixture
authentication and in-memory upstream responses.

| Scenario | Before | After |
| --- | --- | --- |
| A and B share a workspace; A has capacity and B is exhausted | Statuses `[200, 200]`; only A's usage is read; both generation requests are sent | Statuses `[200, 429]`; each member's usage is read; only A's generation is sent |
| Authentication changes from A to B during A's usage lookup | A's usage is read, then B's generation is sent | An identity-change error is returned before any generation |
| Registry token is stale and the external credential file changes | Metadata can resolve a different member under the old check | Usage retains the resolved snapshot; the final identity check rejects the change |

Additional regressions cover separate concurrent lookups for workspace members,
response-header holds that must not cross identities, opaque tokens, and token
rotation within the same member.

## Verification

The five new tests were first run with the baseline production code at
`15e5e4960adb8028b2d099c1835eaa6a2faaa0c8`. All fifteen existing quota tests passed;
all five new regressions failed. The reviewed patch then passed the same twenty
tests and the complete builtin suite. The complete builtin suite was run again
on the exact published compatibility commit after the concurrent privacy merge.

| Check | Result |
| --- | --- |
| Baseline quota suite with new cases | 15 pass; 5 fail |
| Fixed quota suite | 20 / 20 pass |
| Builtin suite on exact `739ecef` | 49 / 49 pass; 374 assertions across 6 files |
| Native Rust workspace build | Pass |
| Go compatibility helper build, cached dependencies only | Pass |
| Native subscription-credit policy suite | 11 / 11 pass |
| Native account metadata suite | 17 / 17 pass |
| Patch whitespace check | Pass |

The 20 quota cases are included in the 49 builtin cases. The native suites exercise
the generated embedded modules through real Rust/Go/Bun IPC. Policy checks include
account selection, runtime policy changes, account disablement during a lookup,
cancellation, malformed or missing availability, and privacy-preserving group
fallback. The metadata suite checks its binding, path and response contracts.

An independent read-only review found no blocking defect in the identity binding,
snapshot use or cancellation lifecycle. Tests use synthetic credentials, private
temporary files and local or in-memory network substitutes. No real provider
account was queried and the user's Codex configuration was not edited.

## Operational behavior and limits

Token renewal invalidates the short quota cache. If renewal happens while a quota
check is in progress, that request is conservatively rejected; the next request
uses the new credentials and checks again. No automatic retry or background token
refresh is added.

This closes a local identity-binding defect. The existing client-side quota check
still cannot make quota lookup and provider billing one atomic operation. Another
client or a request that crosses the remaining window can change availability
after the check.

This iteration publishes code and validation evidence. The mainline integration
retains its newer compatibility pin.
It does not change release versions, create a tag, install a binary or restart
the service. The installed service was separately observed active/running at
version `0.3.1`, with health `{"status":"ok","version":"0.3.1"}`.

The [usage CLI validation](usage-bounds-20261120.md) documents the separate bounded
history-reader and failed-export fixes completed in the same iteration.
