# zcalld：配置写入一致性验收

日期：2026-11-20（Asia/Taipei）  
仓库：[LIghtJUNction/zcalld](https://github.com/LIghtJUNction/zcalld)  
基线提交：`125510eb210096c1b8355cb5529438cf0b730f6e`  
源码提交：`cf94ccbecc8a2f99c6eb498238c8efaa462ba68c`  
源码树：`b2e56a06c73bb3668e2aad9252ddf1d444f801c6`

## 1. 交付结论

本轮修复三类“管理命令报告成功，但保存结果不符合用户选择或无法重新加载”的问题：供应商 URL 与协议组合缺少完整预检；路由和分组写回供应商引用时改变字面名称；更新和删除未正确识别带空白的 section 标题。

供应商 add/edit 现在复用加载器的 endpoint 校验，在消费密钥 stdin 或写文件前拒绝本轮覆盖的非法 URL 与协议组合。路由与分组写回供应商名称时保留字面引号、空格和反斜杠。配置修改使用与读取一致的 section 标题识别方式，同时保留名称内部字符的区别。

## 2. 失败基线与根因

先在固定基线构建实际 debug 二进制，再运行新增的 5 项 CLI 回归测试。旧二进制记录 **33 条失败记录，0 个测试运行错误**；这不是 33 个独立产品缺陷。匹配协议的有效 URL，以及不需要额外保护的名称，作为正向对照。

三个单独探针还记录了修改前后的真实 `zcalld check` 结果：

| 探针 | 修改命令 | 旧版保存结果 | 重新加载 |
| --- | --- | --- | --- |
| 空主机 URL `http://` | 返回成功 | 配置改变，并创建凭据文件 | 失败，提示缺少主机 |
| 字面名称 `"quoted"` | 返回成功 | 请求的供应商名称变成 `quoted` | 仍成功，但引用身份已变 |
| `[ route seed ]` | 返回成功 | 更新时额外添加同名 section | 失败，提示重复 section |

第一类问题来自两处验证能力不同：管理命令检查了 URL 前缀和部分禁止字符，加载器还会检查完整 URL 及其与协议的匹配。第二类问题来自名称和配置值的语义不同：section 中的供应商名称是字面字符串，赋值右侧的内容却会解析一层 INI 引号。第三类问题来自查找方式不同：读取会去除标题整体两侧空白，修改时却曾要求标题文本精确匹配。

供应商身份探针只证实了配置引用与回读的变化，没有发起或观察真实网络请求。

## 3. 实现及兼容性

### URL 与协议预检

`providers add/edit` 先确定本次实际使用的 URL 与协议，再调用加载器已有的 `protocol::endpoint`。URL 先经过共享 `parse_ini_value`，因此尾随普通空格仍按既有读取方式处理；检查成功后保存的仍是原字段，不会替换成构造后的调用地址。占位模型 `validation-model` 只参与内存中的验证，不写入配置。

测试覆盖空主机、超范围端口、非法百分号编码、未编码空格、完整 chat endpoint 配合 Anthropic 协议，以及只修改协议而保留旧完整 URL 的情况。失败用例核对 stdin 偏移仍为零、配置目录条目与文件字节保持不变、合成密钥未出现在命令输出中。edit 用例还保留已有凭据及备份作为对照。

这项修改补齐加载器本来就要求的校验。CLI 原有的前缀、查询串等限制继续适用；它没有扩大 CLI 接受的 URL 范围，也不代表对所有配置编辑建立了通用事务机制。

### 保留字面供应商名称

`routes set` 先精确确认用户选中的 provider section；`groups set` 使用已加载的成员供应商名称。写回时给这些已经确定的字面字符串添加一层 INI 双引号，不再把名称本身当作待解析的 INI 输入。

zcalld 的读取器只剥离外层匹配引号，不执行 JSON 解码或反斜杠转义。因此单层包装可以保留成对或不成对的字面引号、前导空格、内部空格与反斜杠。测试同时放置名称相近的另一个供应商，确保断言检查的是正确身份，而不只是配置能够加载。

### 统一 section 标题识别

新增的小型 helper 依次去除整行外部空白、识别完整方括号，再去除括号内部整体的首尾空白。`sections`、`ini_set` 的起止位置和 `ini_remove` 共用它。

`[provider p]` 与 `[ provider p ]` 因此匹配同一个 section；`[provider  p]` 仍具有不同的名称，内部额外空格不会被压缩。更新和删除回归同时覆盖普通空格、tab 与 Unicode EM SPACE。更新测试核对原标题格式和无关字段、注释仍被保留，删除测试核对目标确实消失后配置仍可加载。

## 4. 实际验收

环境：Linux x86_64；`rustc 1.100.0 (0ef1bc386 2026-11-09)`；`cargo 1.100.0 (3acc79c0c 2026-11-05)`；Python 3.14.10。依赖保持锁定，测试使用 debug 二进制、独立临时目录及合成数据。开发机共享编译锁等待包含在命令耗时中，不能把这些耗时当作性能测量。

| 检查 | 最终结果 |
| --- | --- |
| Rust workspace 测试 | 134 通过 |
| 管理命令 CLI 测试 | 35 通过 |
| 调用方密钥测试 | 6 通过 |
| HTTP 集成测试 | 22 通过 |
| 格式检查、严格 Clippy、构建 | 全部通过 |

主要命令：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
python3 scripts/test-control.py -v
python3 scripts/test-keys.py
python3 scripts/integration.py --binary target/debug/zcalld
```

新增的 5 项测试直接运行 CLI，并调用真实配置检查入口。完整回归继续覆盖上一轮的模型校验与凭据隔离；HTTP 测试使用本地模拟上游。

[结构化证据](../evidence/config-consistency-20261120.json) 保存固定基线、实际命令及退出状态、测试计数、源码对象 ID、二进制 SHA-256，以及经过路径脱敏的原始日志。验证开始时记录的源码指纹与提交文件逐一一致。

## 5. 范围、审查和交付位置

本轮实际运行环境为 Linux x86_64。没有执行 macOS 或 Windows 原生验收，也没有安装候选二进制、切换服务或使用真实供应商账户生成内容。目录不变断言检查条目和文件字节，不包含权限与时间戳；provider 身份断言检查配置引用和回读，不扩大为网络行为结论。

独立只读审查未发现阻塞性问题：URL 检查复用既有路径且发生在 stdin 前；供应商名称保持字面身份；section 规则保留内部空白；README 与 man 的陈述未超出证据。该审查与实际执行的测试分别记录，避免将静态判断写成动态验证。

按用户指定的 [Research-Paper-Writing-Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills) 组织本报告：价值对应三个已复现的保存错误；说明区分配置语法与字面身份；证据包含失败基线及通过结果；覆盖修改和删除、非法和合法输入；设计复用现有校验路径与一个小型标题 helper。

- [源码与回归测试提交](https://github.com/LIghtJUNction/zcalld/commit/cf94ccbecc8a2f99c6eb498238c8efaa462ba68c)
- 本报告：`docs/validation/config-consistency-20261120.md`
- 结构化证据：`docs/evidence/config-consistency-20261120.json`
- 报告与证据副本：[Google Drive / Project z](https://drive.google.com/drive/folders/1vjkFko8onQm9WiScrw7QBopK9vNGTLx0)
- 上一轮：[模型配置校验与原值保留验收](model-validation-20261120.md)
