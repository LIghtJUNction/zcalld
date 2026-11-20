# zcalld 供应商凭据隔离验收

日期：2026-11-20（Asia/Taipei）

## 1. 结论与源码

本轮修复了供应商名称碰撞导致的密钥串写。新增或更新一个供应商时，不会再因为名称的大小写、点号、短横线或下划线转换而覆盖其他供应商的密钥。已有共享引用会在更新目标供应商时分离，其他引用保留原来的保存值。

| 项目 | 固定版本 |
| --- | --- |
| 旧版复现基线 | [6aea363671a9eb6a210cdf88dff22f34d1c829f8](https://github.com/LIghtJUNction/zcalld/commit/6aea363671a9eb6a210cdf88dff22f34d1c829f8) |
| 修复提交 | [e92ac1304667d69611800bd671a946756f97c70a](https://github.com/LIghtJUNction/zcalld/commit/e92ac1304667d69611800bd671a946756f97c70a) |
| 已合入 main 的提交 | [a5acd795f8fdc9a8287d5a9885a622a5128e3382](https://github.com/LIghtJUNction/zcalld/commit/a5acd795f8fdc9a8287d5a9885a622a5128e3382) |
| 完整验证的 Git tree | c0c7dd250316b2c41bae67f77e968a17f716c580 |
| 结构化记录与日志 | [provider-credentials-20261120.json](https://github.com/LIghtJUNction/zcalld/blob/main/docs/evidence/provider-credentials-20261120.json) |

测试工作树与修复提交拥有相同的 Git tree。随后合入的并行变化只涉及 GUI 子模块固定版本和四份验收文档；通过文件差异检查，确认没有改变本记录验证的原生工作区源码、Cargo 锁文件、依赖或测试脚本。

## 2. 问题与修复行为

旧版允许 p-a、p.a、p_a、P-A 作为四个不同供应商，却把它们都转成同一个凭据变量 ZCALLD_PROVIDER_P_A_KEY。后一次保存会替换前一次保存的值。本地 HTTP 模拟上游实际观察到：请求 p-a 时，本应发送 fixture-secret-0，却发送了最后录入的 fixture-secret-3。这些值都是测试专用合成数据。

新分配的变量名使用供应商名称原始 ASCII 字节的十六进制编码，保留名称的每个差异。选择变量时还会检查已有凭据、配置引用和进程环境变量；占用的候选会被跳过。因此，手工创建的凭据和已删除供应商留下的凭据也不会被新供应商认领或覆盖。

更新现有供应商时，已有独占 api_key_env 引用保持稳定。若同一引用还用于其他供应商、header_env.* 或网关 token_env，目标供应商会获得单独的保存位置，原共享值保持不变。环境变量仍优先于磁盘 credentials；写入磁盘并不意味着已经覆盖进程继承的环境变量。

写入操作复用了运行时的严格凭据读取和解析规则。无效 UTF-8、重复变量、错误格式、读取失败和超过 256 KiB 的文件都会在备份或写入前返回错误；写入后将超过同一上限的操作也会被拒绝。配置和原凭据在这些失败场景中保持原字节。

同时提供 --api-key-env 和 --api-key-stdin 现在会明确报错，避免静默忽略参数。只使用 --api-key-env 仍然选择已有凭据，--api-key-stdin 用于保存新的输入值。README 和手册已同步这一行为。

## 3. 针对性回归

七个新增测试方法在旧版全部失败，共产生 13 条失败记录；修复后七个方法全部通过。它们也包含在最终 25 项完整 CLI 回归中，不重复计入总数。

| 结论 | 证据 | 状态 |
| --- | --- | --- |
| 名称差异不会混用密钥 | 四个名称分别添加，HTTP 上游逐一检查 Authorization；轮换一个供应商后再次逐一检查 | 通过 |
| 既有独占引用保持稳定 | 分别更新旧自动变量和自定义变量，检查引用和值 | 通过 |
| 共享引用只分离目标供应商 | 另一供应商、同供应商 header_env、网关 token_env 三种共享场景 | 通过 |
| 已占用变量不会被覆盖 | 保留孤立凭据、进程环境变量和其他配置引用，重复创建供应商 | 通过 |
| 互斥参数没有写入副作用 | 错误中明确两个参数；配置字节不变，未创建 credentials 或备份 | 通过 |
| 坏凭据文件不会被当成空文件 | 无效 UTF-8、重复变量、错误格式、超大文件；检查原文件字节及错误输出 | 通过 |
| 写入增长不会生成不可加载的文件 | 输入文件恰好 256 KiB，新增内容超限时拒绝并保留原字节 | 通过 |

新增测试同时检查正常列表和错误结果不包含合成密钥。既有凭据权限与无效路由原子性回归继续通过。

## 4. 完整本地验收

环境为 Linux x86_64、Rust 1.100.0、Cargo 1.100.0、Python 3.14.10，使用 debug 构建。所有 HTTP 验证只连接本地模拟上游，所有配置和凭据都在测试隔离目录中创建。

| 检查 | 最终结果 |
| --- | --- |
| cargo fmt --all -- --check | 通过 |
| cargo clippy --workspace --all-targets --locked -- -D warnings | 通过 |
| cargo test --workspace --all-targets --locked | 129 通过，0 失败 |
| cargo build --workspace --locked | 通过 |
| python3 scripts/test-control.py | 25 通过，0 失败 |
| python3 scripts/test-keys.py | 6 通过，0 失败 |
| python3 scripts/integration.py --binary target/debug/zcalld | 22 通过，0 失败 |

第一次完整 CLI 验收在既有的 128 MiB 用量日志测试中遇到 ENOSPC，原因是开发机 /tmp 空间不足。改用 /var/tmp 下的隔离临时目录后，完整 CLI 套件通过。没有因此放宽测试、修改断言或清理其他任务的文件；临时目录在测试结束后自动移除。结构化证据保留了这次环境失败及最终通过记录。

## 5. 复现步骤

在包含上述源码提交的检出目录中执行：

~~~sh
git submodule update --init --recursive vendor/zcall vendor/zconv vendor/zplugin vendor/zsys
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
TMPDIR=/var/tmp python3 scripts/test-control.py
python3 scripts/test-keys.py
python3 scripts/integration.py --binary target/debug/zcalld
~~~

单独验证新增场景：

~~~sh
TMPDIR=/var/tmp python3 scripts/test-control.py -k provider_stdin
~~~

## 6. 适用边界

本记录验证的是 Linux 上的本地 debug 构建和凭据管理行为，不代表 Windows、macOS 或正式发布制品已经验收。本轮提交了源码、回归与文档，没有更换正在运行的安装包，也没有重启实际服务。

修复阻止后续串写，并在目标供应商重新输入密钥时分离旧共享引用。若原密钥已经被旧版覆盖，程序不能从覆盖后的文件中恢复原值，需要重新输入正确密钥。

本轮保留 add/edit 的既有更新语义及环境变量优先级。验证范围为顺序执行的管理命令；多进程同时编辑同一配置的竞争不在本记录的通过声明中。

## 7. 文稿自审

| 维度 | 结果 |
| --- | --- |
| 变更价值 | 有旧版真实 HTTP 鉴权失败支撑，修复直接影响供应商请求正确性 |
| 表达清晰度 | 区分源码合入、磁盘写入、环境覆盖和已运行服务 |
| 证据强度 | 提供固定提交、测试树、源文件 blob、命令、退出码和脱敏日志 |
| 验收覆盖 | 覆盖新增、轮换、旧配置、共享引用、占用候选和失败原子性 |
| 设计合理性 | 使用现有解析器和大小限制，没有引入新依赖或批量迁移 |
