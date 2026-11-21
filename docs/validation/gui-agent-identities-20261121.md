# zcall-gui Agent 身份与布局验收记录

- 日期：2026-11-21
- 仓库：`LIghtJUNction/zcall-gui`
- 验收提交：[`73fc1fda9c66d34b903ea691dd0ae4377b49c846`](https://github.com/LIghtJUNction/zcall-gui/commit/73fc1fda9c66d34b903ea691dd0ae4377b49c846)
- 交付状态：源码已推送至 GUI 仓库 `main`；zcalld 通过 `apps/zcall-gui` 固定该提交。本轮为源码集成，未发布安装包。

## 结论与适用范围

本轮完成 Agent 身份图标、字体、配色和列表布局更新。
Linux 原生构建、Rust 测试、静态检查及下列 UI/交互验收通过。
此结论对应 GUI 源码集成，不代表 Project Z 聚合快照已接受该提交。

验证平台限于 Linux 原生默认 OpenGL/Mesa 路径。
本轮没有验证 Windows、macOS 或 CPU software renderer。

## 变更范围

| 项目 | 本轮行为 |
| --- | --- |
| 16 个 Agent Logo | 为已知 Agent 显示对应品牌图形，未知身份保留回退显示 |
| 原生字体 | 使用原生默认无衬线字体呈现正文与控件文字 |
| 中性配色 | 深浅主题使用克制的中性色，保持文字和状态可辨 |
| 响应式紧凑行 | 图标、名称和状态分别呈现，覆盖宽窗口与窄窗口 |
| 分页 | Agent 列表分批显示，支持翻页访问后续身份 |
| 未安装状态优先 | 未安装的 Agent 优先显示未安装状态 |

16 个已知身份为：Claude Code、Codex、Gemini CLI、OpenCode、Pi、
Qwen Code、Aider、Goose、Zed、VS Code、Cursor、Amp、Continue、Cline、
Roo Code、Kilo Code。验收另加入 1 个未知身份，检查回退显示。

素材来源、版本、文件摘要和许可记录在 GUI 的 `assets/agents/sources.json`，
对应许可材料随源码保存。运行时使用内嵌矢量网格，不下载图片。

本轮 Cargo 依赖、zcalld CLI 和 API 均保持不变。

## 构建与静态检查

| 验证项 | 结果与证据 |
| --- | --- |
| Linux native Rust 测试 | 36 个测试通过 |
| Linux native release build | 构建通过 |
| 沙箱同源 Clippy | 下列离线检查通过，警告按错误处理 |
| 格式与差异检查 | fmt、diff 检查通过 |

```sh
cargo clippy --locked --offline --all-targets -- -D warnings
```

Clippy 在沙箱对同源代码执行；原生构建与交互验证在开发环境执行。
主要 Rust 文件和两个 smoke 脚本的 SHA-256 在两处一致。

## UI 矩阵

每组输入均包含 17 个身份：16 个已知身份和 1 个未知身份。
每组启动过程记录到 6 次只读 CLI 调用。

| 窗口尺寸 | 主题 | 语言 | 身份数 | 只读 CLI 调用 |
| --- | --- | --- | ---: | ---: |
| 1080 × 740 | dark | zh-CN | 17 | 6 |
| 1080 × 740 | light | zh-CN | 17 | 6 |
| 1080 × 740 | dark | en | 17 | 6 |
| 1080 × 740 | light | en | 17 | 6 |
| 720 × 560 | dark | ja | 17 | 6 |
| 640 × 560 | light | en | 17 | 6 |
| 900 × 640 | light | zh-TW | 17 | 6 |

上述矩阵覆盖深浅主题、四种语言及宽窄窗口下的身份列表、分页和状态显示。
矩阵使用受控身份数据；真实安装环境的交互验证另列如下。

## 交互与配置验证

| 验证项 | 结果与边界 |
| --- | --- |
| Fixture 完整流程 | 通过；记录 60 次 CLI 调用 |
| 已安装 zcalld 完整流程 | 对真实 zcalld 0.4.4 验证通过；记录 52 次调用 |
| 临时 HOME 下 apply / restore | 恢复后配置字节与原始样本一致，权限保持 `0600` |
| 键盘操作 | 路由选择器与确认流程通过 |
| Catalog 错误路径 | 加载失败时禁用 apply；重试路径通过 |
| 偏好持久化 | 隐私、主题、语言设置的持久化检查通过 |
| 空闲 CPU | 2 秒观察窗口内进程 CPU 时钟计数增量为 0 ticks |

临时 HOME 测试隔离配置写入；字节和权限结论针对该测试样本。
空闲检查覆盖上述观察窗口，不构成长期资源占用测量。

## Project Z 集成状态

本轮核对时，Project Z `main` 为 `be64e5a`；其现有 policy 拒绝内置 Git 依赖。
因此不能将 GUI 提交成功描述为聚合仓库已经接受更新快照。
该迁移由已有 [draft PR #7](https://github.com/LIghtJUNction/project-z/pull/7) 承接：

- 分支：`coord/git-source-policy-20261120`
- 已知 head：`c9de7a60351b277a4903c9f40781cbae69cd6e5c`

在当前提交的 catalog 上调用 impact 函数，返回 `zcall-gui`、`zcalld`、`zcall-omarchy`、`zcalld-compat`。
后两者只有 runtime 关系；本轮没有接口变化，无需代码适配。
本记录提供 GUI 验收证据，聚合快照接受状态仍以该迁移的实际结果为准。

## 证据入口

- [GUI 验收提交](https://github.com/LIghtJUNction/zcall-gui/commit/73fc1fda9c66d34b903ea691dd0ae4377b49c846)
- [身份列表 smoke 脚本](https://github.com/LIghtJUNction/zcall-gui/blob/73fc1fda9c66d34b903ea691dd0ae4377b49c846/scripts/agent_identity_smoke.py)
- [完整 UI smoke 脚本](https://github.com/LIghtJUNction/zcall-gui/blob/73fc1fda9c66d34b903ea691dd0ae4377b49c846/scripts/ui_smoke.py)
- [Logo 来源与许可清单](https://github.com/LIghtJUNction/zcall-gui/blob/73fc1fda9c66d34b903ea691dd0ae4377b49c846/assets/agents/sources.json)
- [Project Z Git source policy 迁移 PR](https://github.com/LIghtJUNction/project-z/pull/7)
