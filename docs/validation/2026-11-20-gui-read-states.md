# Native GUI state and retry validation

Date: 2026-11-20 (Asia/Taipei)

## Result

The GUI now distinguishes an initial read, an unavailable response, a confirmed disconnection and a healthy gateway. A failed read no longer appears as an empty configuration, disabled privacy protection or an invented endpoint. Retry actions reload the existing local state; a successfully read empty catalog still offers provider setup.

The [published GUI change](https://github.com/LIghtJUNction/zcall-gui/commit/da6b56d2cd286a9bf699cb3ee3d0a4f7db755e9f) preserves the existing Ink palette, dependency pin and application version. State is derived from the local snapshot; no background polling was added.

## What changed

`src/view_state.rs` centralizes the state classification used by `src/main.rs`, `src/pages.rs` and `src/dialogs.rs`.

- **Initial read:** pages show a loading state, with unknown counts and endpoint. They do not claim that the gateway is disconnected or that a list is empty.
- **Completed but unusable read:** missing fields, incorrect types and explicit errors show an unavailable state. A completed `null` response is distinct from a snapshot that has not arrived yet.
- **Known gateway state:** explicit boolean health results select connected or disconnected views. The service drawer offers retry while state is unknown. An endpoint is displayed only when the status response supplies a nonblank string.
- **Lists and routes:** only a successfully read array can be treated as empty. A failed model catalog offers retry; a valid empty catalog offers provider setup. Non-string and blank route IDs cannot become selectable routes.
- **Privacy and versions:** unknown privacy state is not presented as disabled protection. A gateway version that has not been established is shown as unknown rather than missing software.

All four language catalogs contain the same **205 unique, nonempty keys**, including the new state and retry messages.

## Fixed validation inputs

| Input | Identity |
| --- | --- |
| GUI baseline | `92cbaedce3c378331d44bef568c04485bad74ee7` |
| Pinned zgpui source | `7838c3c94cf018c02bb36e17c941ca651c5c0e24` |
| Integration CLI | Actual zcalld build from source revision `6aea363671a9eb6a210cdf88dff22f34d1c829f8` |
| GUI binary SHA-256 | `7462df8163c1027ca0c69c3a91084f9735b670100e069dd87929bbb80413aaf9` |
| Native display | Linux, private Xvfb display, llvmpipe software rendering |

## Verification

| Check | Observed result |
| --- | --- |
| GUI Rust tests | **27 passed**, including **8 new state-classification tests** |
| Strict Clippy | Passed with warnings denied |
| Release build | Passed; binary identity recorded above |
| `scripts/ui_state_smoke.py` | **11 state scenarios** completed; **3 retry paths** refreshed successfully; state and recovery screenshots captured |
| Existing `scripts/ui_smoke.py` with the actual zcalld CLI | Passed; **58 CLI invocations** recorded |
| Language catalog consistency | 205 matching unique, nonempty keys across Simplified Chinese, Traditional Chinese, English and Japanese |

The eight new Rust tests cover initial reads, malformed or failed status, explicit health results, observed endpoint values, valid empty arrays, overview setup versus retry, failed versus empty model catalogs, and unknown privacy state.

The native state fixture delays its initial response to capture loading, then exercises unavailable, disconnected, connected, unknown-endpoint, list, catalog and privacy states. Its three recovery cases activate the actual retry controls. It requires a new six-command snapshot read and checks every recorded invocation against a read-only allowlist; no configuration write or service action is accepted.

The existing native interaction suite used the newly built zcalld CLI in a disposable home. It verified provider creation and saved state, model search and selection, modal isolation, tool configuration apply/restore, preservation of the previous tool configuration, privacy settings, language/theme persistence and restoration of the dark theme after reopening. **The 58 invocations are an activity count, not 58 independent test cases.**

## Reproduction

Run from a checkout containing the pinned submodule:

```sh
cargo fmt --all -- --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline

xvfb-run -a -s '-screen 0 1100x740x24 -nolisten tcp' \
  python3 scripts/ui_state_smoke.py target/release/zcall-gui \
  --output _validation/state-ui-dark --theme dark --language zh-CN

xvfb-run -a -s '-screen 0 1100x740x24 -nolisten tcp' \
  python3 scripts/ui_smoke.py target/release/zcall-gui \
  --real-cli /absolute/path/to/zcalld \
  --output _validation/real-cli-state-pass
```

The state fixture writes screenshots and `state-ui.json` beneath the specified output directory. The report records the tested binary hash, renderer diagnostics and read-only call counts. All 14 native screenshots were also inspected at 1080 × 740. No text clipping, overlaps or misleading unknown/empty states were found. This static visual inspection is separate from the successful retry and real-CLI input automation.

## Scope and review

These checks establish Linux native UI behavior with isolated local data. The state fixture uses a synthetic CLI; the integration suite uses the real zcalld CLI with disposable configuration. Neither suite calls a remote model. The display evidence uses software rendering and does not measure hardware-GPU performance. Windows/macOS execution, packaged installation and release publication are separate validation steps.

| Review dimension | Evidence and boundary |
| --- | --- |
| Contribution | Removes misleading status and setup guidance after incomplete or failed reads. |
| Clarity | Unknown, unavailable, disconnected and genuinely empty states have distinct meanings and actions. |
| Evidence strength | State tests, actual native retry interactions and the existing real-CLI workflow passed. |
| Coverage | Loading, malformed data, failures, empty data and successful recovery are covered; platform scope remains Linux. |
| Design | Shared snapshot classification preserves the existing rendering and dependency model without adding periodic work. |

## Machine-readable evidence

- [State fixture report](../evidence/2026-11-20-gui-read-states.json)
- [Actual-CLI interaction report](../evidence/2026-11-20-gui-real-cli.json)
