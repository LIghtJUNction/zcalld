# zcalld：模型配置写入校验与原值保留验收

日期：2026-11-20（Asia/Taipei）  
仓库：[LIghtJUNction/zcalld](https://github.com/LIghtJUNction/zcalld)  
最终源码提交：`5e9de4b069073aa50b5a3fee37d8f55488297f4e`  
最终源码树：`1da1a20fa701ab6e5833dc9954bd679ea6c8dba5`

## 1. 交付结论

本轮修复了模型配置在写入后无法加载、或读回时模型值改变的问题。供应商编辑和路由编辑现在使用与配置加载器一致的 INI 值解析及模型长度规则；供应商命令在读取密钥、创建备份或写文件前完成本次处理的模型和关联路由校验。组路由与迁移导入在写回已经解析的模型字符串时，保留字面引号、反斜杠和首尾空格。

本次交付是已提交并推送的源码、测试和验收记录。测试使用隔离目录及合成数据，没有安装候选二进制、切换服务或进行真实供应商生成请求。

## 2. 已复现的问题

第一组复现固定在 `cb5aeda87d0acf54db358b3b88703b2de8b0b7ac`。新增的 5 个实际 CLI 测试共记录 44 条失败记录，包含参数化子用例；有效的 512 字节和既有 INI 用法作为正向对照。

| 入口 | 旧行为 | 修复后的行为 |
| --- | --- | --- |
| `providers add/edit --model` | 空值、空引号、未闭合引号或过长值可能保存成功，随后加载失败 | 依据解析后的模型值提前拒绝 |
| `providers add/edit --api-key-stdin` | 模型或关联路由参数出错前，先消费 stdin 中的密钥 | 本次处理的模型或关联路由参数错误时不消耗 stdin；配置、凭据及已有备份的文件内容保持不变，也不新增目录条目 |
| `routes set --model` | 同样可以把无法加载的模型值写入配置 | 与供应商入口共用模型校验 |
| `groups set` | 已解析模型裸写后再次被当作 INI 语法解释，丢失引号或空格 | 写回时只添加一层 INI 引号，读回值与原值一致 |

第二组复现固定在第一批修复提交 `ecf9c31283ce59f1d11accc555bc61f99dc54ff2`。新加入的 3 个迁移测试全部失败，分别证实：直接供应商的 `local/` 目录项产生空上游模型；短公开别名配合 513 字节的插件 `apiId` 未被拒绝；两个字面双引号被裸写后变成空模型。

迁移修复在生成配置的最终模型输出处校验已解析的字面字符串，并使用 INI 引号保留内容。模型目录与插件输入不经过第二次 INI 解析。

## 3. 实现与兼容性

共享的 `parse_ini_value` 保留原配置读取器的顺序：先去除值外部的空白，再移除至多一层匹配的单引号或双引号，最后拒绝控制字符。它不进行 JSON 解码或反斜杠转义；配置错误仍带原来的行号。

共享的 `valid_model` 检查解析后的值非空、UTF-8 长度不超过 512 字节且不含控制字符。256 个 `é` 占 512 字节，257 个占 514 字节；测试同时覆盖这两个边界。加引号后的首尾空格属于模型值，继续保留；未加引号的纯空白解析为空并被拒绝。

供应商命令提前执行完整的模型与路由处理块，包括参数缺值、重复参数、名称检查和内存中的 INI 更新。因此，原始值边缘的 tab 或换行即使会被解析器去除，仍由写入字段检查在读取 stdin 前拒绝。既有的凭据引用隔离、备份和写入流程保持原有职责。

本轮没有把“未提供 `--model` 时单独出现的 `--route`”改为报错；该组合继续沿用原有忽略行为。这里的提前拒绝保证仅针对实际处理的模型和关联路由参数，不代表对所有现有配置内容进行一次完整事务校验。

## 4. 实际验证

环境：Linux x86_64；`rustc 1.100.0 (0ef1bc386 2026-11-09)`；`cargo 1.100.0 (3acc79c0c 2026-11-05)`；Python 3.14.10。构建使用 debug profile 和锁定的依赖。命令耗时含开发机编译锁等待，不作为性能数据。测试临时目录位于独立的 `/var/tmp` 子目录，结束后自动清理。

第一批源码通过 131 项 Rust 测试、30 项管理命令测试、格式检查和严格 Clippy 后提交。第二批最终验证如下：

| 检查 | 结果 |
| --- | --- |
| Rust workspace 测试 | 134 通过 |
| 管理命令 CLI 测试 | 30 通过 |
| 调用方密钥测试 | 6 通过 |
| HTTP 集成测试 | 22 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy … -- -D warnings` | 通过 |
| `cargo build --workspace --locked` | 通过 |

执行的主要命令：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
python3 scripts/test-control.py -v
python3 scripts/test-keys.py
python3 scripts/integration.py --binary target/debug/zcalld
```

控制测试直接执行构建出的 CLI，并检查退出状态、整个配置目录的文件内容和 stdin 文件偏移。迁移测试把生成的配置交给真正的 `Config::parse`，再比较模型字符串。HTTP 集成使用本地模拟上游，验证共享配置读取路径与现有网关行为。

[结构化证据](../evidence/model-validation-20261120.json) 包含不可变提交与文件对象 ID、两组失败基线、最终命令退出状态、测试计数、二进制指纹和经过路径脱敏的原始日志。测试数据中的密钥均为合成 fixture 值。

## 5. 验证范围与审查

本轮在 Linux 上实际运行验证，没有执行 macOS 或 Windows 原生验收，也没有对真实供应商账户发起生成请求。跨平台源码未引入新的平台分支或依赖，但这不等于已经在所有平台运行测试。

独立只读审查确认：INI 解析语义与错误行号保留；完整模型处理块在 stdin 前执行；组路由的单层引号包装与现有读取器一致；新增路由字段不影响上一轮凭据引用隔离。未发现阻塞性回归。

按用户补充调用的 agy 也完成了迁移补丁的文字复核，未发现阻塞性问题。该复核范围是提供的补丁和解析规则；实际执行证据来自上述本地测试命令。

按研究写作检查框架复核：价值对应可复现的配置损坏；说明区分原始 INI 输入和已解析字符串；证据保留失败基线与通过结果；覆盖四条生产模型配置写入路径；设计复用既有解析与校验，不增加通用事务框架。

## 6. 提交与交付位置

- [第一批：模型参数预检与组路由原值保留](https://github.com/LIghtJUNction/zcalld/commit/ecf9c31283ce59f1d11accc555bc61f99dc54ff2)
- [第二批：迁移模型校验与原值保留](https://github.com/LIghtJUNction/zcalld/commit/5e9de4b069073aa50b5a3fee37d8f55488297f4e)
- 本报告：`docs/validation/model-validation-20261120.md`
- 结构化证据：`docs/evidence/model-validation-20261120.json`
- 报告与证据副本：[Google Drive / Project z](https://drive.google.com/drive/folders/1vjkFko8onQm9WiScrw7QBopK9vNGTLx0)

写作参考用户指定的 [Research-Paper-Writing-Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills)，以“问题、实现、证据、边界、交付”组织内容。
