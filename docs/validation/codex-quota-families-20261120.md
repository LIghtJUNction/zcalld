# zcalld：按模型额度透传验收

日期：2026-11-20（Asia/Taipei）。

zcalld 已补齐 Codex 按模型区分的额度响应头。真实 Codex 0.170.0 的隔离验收从旧安装版的 **7/11** 提升至候选版的 **11/11**，原先失败的四项均为模型额度用例。修复源码已提交到 [`a4216ef`](https://github.com/LIghtJUNction/zcalld/commit/a4216ef24b5ce5b8375aaf2ab1415c47af02e536)。

## 改动及作用

上游会把“总额度”和“某个模型单独的额度”放在不同名称的 HTTP 响应头中。原实现列出了总额度的固定字段，模型额度因此在转发时被过滤，客户端收不到这些数据。

新的筛选规则保留默认 `x-codex` 额度族，以及 `x-codex-bengalfox`、`x-codex-other-v2` 这类命名额度族。每组可以包含主、次窗口各自的使用百分比、窗口时长、重置时间，以及额度名称；credits 继续使用共享的固定字段。依据为 [Codex 0.170.0 官方额度解析器](https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/codex-api/src/rate_limits.rs)。

规则按字段后缀精确匹配，名称大小写不影响识别，数值字符串保持原样。它同时用于 JSON、SSE，以及上游 429/503 响应；状态码、错误体和 Retry-After 一并保留。私密字段、错误后缀、空名称段和非 Codex 额度族继续被剔除。

头名匹配直接比较原字符串，省去了原先每个字段创建小写 String 的分配。请求转发仍使用原有的响应缓冲和 16 KiB 上游响应头总量限制。本轮没有新增 Cargo 依赖，也没有给整个网关作吞吐或内存降幅承诺。

## 验证结果

| 检查 | 结果 | 覆盖范围 |
| --- | ---: | --- |
| Rust workspace 测试 | 102/102 通过 | 核心及工作区插件、客户端和 SDK 测试 |
| 完整 HTTP 集成测试 | 22/22 通过 | 协议入口、认证、流式转发、插件、退出与禁止不确定请求重放 |
| 其中的额度专项场景 | 12/12 通过 | 两入口，JSON/SSE，429/503，精确值和私密字段过滤 |
| 真实 Codex 隔离验收 | 11/11 通过 | 默认额度、两组模型额度、API-key/ChatGPT、426 降级、zstd 与完整默认路径 |
| 格式及静态检查 | 通过 | cargo fmt；Clippy，warnings 视为错误 |
| 代码独立复核 | 无阻断项 | UTF-8 切片、名称分段、精确后缀、共享 credits、头大小上限 |

额度专项的 12 个场景属于上述 22 项 HTTP 测试中的两个测试方法，表格中的计数不相加。

同一套真实 Codex 脚本也测试了已安装的 0.3.1 二进制：原有七项保持通过，`codex_bengalfox` 和 `codex_other_v2` 在两种认证方式下的四项均因缺少额度快照而失败。候选版四项全部通过。这一对照把改善定位到了模型额度透传，而非路径或认证配置差异。

Codex 完成一次响应时，TokenCount 会保留最后一组额度更新。真实客户端测试因此为每个额度族单独建立一次用例，逐字段比较解析后的快照；HTTP 集成测试另外检查同一响应同时携带多组额度的情况。

完整的新旧客户端回执、二进制及脚本标识保存在 [codex-quota-families.json](../evidence/codex-quota-families.json)。回执记录独立网络命名空间、未继承真实凭据、外部模型请求数为零。测试使用临时目录和合成上游，未修改用户的 Codex 配置。

## 复现

在对应提交的源码目录中运行：

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release --workspace --locked
python3 scripts/integration.py
python3 -B scripts/test-codex-transport.py --binary target/release/zcalld
```

真实客户端验收需要 Linux、bubblewrap 和已安装的 Codex；脚本会创建独立网络环境。旧版对照通过同一脚本的 `--binary /usr/bin/zcalld` 参数执行。记录中的测试平台为 Linux x86_64，本轮没有新增 Windows/macOS 原生验收记录。

## 运行状态与后续范围

修复已经进入 GitHub main。候选程序使用 0.3.1 的版本字符串加上述修复提交；本轮没有创建新版本标签或替换已安装程序。验收时已安装服务为 0.3.1，systemd 状态 active/running，`zcalld health --json` 返回 `status=ok`。

此次验收覆盖响应头及真实客户端解析。独立 AccountUsage 接口、模型目录适配和交互界面的账户额度显示仍需各自完成验收；这些是后续兼容性工作的具体范围。

## 交付自检

- **作用**：关闭已在安装版复现的四项模型额度缺失。
- **可读性**：明确解释总额度、模型额度与独立账户查询的职责。
- **证据**：源码提交、同脚本新旧对照和完整回执可以相互核对。
- **覆盖**：成功、流式、拒绝响应和两种认证方式均有用例；跨平台实测范围已标明。
- **设计**：精确字段规则保留原有隐私边界，未新增依赖或后台轮询。
