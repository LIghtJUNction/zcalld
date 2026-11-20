# zcalld 模型元数据与请求取消验收

日期：2026-11-20（Asia/Taipei）。

本轮修复了模型升级提示与本地别名不一致、HTTP 扩展在客户端明确断开后继续占用请求两个问题，并补齐真实 Codex 客户端的额度与模型目录验收。本轮运行的 **180 项测试全部通过**，格式检查与 Clippy 通过。

- [zcalld 实现提交](https://github.com/LIghtJUNction/zcalld/commit/1aaff74a64fefa9db3bcf8b7b085024cf33da0a4)
- [兼容扩展提交](https://github.com/LIghtJUNction/zcalld-compat/commit/15e5e4960adb8028b2d099c1835eaa6a2faaa0c8)
- [结构化验收结果](../evidence/codex-metadata-cancellation-20261120.json)

## 实现与行为

### 模型升级提示与本地路由一致

模型目录使用本地别名时，升级目标也需要使用客户端能找到的名称。兼容扩展现在将 `upgrade.model` 映射到目录中已展示的原始名称，或它唯一的本地别名。目标未展示或多个别名无法唯一选择时，`upgrade` 设为 `null`。上游说明、推理能力与升级指导保留。

回归覆盖唯一别名、未绑定目标、上游未提供目标、多个别名和原始名称仍可用五种情况。原生网关测试同时验证生成后的 Go/Bun 运行时已包含此行为。

### 明确断开后释放当前请求

HTTP 扩展等待响应头或响应数据期间，TCP RST、明确 socket 错误和 Unix socket 完整关闭会释放当前请求并触发既有取消链。执行线程和共享 Bun host 可以立即处理后续请求。实现使用 `&Stream` 的借用连接视图，没有复制 socket 或增加依赖。

取消回归由旧版的 **2/4** 提升至 **4/4**。测试要求一秒内观察到取消，并检查后续请求在同一 host 上于一秒内完成。TCP 和 Unix 客户端只关闭写方向时，仍可读取完整响应。普通 TCP FIN 保留可能的写方向半关闭语义，继续按响应和正常截止时间处理。

### 通过真实 Codex 解析元数据

新验收运行实际 Codex 0.170.0 的 app-server，仅发送初始化、账户额度读取和模型列表读取 RPC。客户端实际解析到 **37% 使用量、300 分钟窗口、1900000000 重置时间**，以及绑定的 `public-alias` 模型和推理字段。这些均为合成上游的固定测试值。

共用测试数据已补齐官方必填字段，并修正账户标识字段。负例将使用百分比改为不符合整数类型的 37.5：即使 HTTP 返回 200，真实客户端仍返回 RPC 错误。测试因此同时检查传输结果与客户端解析结果。

## 验证结果

环境为 Linux x86_64、Rust 1.100.0、release 构建；实际运行 Go/Bun 兼容助手和 Codex 0.170.0。

| 检查 | 结果 | 验证范围 |
| --- | ---: | --- |
| Rust workspace | 108/108 | 核心、插件与兼容客户端单元测试 |
| Bun 元数据 | 13/13 | 目录映射、账户边界、取消与响应校验 |
| 原生元数据 | 17/17 | Rust → Go/Bun → HTTP 路径及模型升级别名 |
| HTTP 请求取消 | 4/4 | TCP RST、Unix 完整关闭、两种写方向半关闭 |
| MCP | 14/14 | 工具调用、共享 host、响应限制和关闭行为 |
| HTTP 集成 | 22/22 | 普通生成、协议转换、认证、额度响应头和流式传输 |
| 真实 Codex 客户端 | 2/2 | 合法额度/模型解析与畸形额度拒绝 |
| 格式与 Clippy | 通过 | workspace、所有 Rust targets、警告视为错误 |

这里的 Clippy `--all-targets` 指 Rust 的程序、测试和示例等构建目标；平台实测范围为 Linux。

## 复现

在主仓库根目录准备 release 网关、插件及兼容助手后，执行以下 Linux 命令。真实客户端验收额外要求 Codex 0.170.0 已安装。

```bash
zcalld_test_args=(
  --binary target/release/zcalld
  --community-plugin target/release/libzcalld_community.so
  --pluginctl _release/native-compat/zcalld-pluginctl
  --redact-plugin target/release/libzcalld_redact.so
  --privacy-worker _release/native-compat/zcalld-privacy
  --routing-plugin target/release/libzcalld_plugin_routing.so
)
for zcalld_test_script in \
  scripts/test-codex-metadata.py \
  scripts/test-http-cancellation.py \
  scripts/test-codex-client-metadata.py
do
  python3 "$zcalld_test_script" "${zcalld_test_args[@]}" || exit
done

python3 scripts/test-mcp.py \
  --binary target/release/zcalld \
  --pluginctl _release/native-compat/zcalld-pluginctl \
  --community-plugin target/release/libzcalld_community.so
python3 scripts/integration.py \
  --binary target/release/zcalld --plugin-dir target/release
cargo test --release --workspace --locked --jobs 2
cargo fmt --all -- --check
cargo clippy --release --workspace --all-targets --locked --jobs 2 -- -D warnings
```

在 `extensions/compat` 目录执行：

```sh
bun test community/builtin/tests/codex-metadata.test.mjs
```

便携原生验收脚本 `scripts/test-native-compat.py` 已包含请求取消回归。安装版 Codex 验收保持独立执行，适用于已安装相应客户端的环境。

## 验证边界与安装状态

本轮使用真实客户端和合成上游，测试涉及的身份与额度均为虚拟数据；没有请求真实账户额度或启动模型生成。用户现有 Codex 配置保持原样。

本轮完成 Linux 本机验证，其他平台尚未执行同等实测。当前系统安装服务为 **zcalld 0.3.1，active**；这轮实现与验收已进入源码主线，未发布或替换系统安装包。
