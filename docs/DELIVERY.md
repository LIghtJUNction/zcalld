# zcalld v0.1.1 交付报告

交付日期：2026-11-20（Asia/Taipei）。

zcalld 已在开发机完成实现，并通过真实的 patch bump 工作流发布 v0.1.1。GitHub Release 提供 x86_64、aarch64 两种原生 Linux 安装包；AUR 源码包 `zcalld` 为 `0.1.1-1`，预编译包 `zcalld-bin` 为 `0.1.1-2`。项目采用 MIT 许可证。

## 交付位置

| 内容 | 位置 |
| --- | --- |
| 源码与日常开发 | [LIghtJUNction/zcalld](https://github.com/LIghtJUNction/zcalld) |
| 正式版本与校验文件 | [GitHub Release v0.1.1](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.1.1) |
| AUR 源码包 | [zcalld](https://aur.archlinux.org/packages/zcalld) |
| AUR 预编译包 | [zcalld-bin](https://aur.archlinux.org/packages/zcalld-bin) |
| 完整发布工作流记录 | [Bump and release #44600439004](https://github.com/LIghtJUNction/zcalld/actions/runs/44600439004) |
| 开发机目录 | `/home/lightjunction/Documents/GITHUB/zcalld` |

正式版本对应提交 `8fa0382c2c58aa0ff187d4c5b91164ec65c884a5`。后续交付文档补充保存在 `main`；发布标签和安装包保持不变。

## 实现范围与设计取舍

一个本地 AI 网关实际需要完成四件事：接收客户端请求，选出上游，按需要转换协议，再把上游结果送回客户端。zcalld 的常驻核心围绕这四件事实现；模型运行和收费判断交给上游。

| 层次 | 实现 | 作用 |
| --- | --- | --- |
| CLI 与守护进程 | Rust 原生可执行文件 | 配置校验、启动服务、列出路由和插件、生成客户端环境变量 |
| 协议语义 | `zcall 0.3.0` | 请求、响应和 SSE 的跨协议转换 |
| HTTP 与 TLS | 系统 libcurl | 每个 worker 复用上游连接，流式传输；无需启动 curl 子进程 |
| 插件机制 | `zplugin 0.3.0` 与版本化 SDK | 加载显式配置的原生 `.so`，提供路由、请求和观察回调 |
| 可选别名插件 | `libzcalld_aliases.so` | 将简短名称映射为配置中的公开路由 |
| 可选策略插件 | `libzcalld_policy.so` | 模型白名单、请求体上限和进程内计数 |

核心没有 GUI、浏览器运行时、JavaScript 运行时、数据库、价格表、订阅套餐或付费解锁判断。插件按配置顺序运行；未启用的插件不会加载。每个 worker 独立创建插件实例，加载的库保持有效直到进程退出。原生插件属于可信代码，版本化 ABI 负责兼容性检查，不提供进程隔离。[架构说明](https://github.com/LIghtJUNction/zcalld/blob/main/docs/ARCHITECTURE.md)与[插件 SDK 说明](https://github.com/LIghtJUNction/zcalld/blob/main/docs/PLUGINS.md)给出了接口和约束。

支持 OpenAI Chat Completions、Responses、Completions、Anthropic Messages、Gemini GenerateContent/SSE、Gemini Interactions 六类协议，以及 `/v1/models`、`/healthz`。相同协议保留供应商特有字段；跨协议转换使用严格模式，无法表达的内容会明确报错。不能把六类协议入口理解为任意模型、任意扩展字段之间的无损互转。

客户端访问网关的密钥与网关访问上游的密钥分别配置。网关重新构造上游认证信息，不转发客户端的凭据、Cookie 或查询字符串密钥。绑定非回环地址时要求配置访问令牌。请求上传开始后禁止自动重放生成请求，避免连接异常触发不透明的重复调用。

## 内存与性能

节省内存主要靠限制常驻工作和资源上限：默认四个 worker、十六个排队连接；请求不会各自创建新线程；SSE 增量传输；配置、请求体、响应体和转换工作区都有明确边界。长时间生成会占用一个 worker，可根据并发需求调整 worker 数；队列满时返回 HTTP 503。

下面是开发机上三轮测量的中位数。测试使用 0.1.0 核心提交 `797ac0e`、四个 worker、四个并发、本机 HTTP 模拟上游，每轮预热 100 次、正式请求 5,000 次。两个配置共六轮，正式请求 30,000 次全部成功。

| 指标 | 不加载插件 | 加载 aliases + policy |
| --- | ---: | ---: |
| 空闲 RSS | 9.69 MiB | 10.34 MiB |
| 采样峰值 RSS | 12.76 MiB | 13.38 MiB |
| 吞吐量 | 2,210.08 请求/秒 | 2,379.19 请求/秒 |
| 请求延迟 p50 | 1.6021 ms | 1.5522 ms |
| 观察到的线程数 | 5 | 5 |
| 子进程数 | 0 | 0 |

这是共享开发机上的小 JSON、非流式、无 TLS、无真实模型生成测试，内存按 10 ms 间隔采样。各轮吞吐范围重叠，不能据此判断插件会加速，也不能据此宣称优于 Magpie。真实模型、长 SSE、大对话及跨协议转换的开销不在该性能数字的覆盖范围内。[完整性能报告与六份原始 JSON](https://github.com/LIghtJUNction/zcalld/blob/main/docs/PERFORMANCE.md)保留了工作负载、波动范围和复现命令。

正式 v0.1.1 x86_64 发布包中的可执行文件为 **1,172,080 字节，约 1.12 MiB**。上面的运行时基准明确保留原测试版本；没有将早先测量冒充成新发布包的重新测量。

## 验证结果

正式发布工作流在原生 x86_64 和 aarch64 runner 上均通过构建与验收，整个 bump、发布、AUR 流程最终状态为 `success`。

| 验证项目 | 结果与覆盖范围 |
| --- | --- |
| Rust 测试 | 28 项通过：核心 22、aliases 3、policy 3 |
| HTTP 黑盒集成 | 19 项通过，覆盖真实监听器、六类入口、认证隔离、SSE、插件、退出和禁止生成请求重放 |
| 发布事务回归 | 9 项通过，覆盖版本同步、标签、失败回滚、干运行、精确标签验证及 AUR 防降级 |
| 静态检查 | 格式化、Clippy `-D warnings`；发布脚本的 ShellCheck 与工作流校验通过 |
| 实际 systemd | 临时用户服务 `Type=notify` 就绪成功；健康检查 HTTP 200；停止后 exit 0、重启数 0 |
| Arch 源码打包 | 0.1.0 源码归档的 makepkg 构建及测试成功；PKGBUILD 与成品包 namcap 无告警 |
| 正式安装包 | 下载 v0.1.1 x86_64 发布包，校验 SHA-256、运行版本命令、校验示例配置、加载并实际调用两个发布插件，均通过 |
| AUR 预编译包 | 从实际公开仓库克隆并构建 `zcalld-bin 0.1.1-2`；`.SRCINFO` 一致，成品包 namcap 无错误或警告 |

systemd 单次观测中，启动命令耗时 29.30 ms，停止命令耗时 7.05 ms，包含 systemctl 开销；服务验证没有发起模型调用。源码打包和 systemd 的详细实测记录是 0.1.0 阶段的记录；0.1.1 增加了 ARM 平台指针类型兼容修正，并在两种原生 runner 上重新通过完整发布验收。[打包与服务验证记录](https://github.com/LIghtJUNction/zcalld/blob/main/docs/packaging-validation.md)说明了验证环境和边界。

AUR 的 `pkgrel=2` 修正了预编译包的 MIT 许可证目录，最终位于 `/usr/share/licenses/zcalld-bin/`。发布二进制与 GitHub 标签保持不变；同一修正已写入后续版本使用的打包模板。AUR 远端提交为 `d1cd286bbdb0c0c93c216dd2a566dacb0ce32fa2`。最终 Arch 成品包大小为 662,321 字节，SHA-256 为 `9193f877bc52a64d74009db3a1b6bb3ebf44d1b3aa9b882e4ccc057c12811563`。PKGBUILD 的 namcap 仍有一条建议使用 `$CARCH` 的提示，它来自按架构分别声明的下载 URL；两套架构来源与校验和均明确配置。成品包检查无该提示或其他输出。

发布归档的实际信息如下，均提供在同一版本的 `SHA256SUMS` 中：

| 架构 | 归档大小 | SHA-256 |
| --- | ---: | --- |
| x86_64 | 890,445 字节 | `7d558a50e50f73ae2f65c60bd3faebcddd59c344c3bc6b1bcb25319410b15b3e` |
| aarch64 | 841,682 字节 | `da86ac0a8671566bd14fa6cfc8cc1edc9b36e303e1dacfa012ed8589623369ac` |

## 安装与第一次启动

最终验证时，`yay -Si zcalld-bin` 已能找到新项目，但仍显示先前的 `0.1.1-1`；公开 AUR Git 的 master 已确认指向许可证修正后的 `0.1.1-2`。为避免 helper 返回旧修订，优先使用公开 AUR Git。该路径的克隆、下载、校验和打包已验证，可以按以下命令直接安装：

```sh
git clone https://aur.archlinux.org/zcalld-bin.git
cd zcalld-bin
makepkg -si
```

需要源码构建时，将仓库名称换成 `zcalld`。AUR helper 显示最新修订后，也可以二选一使用：

```sh
yay -S zcalld-bin
# 或从源码构建：yay -S zcalld
```

安装包提供标准 `/usr/bin`、`/usr/lib/zcalld/plugins`、system/user systemd unit、man page、许可证及配置文件。pacman 对 `/etc/zcalld/config.conf` 使用 backup 规则；归档的 `install.sh` 也保留已有配置。安装不会自动启用服务。

配置和前台运行：

```sh
zcalld init
# 编辑 ~/.config/zcalld/config.conf，填写实际上游与模型。
zcalld check
zcalld serve
```

初始示例连接 `http://127.0.0.1:11434/v1`，公开路由名为 `local`。需要把示例中的模型名称改为该上游实际可用的模型。zcalld 本身不下载或运行模型。

日常检查：

```sh
zcalld models --json
zcalld providers --json
zcalld plugins --json
zcalld health
```

配置有效且上游模型就绪后，在另一个终端发起流式请求：

```sh
eval "$(zcalld env openai)"
curl "$OPENAI_BASE_URL/chat/completions" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"local","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

确认配置后先停止占用相同端口的前台实例，再选择用户服务：

```sh
systemctl --user daemon-reload
systemctl --user enable --now zcalld.service
journalctl --user -u zcalld.service
```

如果配置引用环境变量，用户服务从 `~/.config/zcalld/credentials` 读取 `NAME=value`；使用 `XDG_CONFIG_HOME` 时相应调整目录，并确保用户服务管理器采用相同设置。凭据文件应为 0600。CLI 则从调用它的 shell 读取变量；终端中的 `export` 不会自动传入 systemd 用户服务。完整加载示例见[服务凭据配置](https://github.com/LIghtJUNction/zcalld/blob/main/docs/RELEASING.md#systemd-operation)。

系统级服务使用 `/etc/zcalld/config.conf` 和 `/etc/zcalld/credentials`，通过 `sudo systemctl enable --now zcalld.service` 启动。两种服务对同一监听地址和端口应二选一。系统服务使用动态非特权身份及文件系统限制。配置、插件升级后执行相应的 `systemctl restart`。

本次验证使用了可清理的临时服务；没有安装到系统或长期启用后台网关。正式连接真实模型只需安装并填写自己的上游与凭据。

## 高速迭代与发布

日常开发先在本地运行 `make check`，完成一个有意义的改动后提交并推送。普通 push 和 PR 不自动触发远端测试；远端完整构建用于正式发布，也可以手动启动检查工作流。

最快的 patch 发布入口：

```sh
gh workflow run bump.yml --repo LIghtJUNction/zcalld -f bump=patch
```

工作流依次完成：更新 workspace 与包版本、内部 SDK 精确依赖、Cargo.lock 和 CHANGELOG；创建版本提交及注释标签；原子推送；原生双架构构建与验收；上传 GitHub Release 与 SHA256SUMS；生成真实校验和的 PKGBUILD/.SRCINFO 并发布两个 AUR 仓库。本次 v0.1.1 已经由这条链路实际完成。

本地等价流程：

```sh
make check
make bump BUMP=patch
make release
```

`make bump` 创建版本提交和标签；`make release` 核对精确标签后推送。还支持 minor、major、自定义稳定版本和 dry-run。AUR 发布所需仓库 Secrets 和启用变量已经配置；密钥没有写入源码。已有发行资产禁止静默替换，重跑旧版本不会覆盖更高的 AUR 版本。[发布指南](https://github.com/LIghtJUNction/zcalld/blob/main/docs/RELEASING.md)说明了回滚、重试与恢复约束。

## 当前功能边界

这一版完成了显式路由、原生插件、协议转换、标准 Linux 服务和自动发布。OAuth 账号发现、自动修改第三方 agent 配置、持久化用量存储、配额驱动的自动切换、图形管理界面均未纳入本版。后续可按独立需求扩展插件或单独的 CLI 工具，保持常驻核心职责集中。

收费、套餐和订阅判断留在服务端。客户端只负责配置和调用上游，遵循 KISS 原则。
