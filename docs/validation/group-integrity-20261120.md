# zcalld 模型分组引用完整性验收

日期：2026-11-20（Asia/Taipei）  
仓库：[LIghtJUNction/zcalld](https://github.com/LIghtJUNction/zcalld)  
基线提交：`eaef79531d75793ae640c587e7d24b0901586173`  
源码提交：[`240347a2f7d9913eb555835e6b7626174fc882eb`](https://github.com/LIghtJUNction/zcalld/commit/240347a2f7d9913eb555835e6b7626174fc882eb)

## 交付结果

本轮修复管理命令破坏模型分组引用的问题。删除仍被分组使用的成员路由或供应商，以及通过普通路由命令覆盖、删除分组入口，现在会在读取密钥输入、创建备份和写入文件之前被拒绝。错误指出涉及的分组，并提示通过 `groups set` 或 `groups remove` 调整。

正常成员更新、删除无关路由或供应商、通过专用命令解除分组引用继续可用。README、路由说明和 man 手册已同步。此次没有增加依赖、修改配置格式或变更版本号。

## 问题与机制

一个模型分组保存两部分状态：`routing.json` 记录分组与成员；`config.conf` 保存可发现的同名公共路由以及成员的实际供应商、模型。原先普通 routes/providers 命令只修改后者，可能返回成功却留下无法选择的成员。

固定基线上的四个独立探针均返回成功并改动了配置，同时保持旧分组定义不变：删除成员、删除其供应商、删除分组入口、覆盖分组入口。新增 CLI 回归在该基线上记录 **66 条失败子用例**；这些是本轮缺口的输入与入口组合，不是 66 个独立产品缺陷。

HTTP 回归还覆盖了真实后果。每个场景在执行管理命令后启动全新的测试网关，使请求读取持久化后的状态：

| 场景 | 基线表现 | 修复后表现 |
| --- | --- | --- |
| 删除分组成员 | CLI 退出 0，随后分组请求 HTTP 404 | CLI 拒绝；文件保持原样，分组请求 HTTP 200，选择原成员 |
| 删除承载分组的供应商 | CLI 退出 0，随后分组请求 HTTP 404 | CLI 拒绝；分组请求 HTTP 200，选择原成员 |
| 先将分组迁移到保留的成员，再删旧供应商 | 正常通过 | 正常通过，HTTP 200 到达新成员上游 |

仅删除分组入口与删除成员具有不同影响；本报告的 HTTP 404 结论限于表中实际运行的两个删除场景。

## 实现与使用方式

公共检查读取并验证现有分组状态，再比较本次会改变的路由名称。它保留名称的字面身份，包括引号、空格和 Unicode，不改变运行时请求路径。

| 管理入口 | 保护规则 |
| --- | --- |
| `routes set` | 禁止直接覆盖分组入口；允许修改普通成员的供应商或模型 |
| `routes remove` | 禁止删除分组入口或仍被分组引用的成员 |
| `providers add/edit --model` | 检查显式 `--route` 与默认生成的路由名称，拒绝与分组入口冲突 |
| `providers remove` | 检查完整删除集合，保护成员及分组入口 |
| `groups set/remove` | 继续通过专用流程更新两份配置，拒绝覆盖损坏的分组状态 |

供应商删除检查同时覆盖“仅承载成员”“仅承载分组入口”和“两者均承载”。成员迁移到另一供应商后，旧供应商仍可能被分组入口引用；重新执行 `groups set` 会按保留成员刷新入口，再允许删除旧供应商。

例如，已有 `fast` 和 `careful` 两条路由时：

```sh
zcalld groups set group/work --members careful
zcalld routes remove fast
```

完全移除分组使用 `zcalld groups remove group/work`。原成员在没有其他分组引用后可以删除。成功编辑继续沿用现有的重启生效方式。

缺少 `routing.json`、空对象和空 groups 数组保持兼容。非对象根值、错误 groups 类型、无效分组名、空或错误成员列表、缺失或错误 member model 均会使相关编辑失败。拒绝用例核对配置目录条目和文件字节不变；带合成密钥 stdin 的用例还确认读取偏移为零且输出不含密钥。

## 实际验证

环境为 Linux x86_64，Rust 1.100.0、Cargo 1.100.0、Python 3.14.10；使用锁定依赖、debug 二进制、独立临时 HOME 和本地模拟上游。

| 检查 | 结果 |
| --- | ---: |
| Rust workspace / all-targets | 134 通过 |
| 完整管理命令 CLI | 44 通过 |
| 路由及分组 HTTP | 8 通过 |
| 通用 HTTP 集成 | 22 通过 |
| 调用方密钥 | 6 通过 |
| 格式检查、严格 Clippy、锁定离线构建、diff 检查 | 全部通过 |

新增 9 个 CLI 测试方法覆盖 69 个表格子用例和 2 个正向操作流程；新增 3 个路由 HTTP 测试，其中 2 个在旧版失败、3 个在新版全部通过。完整 CLI 中其余 35 个原有测试继续通过。

主要命令：

```sh
cargo build --workspace --locked --offline
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --all-targets --locked --offline
python3 scripts/test-control.py -v
python3 scripts/test-keys.py
python3 scripts/integration.py --binary target/debug/zcalld
python3 scripts/test-routing.py \
  --binary target/debug/zcalld \
  --routing-plugin target/debug/libzcalld_plugin_routing.so \
  --redact-plugin target/debug/libzcalld_redact.so \
  --privacy-worker /usr/lib/zcalld-compat/zcalld-privacy -v
```

[结构化证据与脱敏日志](../evidence/group-integrity-20261120.json) 保存失败基线、通过结果、实际命令、来源文件指纹和 Git 对象。提交前已逐项确认 GitHub blob 与本地通过验证的文件一致，验证期间输入保持不变。

## 验证边界与审查

本轮覆盖 routes/providers/groups 的上述管理入口。直接修改文件及 profile 切换仍属于其他状态变更路径。目录断言检查条目与文件字节，未扩大为权限、时间戳或任意并发写入保证。

执行范围为本机 Linux；没有执行 macOS、Windows 原生验证，没有调用真实供应商生成内容，也没有安装候选程序、重启用户服务、创建发布标签或发布制品。源码已提交到 main，正式发布与安装版本继续沿既有流程处理。

独立源码审查未发现阻塞问题：检查位于 stdin 和写入前、供应商删除集合完整、合法成员更新保留、名称身份保持一致。按用户指定的 Research-Paper-Writing-Skills 对报告完成五项核对：价值对应已复现故障；机制与操作可复现；旧版/新版对照明确；负向与正向路径均验证；保护范围和剩余边界明确。测试与静态审查分别记录。

报告及证据副本保存到 [Google Drive / Project z](https://drive.google.com/drive/folders/1vjkFko8onQm9WiScrw7QBopK9vNGTLx0)。
