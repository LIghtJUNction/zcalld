# Zcall 0.4.4：原生网关、可选兼容层与桌面管理

> **zcalld 0.4.4 已正式发布。** [正式运行 44838378576](https://github.com/LIghtJUNction/zcalld/actions/runs/44838378576)以标签 `v0.4.4`、源码 [`0a0a62c`](https://github.com/LIghtJUNction/zcalld/tree/0a0a62cdca25bbdf289a17f03dfeed1acc945595)完成一次正式验收：五个原生目标及发布、AUR 任务全部成功。每个平台的 Claude helper **9/9**、provider **10/10**、完整归档安装器 **12/12** 均通过；其他平台限定测试的跳过情况以各任务日志为准。[发行资产](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.4.4)

## 组件与支持范围

| 组件 | 版本 | 职责与公开证明 |
|---|---|---|
| zcalld | 0.4.4 | Rust 网关、路由、原生插件接口与整包安装器；[正式五平台验收](https://github.com/LIghtJUNction/zcalld/actions/runs/44838378576)、[发行资产](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.4.4)。 |
| zcalld-compat | 0.3.1 | 按需启动的插件管理、隐私处理及 Claude PTY／ConPTY helper；[正式五平台验收](https://github.com/LIghtJUNction/zcalld-compat/actions/runs/44791806657)、[发行资产](https://github.com/LIghtJUNction/zcalld-compat/releases/tag/v0.3.1)。 |
| zcall-gui | 0.1.5 | 独立桌面管理界面、显式首次安装与更新流程；[正式五平台验收](https://github.com/LIghtJUNction/zcall-gui/actions/runs/44826416942)、[发行资产](https://github.com/LIghtJUNction/zcall-gui/releases/tag/v0.1.5)。 |

发行目标为 **Linux x86_64／ARM64、macOS x86_64／ARM64、Windows x86_64**。GUI 关闭后网关可继续独立运行；GUI 覆盖常用管理操作，完整功能仍以 CLI 为准。[架构与命令](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/README.md) · [GUI 范围](https://github.com/LIghtJUNction/zcall-gui/blob/ca35a306088a85e49e9e851aab24329deed20056/README.md)

## 使用边界

- **本地连接：** Unix socket 与 TCP 可以并存。优先使用已配置的 socket；只有路径不存在或连接被拒绝、且已配置 TCP 时才回退。权限错误不会触发回退；Windows 使用 TCP。[传输合同](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/src/transport.rs)
- **Agent 接入：** 内置 **16 类检测、10 类配置写入**。检测不会改写配置；先选择目标 Agent 与路由，再显式应用写入。[检测与配置实现](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/src/adapters.rs)
- **扩展与隐私：** 社区插件可固定版本或摘要安装，默认禁用；启用后以当前用户权限执行，完整性校验不构成沙箱。隐私处理提供规则与可选本地模型、阻断及占位符还原。JS 社区功能需要 Bun，显式 Claude 原生模式可以不启动 Bun；helper 按功能需要运行。[插件信任边界](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/docs/PLUGINS.md) · [兼容层说明](https://github.com/LIghtJUNction/zcalld-compat/blob/c521d1fb1ce8eac173ea2bb481816d3cd1df82d3/README.md)

## 安装、更新与图形依赖

CLI 使用 `zcalld-installer` 的 `plan` → `apply` 流程，并可查询 `status`、继续 `resume` 或执行 `rollback`；运行中的受管网关可通过 `apply --defer` 等待退出后再切换整套目录。GUI 的 core 安装也先准备、查看计划，再明确应用；GUI 自更新是独立流程。包管理器安装继续由对应包管理器更新。[安装器命令](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/crates/zcalld-installer/src/cli.rs) · [GUI 安装](https://github.com/LIghtJUNction/zcall-gui/blob/ca35a306088a85e49e9e851aab24329deed20056/src/core_install.rs) · [GUI 自更新](https://github.com/LIghtJUNction/zcall-gui/blob/ca35a306088a85e49e9e851aab24329deed20056/src/updater.rs)

Core portable 归档包含 `zcalld-pluginctl`、`zcalld-privacy`、`zcalld-claude-tui` 三个带版本的 helper，以及对应运行时资源和许可证；Arch 的 core／compat 仍独立分包。[归档布局](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/scripts/package.py)

Arch Linux 的 core 可选择源码包 [zcalld](https://aur.archlinux.org/packages/zcalld) 或预编译包 [zcalld-bin](https://aur.archlinux.org/packages/zcalld-bin)；兼容层为 [zcalld-compat](https://aur.archlinux.org/packages/zcalld-compat)，GUI 为 [zcall-gui](https://aur.archlinux.org/packages/zcall-gui)。例如，使用 paru 安装三项源码包：

```sh
paru -S zcalld zcalld-compat zcall-gui
```

[Core 正式源码包](https://github.com/LIghtJUNction/zcalld/releases/download/v0.4.4/zcalld-0.4.4-source.tar.gz)为 4,207,741 字节，SHA-256 为 `c8e87b57569d4ff726f0651ae3fbf0bd64d6ceb6ab7994e9386ce44e3d93b273`；下载其他目标归档时，使用该发行附带的 `SHA256SUMS` 核对。

UI 使用 **zgpui（原名 zui-native）**，采用原生 GPU 绘制，不嵌入浏览器。Linux 为 X11／GLX／OpenGL，Wayland 桌面需 XWayland；Windows 为 Win32／WGL／OpenGL；macOS 为 AppKit／OpenGL，尚非 Metal。Rust 部分依赖 Project Z 自有 crate、无第三方 Cargo 依赖，**这不等于没有系统依赖**：仍需平台图形库、网络库及归档工具，Windows **core 归档**随附所需传输 DLL；可选 Go／Bun 组件另有自己的依赖与许可证。[原生后端与限制](https://github.com/LIghtJUNction/zgpui/blob/b4d5493e0cd985d00558c7be0b5726965ab5163a/docs/native.md) · [系统依赖](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/README.md)

## 开发与独立发布

Core 日常验证入口为 [`./scripts/check.sh`](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/scripts/check.sh) 与 `make check-native`。以下为维护者发起下一次补丁版的命令；本说明不会执行它们。

```sh
gh workflow run bump.yml --repo LIghtJUNction/zcalld --ref main -f bump=patch -f dry_run=false
gh workflow run bump.yml --repo LIghtJUNction/zcalld-compat --ref main -f part=patch
gh workflow run bump.yml --repo LIghtJUNction/zcall-gui --ref main -f bump=patch -f dry_run=false
```

工作流合同：[core](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/.github/workflows/bump.yml)、[compat](https://github.com/LIghtJUNction/zcalld-compat/blob/c521d1fb1ce8eac173ea2bb481816d3cd1df82d3/.github/workflows/bump.yml)、[GUI](https://github.com/LIghtJUNction/zcall-gui/blob/ca35a306088a85e49e9e851aab24329deed20056/.github/workflows/bump.yml)。五个公开库各自提供 `release.yml`：

| 库与发布入口 | 本轮源码版本 | 发布补丁版输入 |
|---|---|---|
| [zcall](https://github.com/LIghtJUNction/zcall/blob/317c52f101f112c6af671a629d8406305a168e4a/.github/workflows/release.yml) | 0.4.0 | `bump=patch`、`publish=true` |
| [zconv](https://github.com/LIghtJUNction/zconv/blob/0af1f7eb0766ff0ce9db5cee7c6184eff511a8c1/.github/workflows/release.yml) | 0.4.0 | `bump=patch`、`publish=true` |
| [zplugin](https://github.com/LIghtJUNction/zplugin/blob/cf02c1d9db575b92b36297dfe1fde72de9ee51e4/.github/workflows/release.yml) | 0.3.1 | `bump=patch`、`dry_run=false` |
| [zsys](https://github.com/LIghtJUNction/zsys/blob/808fcfdb73584ca7ca122e20b76f0209f56ab0f7/.github/workflows/release.yml) | 0.1.1 | `bump=patch` |
| [zgpui](https://github.com/LIghtJUNction/zgpui/blob/b4d5493e0cd985d00558c7be0b5726965ab5163a/.github/workflows/release.yml) | 0.1.5 | `bump=patch` |

子模块 gitlink 与 Cargo.lock 固定具体提交；同版本标签不一定就是本轮固定提交，更新库时应一并核对两者。发布门禁中的本地模拟 CLI／后端验收，不等于真实供应商认证、付费调用或物理 GPU 性能测试。
