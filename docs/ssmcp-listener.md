# SSMCP 本地监听

原生 listen 子命令把用户明确发送的一条输入交给正在运行的本地 zcalld 网关，并将执行结果写回原 SSMCP 会话。它只支持 Linux 私有 Unix listener。模型、思考强度和多轮上下文的动态协议仍属于后续阶段。

## 实际入口

先启动配置好的 zcalld serve，再对目标 SSMCP 客户端调用 ssmcp.ui.listener，传入 {"op":"status"}，读取返回的 local_endpoint。端点必须来自用户要操作的那个客户端；命令不会扫描其他会话。

~~~sh
zcalld listen \
  --config /absolute/path/to/config.conf \
  --ssmcp-socket /tmp/ssmcp-1000/listener-PID-RANDOM/receiver.sock \
  --model exact-public-route
~~~

--model 必须是配置中实际存在的精确公共 route。模型清单可用 zcalld models 查看。网关优先使用配置的私有 Unix socket；没有已存在的 Unix socket 时，只接受受控 loopback TCP。生成仍经过现有 /v1/chat/completions 路由、账号和过滤逻辑。--token-env NAME 可显式选择调用网关的令牌环境变量；省略时使用网关配置的兼容 caller token，不把上游 provider token 当成入站身份。

在 SSMCP 的收件人选择器中明确选择 Next caller，然后在 composer 输入：

~~~text
/send 请检查这段代码。
~~~

Enter 提交的是这条显式 /send。普通 Enter 的原有请求队列继续保留，不会被 listen 自动接管。选择原生收件箱等不支持监听的目标时，SSMCP 会拒绝 /send 并保留原草稿和目标。

## 接收与执行

listen 显式登记 enable_reply=true 的本地租约，依次 read、ack，然后调用一次现有生成入口。accepted 仅表示本地程序已接收输入；模型完成或失败会产生单独的 completed / failed 结果。结果不会批准结束整个任务，也不会修改 ui.message / request_finish 的既有规则。

生成期间继续续租。程序一次只处理一个生成；期间收到的新输入会被明确拒收为忙碌，原草稿由 SSMCP 保留。每次模型请求仅包含当前输入及 SSMCP 已准备的引用材料；程序不导入历史、不构造持久上下文、不执行模型工具。

结果只允许写回仍有效的原租约、已 accepted 的消息和原当前会话。切换/归档会话、租约过期或客户端断线后，迟到结果不能进入新会话。程序会停止，不自动重新登记、不重跑模型、不重放未知回复。令牌只保存在当前进程内存中。

--once 在一个生成结果完成回写后退出；--json 输出不含租约令牌的登记、接收和结果回执。--timeout-ms 接受 1000 至 900000 毫秒。用户原始输入和回复正文各最多 16 KiB（按 UTF-8 字节计算）；附加引用材料另受 listener 帧大小及网关请求上限约束。本地模型响应解析上限为 256 KiB，且服从更小的网关配置限制。HTTP 错误正文不会复制到会话中。

## 验证范围

SSMCP 仓库的 scripts/e2e_zcalld_listener.py 使用预先编译的两个项目二进制、真实 owned PTY、隔离账号网关、真实 zcalld serve/listen 进程和可控 fake HTTP provider。它从真实 composer 输入 /send，验证接收与模型完成的先后、普通队列隔离、原文和回写身份、忙碌拒收、模型失败、会话切换及断线不重放。该脚本不构建缺失二进制、不使用真实模型或生产登录。

已验证候选的源码、二进制哈希和 94 项真实终端检查见[验收记录](ssmcp-listener-validation.md)。后续升版、安装包和部署仍需对应各自的精确产物记录。
