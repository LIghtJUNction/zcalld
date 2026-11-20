# SSMCP 原生监听 P1 验收记录

2026-11-20 的 Linux 开发候选已完成原生监听单元测试、严格 Clippy、CLI 构建，以及真实终端到假模型服务的闭环验收。终端验收共 94 项检查通过，实际产生 4 次预期的 HTTP 生成请求。

本记录对应下表中的冻结源码和二进制。它没有把源码版本号、接收 ACK 或单项开发验收当作正式发布完成。

## 固定输入

| 输入 | 精确提交 |
| --- | --- |
| zcalld 产品 | `28f41167449379aedf4f27874c8151ec6196754b` |
| SSMCP 产品 | `ed2c6e665c13f4310fc626f01f360ff7056a52ac` |
| SSMCP 测试脚本 | `197b0c2f4c97062620e8b1a73883de6a859c275d` |

SSMCP 产品和测试提交的 `src` tree 都是 `3108668d36bfc324d4cd6467194812ee9f50c431`；测试脚本从精确提交归档后以只读方式运行。zcalld 的 525 个冻结源码文件和测试归档的 1039 个文件，在对应验证前后均保持原哈希。

zcalld 使用以下原有 vendor gitlinks；本次功能没有升级底层库：

| 依赖 | 精确提交 |
| --- | --- |
| zcall | `45df84f93848a9f7dc66eb7da953d18d14bb08f7` |
| zconv | `0af1f7eb0766ff0ce9db5cee7c6184eff511a8c1` |
| zplugin | `00d566eedcff7b2932e90b9dfeb44910c783d305` |
| zsys | `808fcfdb73584ca7ca122e20b76f0209f56ab0f7` |

## 原生构建

验证使用独立 target、单编译任务、离线锁文件、关闭增量编译和开发调试符号。以下命令均成功：

~~~sh
cargo test -p zcalld --locked --offline native_listener_ -- --nocapture
cargo clippy -p zcalld --all-targets --locked --offline -- -D warnings
cargo build -p zcalld --bin zcalld --locked --offline
~~~

七项原生回归覆盖参数、私有 Unix socket 与有界帧、原连接及 conversation 身份、输入原文、未自动导入的历史上下文、响应大小、工具调用与控制字符拒绝。

冻结 CLI 的 SHA256 是：

~~~text
afc254df0cc19e839a3f7a5f2132f43199fb703f1507b3846dc52a1ac2e10000
~~~

该候选在验证时仍报告工作区基线版本 `0.3.1`。这不是声称已发布的 v0.3.1 包含监听功能；随后通过统一发布工具准备的版本需要对应自己的构建和安装验收。

## 真实终端路径

验收脚本是 SSMCP 仓库中的 [scripts/e2e_zcalld_listener.py](https://github.com/LIghtJUNction/ssmcp/blob/197b0c2f4c97062620e8b1a73883de6a859c275d/scripts/e2e_zcalld_listener.py)。它启动独立账号路由、真实 SSMCP PTY、原生 `zcalld listen` 与 `zcalld serve`，并把生成请求交给可控的本地 HTTP 假服务。

外层使用 `bwrap` 清空环境、隔离网络和进程、只读挂载产品与测试输入，并提供私有临时目录。持久写入限于本次报告目录。没有调用真实模型、生产登录或外部 agent。

用户入口是实际 composer 的 **Next caller** 加 `/send <正文>`，由 PTY 输入并按 Enter 提交；普通 Enter 的原有请求也在同一终端实际提交，用于验证队列隔离。

| 场景 | 已观察结果 |
| --- | --- |
| 普通 Enter 与监听登记 | 原请求保持 queued；登记监听不会开始模型生成。 |
| 显式 /send | 真实 Unix 接收先得到 accepted ACK，假服务在受控等待后才返回模型结果。 |
| 原文与路由 | 中文、空格、换行、缩进和字面 shell 片段保持完整；只有配置中的模型 route 被映射到假上游。 |
| 忙碌时再发送 | 明确 rejected，第二条草稿仍可编辑，没有额外生成请求。 |
| 成功结果 | 原 user 输入在 JSONL 中先于唯一 assistant 结果；结果回执绑定原 delivery 和 conversation。 |
| HTTP 500 | 已 accepted 的输入不被改写成拒收；另有 failed 执行结果，上游错误正文不进入会话。 |
| 归档后迟到回复 | 原历史字节不变，迟到结果被拒绝。 |
| 断线 | 生成被取消；新连接没有旧租约、待发送消息或回执；所有历史均没有僵尸结果。 |
| 重复与冲突 | 真实 Unix 重复回复得到相同回执和同一个 history message；更改内容返回明确冲突。 |
| 已关闭租约 | close 本身成功；之后的旧凭据回复返回明确的租约过期错误。 |
| 请求计数 | 每次合法 HTTP 尝试在匹配计划前入账；最终恰好 4 次计划内请求、0 次意外请求。 |

测试使用具体协议错误验证负例，worker 或传输故障不能算作预期拒绝。三个正常返回的执行回执分别为 completed、failed、completed，均有 `history_saved=true` 和 `shown=true`。120 列和 48 列、各 30 行的真实 VT 已保存供界面检查。

94 项检查总耗时为 10.413 秒；外层包含初始化和校验的耗时为 11.095 秒。所有参与的产品二进制和测试脚本在前后校验中一致。

## 证据与后续版本

私有原始日志、临时凭据和完整回执没有进入此仓库。脱敏报告与来源绑定清单的 SHA256 为：

| 文件 | SHA256 |
| --- | --- |
| report.json | `d2c9b668bf312140418e475fac0f55fc7066a0fa13e11c55a876b71aed189b43` |
| source-binding.json | `eae64848cb04ef6049c10e8ef1466f61383db5debcb190c045bcd8132e464f2a` |

来源清单将产品提交、测试提交、构建报告及二进制哈希逐项绑定；仅传入一个 40 位 SHA 不构成编译来源证明。

本次是 P1 开发候选验收。SSMCP 当时仍有独立折叠界面的旧回归待修，完整发布门禁由 SSMCP 发布流程另行完成。zcalld 后续版本的优化包、安装结果及双端最终版本组合也应另行记录。动态模型/effort 协议、持久多轮上下文和模型工具执行仍不在 P1 的交付范围内。
