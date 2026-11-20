# 生成请求断开后的取消与资源回收验收

日期：2026-11-20

## 结论与源码范围

本轮修复让普通生成请求在确认调用方断开后取消上游操作并释放 worker，同时保留写半关闭后继续读取完整响应的能力。相同的 16 个回归用例中，旧版 8 个取消用例全部失败、8 个半关闭用例全部通过；修复版 16 个全部通过。

| 角色 | 不可变提交 |
| --- | --- |
| 旧版基线 | [`33e5241386111f76e0a549cefb01fab50dc13cb9`](https://github.com/LIghtJUNction/zcalld/commit/33e5241386111f76e0a549cefb01fab50dc13cb9) |
| 已验证、已推送的修复 | [`6aea363671a9eb6a210cdf88dff22f34d1c829f8`](https://github.com/LIghtJUNction/zcalld/commit/6aea363671a9eb6a210cdf88dff22f34d1c829f8) |

验收环境为 Linux x86_64、Rust 1.100.0。测试使用本机合成上游，经过真实 libcurl 路径以及 Rust community 插件与 Bun host 路径；没有调用真实模型服务或使用真实账号。这里验证的是固定源码和隔离构建产物，不能据此判断已安装服务是否包含修复。

## 故障与修改

旧版的 HTTP 扩展已经能处理确认断开，但普通生成请求的等待路径缺少同样的检查。community 生成在等待响应头或响应体时只检查服务停止与超时；libcurl 的进度回调只检查服务停止。调用方发送 TCP RST 或完整关闭 Unix socket 后，安静的上游仍可能占用固定 worker，直到后续写入失败或整个上游请求超时。

修复在内部 `Sink` 接口加入取消检查。libcurl 进度回调通过已有同步 `Transfer` 上下文检查调用方状态，记录断开错误后终止传输；回调保护了空指针和 panic 边界。community 的响应头、响应体轮询执行相同检查，退出后由既有资源释放逻辑取消请求。响应写入器使用已有的借用连接视图，保持 socket 可观察，无需复制 socket 或增加线程。

开始下一次上游尝试前也会检查确认断开。取消产生错误结果，不满足“完整收到可重试的拒绝响应”的条件，因此不会触发生成重放。内部观察器沿用既有错误路径的 `502` 记账；本轮没有新增单独的调用方取消状态分类。

修改没有增加依赖，也没有改变公开 zcall、zplugin 或兼容层 ABI。实现和测试可从[完整差异](https://github.com/LIghtJUNction/zcalld/compare/33e5241386111f76e0a549cefb01fab50dc13cb9...6aea363671a9eb6a210cdf88dff22f34d1c829f8)核对，主要源码为 [`src/curl.rs`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/src/curl.rs) 和 [`src/server.rs`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/src/server.rs)。

## 结论与证据

专用回归矩阵覆盖两个上游路径、两个等待阶段以及 TCP/Unix 两种下游连接。取消用例使用 TCP RST 或完整 Unix 关闭；对应的半关闭用例只调用 `shutdown(SHUT_WR)`，仍然读取响应。

| 主张 | 判定依据 | 结果 |
| --- | --- | --- |
| 确认断开能取消真实上游操作 | 本机上游实际观察到 FIN/RST；测试不会把超时或清理动作算作取消成功 | 旧版 8/8 失败；修复版 8/8 通过 |
| 取消后能及时复用 worker | 仅配置一个 worker；上游超时为 8 秒；取消及健康请求恢复总耗时必须小于 2.5 秒 | 修复版全部满足门限 |
| 后续生成正常且没有重放 | 每例再执行一次生成，检查完整 SSE 结尾，并要求上游恰好收到两次请求 | 修复版全部通过 |
| 写半关闭保留有效响应 | TCP/Unix 在等待响应头和响应体阶段关闭写方向，仍收到完整响应 | 旧版和修复版均为 8/8 通过 |
| 修改没有破坏已有受测行为 | Rust workspace 108 个测试、HTTP 集成 22 个测试；格式、构建和 Clippy `-D warnings` | 全部通过 |

同一专用脚本在旧版完成 16 个用例耗时 23.231 秒，在修复版耗时 6.749 秒。这是测试运行时间，包含等待断言、进程启动与清理，不能据此计算生成延迟、吞吐提升或性能加速比。2.5 秒是本机合成场景的通过门限，不是对所有平台、负载或 libcurl 调度时机的延迟保证。

## 复现方法

准备两个独立工作树，分别固定到上述基线和修复提交，并初始化各自锁定的子模块。以下命令在修复工作树运行；旧版工作树使用相对位置 `../zcalld-baseline`。环境需具备 Rust 1.100.0、Python、Bun、系统原生库及已缓存的 Cargo 依赖；`--offline` 要求依赖缓存已齐备。

先在两个工作树分别构建各自二进制与插件：

```sh
cargo build --workspace --locked --offline
```

用修复提交中的同一份脚本验证旧版产物。预期 8 个取消用例失败、8 个半关闭用例通过：

```sh
python3 scripts/test-generation-cancellation.py \
  --binary ../zcalld-baseline/target/debug/zcalld \
  --community-plugin ../zcalld-baseline/target/debug/libzcalld_community.so \
  --bun "$(command -v bun)" \
  --community-host ../zcalld-baseline/extensions/compat/community/vendor/host.js \
  --community-pi ../zcalld-baseline/extensions/compat/community/vendor/pi.js
```

验证修复版产物及已有检查：

```sh
python3 scripts/test-generation-cancellation.py \
  --binary target/debug/zcalld \
  --community-plugin target/debug/libzcalld_community.so \
  --bun "$(command -v bun)" \
  --community-host extensions/compat/community/vendor/host.js \
  --community-pi extensions/compat/community/vendor/pi.js

cargo fmt --all -- --check
cargo test --workspace --all-targets --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
python3 scripts/integration.py --binary target/debug/zcalld --plugin-dir target/debug
```

测试实现见 [`scripts/test-generation-cancellation.py`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/scripts/test-generation-cancellation.py)。常规 [`scripts/check.sh`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/scripts/check.sh) 已接入直接上游用例；[`scripts/test-native-compat.py`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/scripts/test-native-compat.py) 已接入完整的 community 组合。仅传 `--binary` 时，脚本明确跳过 8 个 community 用例，不能将这种运行报告为完整的 16 项验收。

## 保留边界

普通 TCP FIN 仍可能表示调用方只关闭了写方向、正在等待读取响应。实现继续保留这一语义，不能把所有 FIN 都当作取消；本轮 TCP 取消证据来自 RST 或明确 socket 错误，Unix 取消证据来自完整挂断。连接判定继续复用 [`Stream::is_disconnected`](https://github.com/LIghtJUNction/zcalld/blob/6aea363671a9eb6a210cdf88dff22f34d1c829f8/src/transport.rs)。

本轮没有进行 Windows/macOS 原生运行验收，也没有测量真实上游的计费终止时刻。网关关闭请求能回收本机资源，但不能保证远端服务立即停止计算或停止计费。本报告不构成新版本发布、安装或运行服务替换的证明。
