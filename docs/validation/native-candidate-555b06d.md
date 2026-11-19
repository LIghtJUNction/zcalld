# Native candidate acceptance

This record covers the exact source snapshots named below. It establishes native build and runtime acceptance for the core transport and GUI; the later 0.2.0 feature freeze and the installed Magpie cutover require their own results.

## Core transport

Core source **555b06d6c4edc33f0d2b3bf58d90964244cf03ca** passed all five native build jobs in [release candidate 44636108505](https://github.com/LIghtJUNction/zcalld/actions/runs/44636108505).

| Target | Native result |
| --- | --- |
| Linux x86_64 | Passed |
| Linux aarch64 | Passed |
| Windows x86_64 MSVC | Passed |
| macOS aarch64 | Passed |
| macOS x86_64 | Passed |

The jobs check formatting, run Clippy with warnings denied, run workspace tests, build release binaries and assemble platform archives. The Windows and macOS jobs also execute [the native runtime fixture](../../scripts/test-native-runtime.py): a real local HTTP upstream, JSON generation, SSE streaming, dynamic alias and policy plugins, rejection of a forbidden route, and separation of caller authentication from the upstream provider key. The fixture makes no request to a real model provider.

The Windows artifact uses the pinned vcpkg curl SSL feature with Windows Schannel. A successful cross-compilation alone would not verify the DLL loader or HTTP runtime; this candidate executes both on its native runner.

These results precede later instruction/skill library and advanced routing additions. They must not be attributed to an untested later commit or to a live account.

## Native GUI

GUI source **90cedb0ecaaf7297a6f1d5ce135217db2a74416f**, with zui source **5e37fcfb791eec9ac523fc19373e95d0a513112f**, passed the five native targets in [GUI candidate 44636510212](https://github.com/LIghtJUNction/zcall-gui/actions/runs/44636510212). Its gates include an actual native window smoke check. macOS runners used Apple Software Renderer; the Windows runner used GDI Generic. Those results prove the exercised platform path, not hardware GPU throughput.

The final GUI source **4d0267f229c3fea7277369e50404a455d8dabc06** adds an explicit distinction between saved privacy settings and unknown runtime state after reopening the application. It keeps the same zui pin. Its local Clippy, 15 Rust tests and release build passed; its formal release must run the complete native gate for that exact commit.

### Hardware observation

A separate observation used GUI source 90cedb0 on the device's actual Hyprland/Xwayland session, with temporary configuration and a read-only CLI fixture. It started and closed only its own GUI process.

| Measurement | Observation |
| --- | --- |
| Renderer | Mesa Intel UHD Graphics, ADL-S GT1 |
| Actual tiled window | 1224 × 1526 |
| RSS | 61,968 KiB |
| PSS | 50,871 KiB |
| Private mappings | 46,476 KiB |
| Swap | 0 |
| Stable idle interval | 4.001 seconds |
| CPU ticks during that interval | 0 |
| Presentations over the 12-second lifetime | 6 |

These are process CPU address-space measurements. They do not include all GPU allocations or driver memory. They are an idle observation at the stated window size, not a throughput benchmark or a guarantee for every page, driver or display.

PSS apportions shared mapped pages among processes; it is useful when comparing a native application with applications that also use shared graphics libraries. RSS counts all mapped resident pages for each process and therefore must not be summed as though shared pages were private.

## Dependency and publication boundaries

The shared zui runtime fix is 9eac4c9a2232b6cdb82a29bf6adf94f96e9412bb; final 5e37fcf adds its maintenance rule. [The window-property evidence](https://github.com/LIghtJUNction/zui/blob/5e37fcfb791eec9ac523fc19373e95d0a513112f/docs/window-properties.json) covers caller-specific window identity and requested sizes of 660 × 264, 1080 × 740 and 512 × 320.

Core source pins zplugin 0.3.1 at 00d566eedcff7b2932e90b9dfeb44910c783d305 and zsys 0.1.1 at 808fcfdb73584ca7ca122e20b76f0209f56ab0f7. The loader is shared through zsys. The GUI and core remain separate applications; retaining GUI sources as a submodule does not link GUI code into the daemon.

Crate publication for zui-native 0.1.0 belongs to the Project Z coordinator. Downstream owners must return source pins, lockfile state, validation and pushed commits, including the outer source-distribution relationship. [The maintenance rule](../MAINTENANCE.md) requires that evidence before propagation can be called complete.

No live Magpie replacement, real provider availability, installed package version or zero-downtime result is claimed by this candidate record.
