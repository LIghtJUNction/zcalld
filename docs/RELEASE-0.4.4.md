# Zcall 0.4.4：原生网关、可选兼容层与桌面管理

> **zcalld 0.4.4 已正式发布。** [正式运行 44838378576](https://github.com/LIghtJUNction/zcalld/actions/runs/44838378576)以标签 `v0.4.4`、源码 [`0a0a62c`](https://github.com/LIghtJUNction/zcalld/tree/0a0a62cdca25bbdf289a17f03dfeed1acc945595)完成一次正式验收：五个原生目标及发布、AUR 任务全部成功。每个平台的 Claude helper **9/9**、provider **10/10**、完整归档安装器 **12/12** 均通过；其他平台限定测试的跳过情况以各任务日志为准。[发行资产](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.4.4)

## 组件与支持范围

| 组件 | 版本 | 职责与公开证明 |
|---|---|---|
| zcalld | 0.4.4 | Rust 网关、路由、原生插件接口与整包安装器；[正式五平台验收](https://github.com/LIghtJUNction/zcalld/actions/runs/44838378576)、[发行资产](https://github.com/LIghtJUNction/zcalld/releases/tag/v0.4.4)。 |
| zcalld-compat | 0.3.1 | 按需启动的插件管理、隐私处理及 Claude PTY／ConPTY helper；[正式五平台验收](https://github.com/LIghtJUNction/zcalld-compat/actions/runs/44791806657)、[发行资产](https://github.com/LIghtJUNction/zcalld-compat/releases/tag/v0.3.1)。 |
| zcall-gui | 0.1.6；源码 AUR 0.1.6-1 | 独立桌面管理界面、显式首次安装与更新流程；修复换页后侧栏选择标记未及时重绘；[正式五平台验收](https://github.com/LIghtJUNction/zcall-gui/actions/runs/44853641350)、[发行资产](https://github.com/LIghtJUNction/zcall-gui/releases/tag/v0.1.6)。 |

发行目标为 **Linux x86_64／ARM64、macOS x86_64／ARM64、Windows x86_64**。GUI 关闭后网关可继续独立运行；GUI 覆盖常用管理操作，完整功能仍以 CLI 为准。[架构与命令](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/README.md) · [GUI 范围](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/README.md)

## 使用边界

- **本地连接：** Unix socket 与 TCP 可以并存。优先使用已配置的 socket；只有路径不存在或连接被拒绝、且已配置 TCP 时才回退。权限错误不会触发回退；Windows 使用 TCP。[传输合同](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/src/transport.rs)
- **Agent 接入：** 内置 **16 类检测、10 类配置写入**。检测不会改写配置；先选择目标 Agent 与路由，再显式应用写入。[检测与配置实现](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/src/adapters.rs)
- **扩展与隐私：** 社区插件可固定版本或摘要安装，默认禁用；启用后以当前用户权限执行，完整性校验不构成沙箱。隐私处理提供规则与可选本地模型、阻断及占位符还原。JS 社区功能需要 Bun，显式 Claude 原生模式可以不启动 Bun；helper 按功能需要运行。[插件信任边界](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/docs/PLUGINS.md) · [兼容层说明](https://github.com/LIghtJUNction/zcalld-compat/blob/c521d1fb1ce8eac173ea2bb481816d3cd1df82d3/README.md)

## 安装、更新与图形依赖

CLI 使用 `zcalld-installer` 的 `plan` → `apply` 流程，并可查询 `status`、继续 `resume` 或执行 `rollback`；运行中的受管网关可通过 `apply --defer` 等待退出后再切换整套目录。GUI 的 core 安装也先准备、查看计划，再明确应用；GUI 自更新是独立流程。包管理器安装继续由对应包管理器更新。[安装器命令](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/crates/zcalld-installer/src/cli.rs) · [GUI 安装](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/src/core_install.rs) · [GUI 自更新](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/src/updater.rs)

Core portable 归档包含 `zcalld-pluginctl`、`zcalld-privacy`、`zcalld-claude-tui` 三个带版本的 helper，以及对应运行时资源和许可证；Arch 的 core／compat 仍独立分包。[归档布局](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/scripts/package.py)

Arch Linux 的 core 可选择源码包 [zcalld](https://aur.archlinux.org/packages/zcalld) 或预编译包 [zcalld-bin](https://aur.archlinux.org/packages/zcalld-bin)；兼容层为 [zcalld-compat](https://aur.archlinux.org/packages/zcalld-compat)，GUI 为 [zcall-gui](https://aur.archlinux.org/packages/zcall-gui)。例如，使用 paru 安装三项源码包：

```sh
paru -S zcalld zcalld-compat zcall-gui
```

[Core 正式源码包](https://github.com/LIghtJUNction/zcalld/releases/download/v0.4.4/zcalld-0.4.4-source.tar.gz)为 4,207,741 字节，SHA-256 为 `c8e87b57569d4ff726f0651ae3fbf0bd64d6ceb6ab7994e9386ce44e3d93b273`；下载其他目标归档时，使用该发行附带的 `SHA256SUMS` 核对。

UI 使用 **zgpui（原名 zui-native）**，采用原生 GPU 绘制，不嵌入浏览器。Linux 为 X11／GLX／OpenGL，Wayland 桌面需 XWayland；Windows 为 Win32／WGL／OpenGL；macOS 为 AppKit／OpenGL，尚非 Metal。Rust 部分依赖 Project Z 自有 crate、无第三方 Cargo 依赖，**这不等于没有系统依赖**：仍需平台图形库、网络库及归档工具，Windows **core 归档**随附所需传输 DLL；可选 Go／Bun 组件另有自己的依赖与许可证。[原生后端与限制](https://github.com/LIghtJUNction/zgpui/blob/b4d5493e0cd985d00558c7be0b5726965ab5163a/docs/native.md) · [系统依赖](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/README.md)

### GUI 0.1.6：换页后及时重绘

0.1.5 的实际窗口验收发现，点击侧栏后正文已切到新页面，侧栏却可能仍标记旧页面。换页发生在绘制过程中，当前帧已经计算过侧栏的选中状态；没有新的输入事件时，界面也没有请求下一帧。0.1.6 在侧栏、概览入口和安装入口等六处绘制中换页的位置设置重绘标记，让下一帧完成一致的页面显示，随后恢复空闲等待。布局、图标、后端命令与更新器均未改动，图形库仍固定为 zgpui 0.1.5。[修复提交](https://github.com/LIghtJUNction/zcall-gui/commit/006c28670ee117e0c4557d8859f2d18f5c44ceb1)

回归使用独立 Xvfb 和合成数据，只发送一次鼠标按下、释放，之后仅查询和截图，不靠后续移动、按键或 Expose 事件补帧。旧二进制在首个侧栏选中标记断言失败；修复版通过覆盖六页和相关入口的十次导航，页面标记与标题同步，导航期间 CLI 调用数不增加。本机完整 `scripts/check.py --ui` 通过；正式 Linux 两个架构也运行该导航检查，2 秒空闲采样为 0 CPU ticks。这些是隔离测试结果，实际设备六页及 GPU 验收单独记录。[回归实现](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/scripts/ui_smoke.py)

[正式运行 44853641350](https://github.com/LIghtJUNction/zcall-gui/actions/runs/44853641350)由既有补丁版工作流发起，五个平台的验证、打包、制品上传及最终发布全部成功。每个平台的安装引导检查均为 9/9；既有发布测试共 11 项，在非 Arch 正式运行器上保留 1 项 `makepkg` 条件跳过，其余 10 项通过，本机 Arch 运行则为 11/11。标签 `v0.1.6` 固定源码 [`2593369`](https://github.com/LIghtJUNction/zcall-gui/tree/2593369b2e9695a4a4cabed20eb80bee01fc419c)，[正式源码归档](https://github.com/LIghtJUNction/zcall-gui/releases/download/v0.1.6/zcall-gui-v0.1.6-source.tar.gz)为 314,847 字节，SHA-256 为 `e54822ffaf1681f311e7f54e3042c014e7f36bfa49defd4274cc545ba71c298c`。源码 AUR 配方 **0.1.6-1** 已发布到提交 [`3d9e69b`](https://aur.archlinux.org/cgit/aur.git/commit/?h=zcall-gui&id=3d9e69b19b0bdf3cbfd09f09c58cb5a328e872be)，继承下述离线构建修正。

**GUI 0.1.6 单包干净构建与包质量验收：** 已完成 Arch Linux x86_64 干净构建、`check()` 及新隔离环境中的 GUI 依赖与版本检查，20 个实际步骤全部成功。新包为 472,739 字节，SHA-256 为 `973d1192a8ec772cb2d91918ddc912949be3c129b2b732d92a59a08604e76216`。namcap 原始结果为错误 0、警告 2；两条警告分别对应 GUI 通过命令调用的 `zcalld` 与 `curl`，已结合正式源码、AUR 提交、实际包和二进制摘要重新审核，已审可接受 2、未审 0，依赖没有删除。[后端调用](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/src/backend.rs) · [更新器调用](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/src/updater.rs)。core 0.4.4-1 与 compat 0.3.1-1 沿用已验收的成品及原始证据；此次只为新 GUI 构建源码包，不把旧包复用记作三包重新构建。

Core 的不可变 `v0.4.4` 标签及正式源码归档仍对应 `0a0a62c`，其中 GUI gitlink 为 `ca35a306`。后续主分支提交 [`0b7d7b3`](https://github.com/LIghtJUNction/zcalld/commit/0b7d7b386363bc71dfbb4e0d78033da7819db17b)只把 `apps/zcall-gui` 更新到 0.1.6 正式提交 `2593369`；没有改写 core 的标签或归档，也没有改变五个库的固定提交。

### 历史包装修订：GUI 0.1.5-2

在 0.1.6 发布前，Arch 的 [zcall-gui 源码包](https://aur.archlinux.org/packages/zcall-gui)曾修订为 **0.1.5-2**：`0.1.5` 是 GUI 版本，`-2` 是 AUR 包修订号。该修订修复空 Cargo 缓存下的离线构建：`prepare()` 核验正式归档中的库版本与锁文件，将构建副本的依赖指向随包提供的 `vendor/zgpui`，离线生成锁文件，后续构建与测试继续使用 `--frozen`。仓库的 Git 依赖及固定提交、运行时代码、正式发行资产和二进制 AUR 包保持原样。[包装修正](https://github.com/LIghtJUNction/zcall-gui/commit/d491b627223807da7b2929f3feea7461393d124c)

当时新增的冷缓存检查执行实际 `prepare()`，再分别解析 GUI 与启用 `native` 特性的内含库，要求完整依赖图均来自归档内源码。[AUR 专用工作流运行 44844718137](https://github.com/LIghtJUNction/zcall-gui/actions/runs/44844718137)已成功。以下保留当时的发布命令示例，不是对旧版本的再次发布指令：

```sh
gh workflow run aur.yml --repo LIghtJUNction/zcall-gui --ref main \
  -f version=0.1.5 -f source_pkgrel=2
```

该入口在检查成功后发布同一份已核验配方；后续新版本的默认包修订号仍为 `1`。[手动工作流与参数](https://github.com/LIghtJUNction/zcall-gui/blob/d491b627223807da7b2929f3feea7461393d124c/.github/workflows/aur.yml)

**历史中间组合的 Arch Linux x86_64 验收已完成：** `zcalld 0.4.4-1`、`zcalld-compat 0.3.1-1`、`zcall-gui 0.1.5-2` 的干净构建与 `check()` 均通过；三包联合安装后，六个程序的版本、Claude helper 能力声明及 system／user unit 语法检查均通过。namcap 错误为 0，原始警告依次为 **0／1／2**：compat 的核心服务依赖，以及 GUI 的 `zcalld`、`curl` 命令依赖均已按源码和实际产包逐项审核，未审警告为 0，原始警告总数保持。历史汇总复用已完成的实际构建与测试记录并复核原始证据，保留此前失败回执，没有将续验记作重新编译；这些结果只适用于上述固定版本，不作为新 GUI 0.1.6 包的验收结论；生产设备部署另行验证。

## 开发与独立发布

Core 日常验证入口为 [`./scripts/check.sh`](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/scripts/check.sh) 与 `make check-native`。以下为维护者发起下一次补丁版的命令；本说明不会执行它们。

```sh
gh workflow run bump.yml --repo LIghtJUNction/zcalld --ref main -f bump=patch -f dry_run=false
gh workflow run bump.yml --repo LIghtJUNction/zcalld-compat --ref main -f part=patch
gh workflow run bump.yml --repo LIghtJUNction/zcall-gui --ref main -f bump=patch -f dry_run=false
```

工作流合同：[core](https://github.com/LIghtJUNction/zcalld/blob/0a0a62cdca25bbdf289a17f03dfeed1acc945595/.github/workflows/bump.yml)、[compat](https://github.com/LIghtJUNction/zcalld-compat/blob/c521d1fb1ce8eac173ea2bb481816d3cd1df82d3/.github/workflows/bump.yml)、[GUI](https://github.com/LIghtJUNction/zcall-gui/blob/2593369b2e9695a4a4cabed20eb80bee01fc419c/.github/workflows/bump.yml)。五个公开库各自提供 `release.yml`：

| 库与发布入口 | 本轮源码版本 | 发布补丁版输入 |
|---|---|---|
| [zcall](https://github.com/LIghtJUNction/zcall/blob/317c52f101f112c6af671a629d8406305a168e4a/.github/workflows/release.yml) | 0.4.0 | `bump=patch`、`publish=true` |
| [zconv](https://github.com/LIghtJUNction/zconv/blob/0af1f7eb0766ff0ce9db5cee7c6184eff511a8c1/.github/workflows/release.yml) | 0.4.0 | `bump=patch`、`publish=true` |
| [zplugin](https://github.com/LIghtJUNction/zplugin/blob/cf02c1d9db575b92b36297dfe1fde72de9ee51e4/.github/workflows/release.yml) | 0.3.1 | `bump=patch`、`dry_run=false` |
| [zsys](https://github.com/LIghtJUNction/zsys/blob/808fcfdb73584ca7ca122e20b76f0209f56ab0f7/.github/workflows/release.yml) | 0.1.1 | `bump=patch` |
| [zgpui](https://github.com/LIghtJUNction/zgpui/blob/b4d5493e0cd985d00558c7be0b5726965ab5163a/.github/workflows/release.yml) | 0.1.5 | `bump=patch` |

子模块 gitlink 与 Cargo.lock 固定具体提交；同版本标签不一定就是本轮固定提交，更新库时应一并核对两者。发布门禁中的本地模拟 CLI／后端验收，不等于真实供应商认证、付费调用或物理 GPU 性能测试。
