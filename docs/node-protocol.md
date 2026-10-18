# Node 协议参考

协议版本为 `2`。产品入口为 `vibeface`。业务报文以 `crates/node/src/model.rs` 为准，授权类型和校验以 `crates/secure/src/authorization.rs` 为准。本文描述 Node HTTP、对等传输、事件和执行账本。

## 身份与数据范围

账户、设备、空间、资源和服务分别使用 `acc2_`、`dev2_`、`spc2_`、`res2_` 和 `svc2_` 标识。它们不能互换。公开传输层的 `node_id` 仍是 Node Ed25519 公钥原始字节的 SHA256，编码为 64 个小写十六进制字符。地址不参与身份计算。

客户端独立生成账户根、恢复密钥及日常设备密钥。Node 使用自己的服务密钥。根和恢复私钥不交给 Node，设备持钥证明绑定精确用途和账号。个人空间从账户标识派生，团队空间有独立根和政策。

账户文档、空间文档、设备证书、成员记录、资源记录、Grant、政策提交和 Invocation 都有独立签名 domain。公开签名对象使用 `{payload,issuer,signature}`。正文规范化、对象 hash、授权投影和请求签名由共享 Rust 实现，浏览器调用同一份 WASM。

角色为 `gateway`、`relay`、`replica`、`scheduler` 和 `executor`。`config.json` 控制本机能力，已验证的证书、Grant 与当前政策限制实际访问。公共中继能力不授予业务数据或执行权限。

客户端首次信任来自可信创建、连接卡或已保存的 profile pins。登录响应中的 pins 不能为同一响应自行建立信任。政策 stamp 包含 domain、authority epoch、sequence 和 head。已经保存的政策最低 head 不能被较旧记录覆盖。

客户端 scope 固定为 `{account_id,space_id,device_id}`。业务报文只使用 `space_id`，不接受旧字段别名。Project 是已签登记的资源，一个 Project 可以包含多个会话。事件、附件、执行账本、通知和同步均受空间及资源授权限制。公网发现目录不发布这些私人记录。一个物理 Node 可运行 Agent，也可为其他空间转发密文。

## 本机配置

`config.json` 由 `init` 生成，修改后重启生效。字段名固定，不接受未知字段。

| 字段 | 含义 |
|---|---|
| `listen` | 实际 HTTP 监听地址，默认 `127.0.0.1:4790`，决定可接入的网卡 |
| `public_url` | 展示使用的主入口 origin，公网为 HTTPS |
| `allowed_origins` | 额外允许的浏览器 origin 数组，默认空；`init --allowed-origin` 可重复指定 |
| `allow_insecure_lan` | 明确允许可信 LAN 上的明文 HTTP |
| `roles` | 本机启用的 Node 角色 |
| `roots` | 编码执行允许使用的绝对目录列表 |
| `network` | 公共发现、候选直连地址、mDNS 与公共中继配置 |
| `manager` | 管理 SDK 配置，未启用时为 `null` |
| `codex_socket` | 已有 Codex App Server socket，`null` 时尝试启动私有实例 |
| `codex_bin`、`claude_bin`、`cursor_bin` | 对应 Provider 程序路径 |
| `timeout_secs` | 单次适配器超时，默认 600 秒 |
| `daily_decision_limit` | 本 Node 每个个人空间的 UTC 每日管理调用次数，默认 20 |

`gateway`、`scheduler`、`executor` 依赖 `replica`，配置和设备角色授权均须满足此条件。无需 Agent 的入口可启用 `gateway`、`replica` 和可选 `relay`；纯 `relay` 可以只转发消息而不保存业务副本。当前不提供无本地历史的 Gateway 代理模式。

启用管理 Agent 时，`manager` 业务配置只保留 Provider 和模型，不接受宿主 SDK 程序路径：

```json
{
  "provider": "codex_sdk",
  "model": "your-model-id"
}
```

该对象作为 `manager` 的值，保留 `config.json` 的其他字段。管理 Agent 按本次空间与资源选择唯一只读项目目录，不使用全局工作区或全部执行根。它在固定镜像内使用受信 sidecar，必须另外配置 `execution_isolation` 和受信模型网关，见[执行镜像说明](execution-image.md)。Cursor SDK 管理目前拒绝，不回退到宿主进程。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins` 和本机 HTTP 别名组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/app.js`、`/transport.js`、`/style.css` | GET | 内置可信网页客户端 |
| `/generated/vibeface_secure.js`、`/generated/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | Node 公开身份与能力信息 |
| `/api/network/ws` | GET，WebSocket upgrade | 直连、发现、中继及端到端客户端通道 |

网站不提供用户名密码注册、登录 cookie 或明文业务 RPC。网页的密钥存储需要安全上下文。localhost 或可信 HTTPS 页面可启动客户端，普通 LAN HTTP 页面不能降级为无密钥认证。

WebSocket 建立后先接收文本 `welcome`。直连客户端发送 `{"type":"direct","service":"client"}`。通过中继时发送 `{"type":"dial","target":"NODE_ID","service":"client"}`。收到 `ready` 后，真实两端执行 Noise XX 握手，协议 context 为 `vibeface:client:v2`。客户端从可信入口钉住 Node 公钥和 NodeID，服务端把已验证的客户端授权绑定到该 Noise 会话。

握手后的应用消息均为二进制加密记录。文本应用消息、身份错配、篡改及重放关闭通道。大消息通过 `sealMessage` 和 `openRecord` 分片重组，不能直接把一个超长 JSON 塞入单个 Noise record。

解密后的请求为 `Request { id: string, method, params, invocation? }`，响应为 `Response { id, result, error? }`。`error` 是错误说明字符串，不包含成功结果。请求 ID 仅用于响应匹配，业务写入仍依靠稳定的 message ID 和 operation ID 去重。

| method | params | 用途 |
| --- | --- | --- |
| `info` | `{}` | 当前 Node 的公开信息 |
| `identity` | `{method,params}` | 两阶段账户创建和签名政策管理 |
| `authenticate` | `{account_id,space_id,device_public_key,resource_id?,audience?}` | 按所选资源和宿主验证与 Noise 相同的日常设备，并绑定授权会话 |
| `prepare_invocation` | `{body,intent}` | 先验证独立签名 PrepareIntent，再返回未签 Invocation 模板和最终业务正文 |
| `rpc` | 下节业务对象，顶层另带签名 `invocation` | 按签名正文执行已授权业务 |
| `subscribe` | 含空间、资源和授权 ID 的正文，顶层另带 `invocation` | 订阅当前授权资源的变化 |

`identity` 的 `status` 返回公开初始化状态与 Node 身份。首次创建使用以下步骤：

1. 从本机安装卡取得 Node 公钥、NodeID、capability 和 claim nonce。它们不能从对端自报的声明中学习。
2. 调用 `identity` 的 `node_claim`，正文为 `{capability}`。共享 `verifyNodeClaim` 核对卡中的公钥、nonce、签名与有效期。
3. 客户端调用 `prepareAccountCreation`，将 `nodeRequestJson()` 发给 `node_possession`，取得 Node 对精确服务证书的持钥证明。
4. 客户端调用 `finishAccountCreation`，把生成的 `{pins,authorization}` 发给 `bootstrap`。Node 持久绑定申请设备、账户和证书，消费安装授权。完全相同的请求可以幂等重试，冲突请求拒绝。
5. 客户端保存自己创建的账户和空间 pins，再通过顶层 `authenticate` 登录。

已有账户添加另一台 Node 使用独立的 Service 登记，不重新创建账户，也不复用普通客户端的 Offer。客户端从新 Node 的安装卡独立核对公钥和 claim nonce，取得并验证 `node_claim` 后，使用已有账户的根材料及当前已验证授权调用 `prepareAccountNodeEnrollment`。`requestJson()` 返回 `{intent,pins,bundle}`，发送给新 Node 的 `account_node_possession` 后得到 `{proof}`。

`prepareAccountNodeEnrollmentCommit` 将持钥证明绑定到申请，生成发给当前账户权威 Node 的 `account_node_commit` 请求。权威 Node 原子提交账户政策和回执，返回 `{journal_delta}`。客户端用 `finishAccountNodeEnrollment` 核验回执，再把 `installationJson()` 发给新 Node 的 `account_node_install`。安装响应中的 `pins` 是提交后的 pins，不是安装对象中用于验证原申请的旧 pins。完全相同的持钥、提交及安装请求可幂等重试。

这条登记只增加账户所有的 Node Service 证书，不增加业务 Grant，也不新建 Project。完成后客户端重新 `authenticate`，使用新的账户政策头。只有传输证书的 Gateway 可以转发指向原 Project host 的请求，不能凭登记取得该 Project 的历史或执行权限。

登录返回 `{scope,authorization,pins,resources}`。`authorization` 是 `{journal,grant_leaf}`。客户端用独立保存的 pins 调用 `verifyClientAuthorization`，核对预期账户、空间、自己的设备公钥及授权目标 Node。可见 Project 必须来自已经验证的政策投影。登录返回的目录不等于全空间资源目录。

Project 的宿主为 B、入口为 A 时，客户端分别保存 A 的 Noise 公钥和 B 的已验证服务身份。登录显式选择资源及 B 的 audience。客户端用 `signForwardedClientInvocation` 固定 presenter 为 A，保留原 actor、正文和 B audience，再由 A 转发到 B。A 的本地 Grant 或任务绑定不能给另一个宿主授权，也不能将 B 的公钥误作与 A 握手的 pin。

客户端先构造完整业务意图，确定 action、稳定授权 ID 和资源，再用 `signClientPrepareIntent` 对原始正文和独立已验上下文签名。所有调用，包括直连宿主，都以 `{body,intent}` 请求 `prepare_invocation`。上下文来自已验账户、TaskCreated、TaskBinding 和用户意图，不能从服务端模板复制。资源宿主在读取任务或会话元数据前验证该签名、资源 Grant、真实传输、audience、presenter、nonce 和政策有效期。

返回的 `{invocation,body}` 不能直接盲签。先验证 TaskBinding 和允许的路径规范化，逐项核对最终正文，再用 `signClientInvocation` 或 `signForwardedClientInvocation` 对照自己的 action、ID 和正文签名。最终 Invocation 使用独立 nonce，PrepareIntent 本身不能执行写入。发送时必须使用与签名相同的 body。Node 自己获取并验证政策新鲜度回执，调用方不能自报最新状态。

`InvocationContext.task` 是已验证 TaskCreated 的裸 64 位事件 hash，与 DAG 和 TaskBinding 的 `task_ref` 相同。编码执行实例放在 `execution_session`，管理 Agent 的 SDK 实例放在 `sdk_session`，二者不能混用。本人账户请求的 `sdk_session` 为 null。

政策安装使用 `identity` 的 `policy_commit`，参数为 `{journal}`。签名更新必须通过已有 pins 和当前授权验证；新根不能从这条入口自行取得信任。设备登记、撤销与恢复使用新的签名政策记录，旧 `NetworkState`、邀请和 checkpoint 恢复格式不受支持。

本机 `access --json` 输出含入口 URL、NodeID、公钥及首次认领使用的 capability 和 claim nonce。认领后的输出省略两个 claim 字段，不输出 null。capability 不放入服务日志、公共目录或 DAG。知道 Node 地址或公开身份不能抢占初始化，也不能取得私人网络权限。

### 连接文件与设备邀请

Rust `connection` 模块同时为 CLI 和 WASM 解析入口。`ConnectionRoute` 包含规范化的 HTTP 或 HTTPS origin、目标 `node_public_key` 和 `relay`。公网 Relay 必须使用 HTTPS；回环 HTTP 允许本机验证。URL 不能包含用户名、口令、路径、query 或 fragment。

普通连接卡包含 `{url,node_id,node_public_key,relay}`，NodeID 必须等于目标公钥的 SHA-256。首次认领卡另外包含相同的 `capability` 与 `claim_nonce`。用途不能混合，普通卡不接受空值 claim 字段或未知字段。两种文件均限制为 64 KiB。

设备邀请使用 `{version:2,kind:"vibeface-device-enrollment",endpoint,node_public_key,relay,account_id,offer_hash,expires_at_ms}`。这个引用用于读取权威 Node 上的签名 `EnrollmentOffer`。offer 的签名同时保护目标公钥、路由模式、入口、账户、权限、政策头和有效期。引用与签名内容必须一致，展示名称不是身份证明。

`enrollment_offer_publish` 验证当前管理设备后返回 `{offer_hash,reference}`。`enrollment_offer_get` 查询不到时返回包含原账户和摘要的 `status:"not_found"`。`enrollment_status` 查询不到对应申请时返回原账户、offer 摘要和申请编号，不把损坏记录当成不存在。

`request_id` 由 offer 摘要和完整未签名 `DeviceEnrollmentIntent` 计算。新设备在第一次提交前保存该意图。重连先用同一编号查状态；只有 Node 明确确认不存在时才重发原意图。已有管理设备签署批准、新设备签署持钥证明，最后由权威 Node 原子提交设备证书、邀请消费状态和完成回执。相同请求返回原记录，另一申请不能再次消费已完成的邀请。

Web 将待核对的公开邀请与意图限定到原设备配置、账户和目标。CLI 将独立设备私钥、已验证政策和待核对请求保存在加密 profile 中。两端继续使用有限政策读取与验证，不通过恢复包、Node 私钥或 Relay 登录绕过授权。

管理 SDK 的工具调用走隔离 sidecar 的受信通道，绑定账户、空间、资源、原发起人和当前 SDK 会话。它不提供外部 Bot 可保存的长期身份，不能添加或撤销设备、回答审批或验收工单。

HTTP 成功或通道返回结果表示本次请求已处理。任务是否完成仍查询执行账本及工单状态。

## RPC 方法

`rpc` 通道方法的 `params` 使用 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。每次请求显式填写 `space_id`、`resource_id` 和 `authorization_id`，并在通道请求顶层携带已签 Invocation。操作请求中的空间必须一致。已有会话、任务、会话实例及操作的资源由数据库核对，调用方不能改写归属。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | `resource_id` | 已授权 Node、可达性和当前可查能力 |
| `conversations` | `resource_id` | 本地已知业务会话 |
| `create_conversation` | `resource_id`、`conversation_id`、`title` | 以稳定 ID 创建会话，同 ID 同内容可去重 |
| `conversation` | `conversation_id`、可选 `basis_heads` | 消息、工单、heads、missing，以及执行端可查事实 |
| `context` | `conversation_id`、可选 `blob_hash` | 读取当前资源的 DAG 上下文或由其事件引用的附件 |
| `message` | `conversation_id`、`message_id`、`text`、可选 `basis_heads`、`scheduler_node` | 保存用户消息并请求指定管理 Agent |
| `create_task` | `conversation_id`、`task_id`、`title`、`goal`、`acceptance`、可选 `auto_followup` | 创建工单，自动后续只接受本人授权 |
| `update_task` | `conversation_id`、`task_id`、`status`、`detail` | 追加工单状态变化 |
| `bind_execution` | `conversation_id`、`task_id`、`task_ref`、`target_node`、`session_id`、`executor`、`cwd` | 取得明确执行意图的已签任务绑定 |
| `operate` | `request` | 向指定执行 Node 提交 `OperationRequest` |
| `session` | `target_node`、`session_id` | 查询原执行会话及当前事实 |
| `operation` | `target_node`、`operation_id` | 查询持久执行账本 |
| `notifications` | `resource_id`、`client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id` | 记录该客户端已读 |
| `export` | `conversation_id` | 导出包含签名事件的快照 |
| `reconcile` | `target_node`、`operation_id` | 只读查询实际执行事实，保留 Unknown、资源锁及原接单记录 |
| `resolve_unknown` | `target_node`、`operation_id`、`outcome`、`evidence` | 本人根据证据解决未知结果，不重发操作 |
| `seal_execution` | 完整旧 `binding` | 原执行端确认无活动或未知操作后封存旧绑定 |
| `rebind_execution` | 完整旧 `binding`、`previous_seal`、`target_node`、`session_id`、`executor`、`cwd` | 本人明确封存原绑定后建立新的执行安排 |

工单状态为 `open`、`working`、`blocked`、`needs_review`、`accepted` 或 `cancelled`。`accepted` 和 `cancelled` 只能由本人设置，Agent 不能将它们改回活动状态。`resolve_unknown.outcome` 为 `succeeded` 或 `failed`，证据至少 12 字节。

`message_id`、`conversation_id`、`task_id` 和 `operation_id` 是业务去重身份。重试保留原 ID 和原内容；同 ID 换内容会被拒绝。管理 Agent 的可用工具为 `nodes`、`context`、`create_task`、`update_task`、`bind_execution`、`open_execution`、`send_execution` 和 `execution_status`，实际列表继续按当前 Grant 过滤。

`authorization_id` 标识这次签名调用。`operate` 使用原 `operation_id`，结果不明时仍保留该 ID。查询 `operation` 则生成独立的授权 ID，不能把被查询的操作 ID 当作本次读取的身份。

## 执行请求

以下展示 `operate.request` 的业务字段。实际发送时还必须附上完整、已验证的 `binding` 对象，不能只发送绑定编号：

```json
{
	"operation_id": "stable-operation-id",
	"space_id": "spc2_<64 lowercase hex>",
	"conversation_id": "conversation-id",
	"task_id": "task-id",
	"target_node": "TARGET_NODE_ID",
	"session_id": "logical-session-id",
	"expected_revision": 0,
	"basis_heads": [],
	"action": {
		"type": "open",
		"executor": "codex",
		"cwd": "/absolute/project"
	},
	"resources": ["/absolute/project"]
}
```

新执行会话先创建任务并显式取得绑定。`task_ref` 必须是经过共享事件 verifier 核验的 `TaskCreated.hash`。`basis_heads` 标记本次操作依据的已知历史。目标 Node 在接单前拉取并校验其依赖闭包、派发记录和必要附件。空数组不表示全网没有其他历史。

向已关联工单的执行会话提交新工作时，必须显式提供活动工单的 `task_id`，不能省略编号来绕过验收或取消。执行端独立核对绑定、任务创建依据及当前授权。

| action.type | 其他字段 | 执行器支持 |
|---|---|---|
| `open` | `executor`、`cwd` | Codex、Claude Code、Cursor Agent |
| `send` | `text` | Codex、Claude Code、Cursor Agent |
| `read` | 无 | Codex |
| `steer` | `turn_id`、`text` | Codex |
| `interrupt` | `turn_id` | Codex |
| `respond_request` | 原始 `request_id`、`response` | Codex，仅本人 |

执行器枚举为 `codex`、`claude_code`、`cursor_agent`。逻辑 session ID 与执行器原生 session ID 分开保存。审批回答必须绑定当前仍有效的原请求，Node 不自动批准。

`OperationRequest.binding` 是共享的 `SignedBinding`。客户端先通过已授权的 `bind_execution` 取得证明，再调用共享 `verifyTaskBinding`，用已验证的任务创建事件和自身执行意图逐字段核对，随后签署包含该证明的最终业务正文。执行端实际接单时再次验证，不能只依靠请求中的目标字段。

绑定正文包含 `space_id`、`conversation_id`、`task_ref`、`task_id`、`authority_node`、`generation`、`target_node`、`session_id`、`open_spec`、`previous_binding_hash` 和 `previous_seal`。`task_ref` 固定到真实 `TaskCreated` 事件 hash。该创建事件的作者 Node 对首个执行位置执行持久 CAS，两个调度器向不同目标并发派发也只能取得一份绑定。这个 Node 只负责该绑定的唯一确认，不是会话调度负责人。

已有绑定不替代政策新鲜度。原创建 Node 或当前政策权威不可达时，只有仍满足新鲜度和资源授权的请求可以继续。新绑定无法取得唯一确认时拒绝派发，不能在另一台设备自行建立替代，也不临时创建另一份任务绕过确认。

更换执行位置由本人调用 `rebind_execution`，原执行端先明确封存旧绑定，新代绑定引用前代证明。`unknown` 阻止封存和迁移，不用超时假定原执行已经停止。

`expected_revision` 必须匹配执行 Node 当前会话版本。打开及发送操作将工作目录和额外资源规范化，检查本机父子目录重叠。控制操作不能申请额外资源。此机制不提供跨设备共享文件系统的全局锁。

操作状态如下：

```mermaid
stateDiagram-v2
    [*] --> accepted: 校验并持久接单
    accepted --> running: 本机领取执行
    running --> succeeded: 确认操作成功
    running --> failed: 确认操作失败
    running --> unknown: 回执丢失或进程重启
    unknown --> succeeded: 对账或本人附证据确认
    unknown --> failed: 对账或本人附证据确认
```

入账前的非法请求直接拒绝。`rejected` 也属于终态。导入 DAG 历史不创建执行队列；重启仅恢复本机已持久接单而尚未开始的操作。运行中操作重启后变为 `unknown`，不自动重新执行。

Codex `send` 成功启动 turn 后仍需等待进度与终止事件。`turn/completed` 中的 completed、failed、interrupted 各自保留含义，都不自动把工单变为 `accepted`。

## 事件 DAG

`SignedEvent` 包含三个字段：

- `hash` 为 `canonical_body` UTF-8 字节的 SHA256。
- `canonical_body` 为原始规范 JSON 字符串，校验时保留并核对这些字节。
- `signature` 为作者 Ed25519 签名，签名输入是 `vibeface:event:v2:` 加十六进制 hash 的 ASCII 字节。

正文结构为：

```text
Event {
  version, space_id, resource_id, cause, scope_id, author,
  sequence, previous, parents, created_at_ms,
  body, blobs
}
```

`previous` 和 `sequence` 校验同一作者的来源链，`parents` 引用当前事件依据的因果前项。父引用和附件列表按序去重。`created_at_ms` 不决定唯一全局顺序，也不能覆盖并发分支。

正文类型包括 `conversation_created`、`message_posted`、`task_created`、`task_changed`、`dispatch_recorded`、`execution_observed`、`decision_recorded`、`notification_raised`、`notification_acknowledged` 和 `context_checkpoint`。

投影前检查空间、资源、作者证书、签名、hash、来源链、授权依据、因果依赖及附件。`cause` 保留事件产生时的授权依据，导入历史不能凭作者身份重新授予权限。缺口和来源分叉分别保留在 `missing.events`、`missing.blobs` 与 `missing.conflicts`。必要历史未齐或冲突未处理时，禁止继续调度和执行。

事件上限为 1 MiB。附件使用 `BlobRef { hash, size }`，单件上限为 64 MiB，传输分块并校验总长度及 SHA256。附件只能经当前授权资源中的事件引用读取，不能跨空间或资源凭 hash 读取。

节点交换 scope 的 heads，按需拉取事件、前驱和附件，再重建 SQLite 视图。并发历史保留为不同 heads。快照和摘要不替代原始依据，历史导入和视图重建不触发 Agent 或外部写操作。

DAG 能验证已知历史位置及依赖完整，不能证明全网最新。原始副本全部失联或丢失时，不承诺恢复缺失内容，也不迁移其他设备 SDK 的隐藏状态。

## 发现、对等连接与公共中继

`network` 配置包含 `bootstrap_urls`、`advertise_urls`、`mdns` 和 `relay_enabled`。地址使用 `/api/network/ws` 的完整 WS 或 WSS URL。`bootstrap_urls` 提供可替换的会合入口，`advertise_urls` 是本 Node 声明的候选直连地址。Node 可通过 mDNS 获取局域网候选地址，随后仍要核验身份。

`Reachability` 包含 `node_id`、`public_key`、`direct_urls`、`relay_urls`、`issued_at`、`expires_at` 和 `signature`。设备使用 Ed25519 的 `reachability` domain 签名。记录有效期受限，只按已知 NodeID 查询，不提供全网枚举。签名证明设备声明该路径，不证明设备属于本人或路径此刻可达。

NAT 后的 Node 主动连接会合入口，在随机 challenge 上签名后预约 relay。中继使用单独的出站 attach 通道连接目标，建立通道后只搬运二进制记录。中继限制为 128 个连接、64 份预约和 32 条电路。每条电路最多传输 64 MiB、持续 30 分钟，并限制每秒流量。中继不解释工单或执行命令。

私网 Node 在该流上执行 context 为 `vibeface:peer:v2` 的独立 Noise 握手。业务报文保留空间、来源、目标、请求 ID、签名和去重约束，真实业务访问还须验证资源授权、传输证据与当前政策。公共 relay 不需要也不得获得私人网络成员身份。TLS 保护逐跳传输，Noise 保护真实通信两端之间的业务内容。

request 与 response 指向特定 Node，响应必须来自原请求的预期 Node。路径变化不能改变已绑定执行目标，也不触发业务写入重放。连接目录和在线状态是有时效的软状态。

## 通知和自动后续

加密通道的 `changed` 提醒用于刷新当前授权资源的视图。提醒不是唯一历史来源，重连后重新查询持久状态。客户端读取同一资源的 `notifications`，使用自己的稳定 `client_id` 确认 `notification_id`，同一客户端换路由后仍可去重。不同浏览器具有独立设备身份，各自确认。资源或身份不匹配的读取和 ACK 拒绝。

普通进度、审批提醒和结果通知不调用管理模型。无人在线时保留通知，重新连接后读取。浏览器系统提醒是可选客户端功能，不等于网页关闭后的手机推送。

本人授权自动后续后，产生结果的执行 Node 以 `(result_event_hash, policy_version)` 持久领取一次处理，可选择本机或其他可达调度 Node。委托结果未知时保留状态并查询原调用，不因超时再派给另一台。

`daily_decision_limit` 限制每个 Node 上、每个个人网络的 UTC 每日管理 SDK 调用次数。预算先预留，失败调用不保证退回；它不限制执行器自身调用，也不是 token 或账单上限。

每条用户消息在签名 `MessagePosted.scheduler_node` 中固定本次调度目标。客户端重试必须保留原 `message_id` 和 `scheduler_node`，网页将两者一同持久化。同一目标通过本地调用账本去重。已取得该消息的入口拒绝改写其目标；未同步节点之间没有全局消息唯一性共识，调用方不能用同一编号改换目标。切换入口后继续工作时提交一条新消息，可选择任意已授权调度节点。这个目标只属于该消息，不是会话或工单的负责人。

`reconcile` 在目标执行节点确认账本不存在时返回 `state=not_found`、`operation=null` 和目标节点身份。节点不可达返回错误，不当作不存在。网页允许本人确认旧请求结束后解除本地待核对记录，不自动重发。

## 本机存储格式

身份政策、资源目录、DAG、执行接单、nonce、附件和通知使用同一个 `node.sqlite3`。Atlas 声明为 `crates/node/schema.hcl`，唯一初始 SQL 为 `crates/node/migrations/202610180001_initial.sql`。运行程序从空库执行这份内嵌 SQL，记录结构指纹及 `ready` 状态；运行时不依赖 Atlas CLI。

目录格式为 `vibeface-node`，data epoch 为 `3`，layout version 为 `1`，与政策 authority epoch 和恢复代数分开。事件和会话必须显式引用已签资源目录，没有旧资源默认值。授权、接单和不可重放记录在同一 SQLite 事务中提交，DAG 投影重建不清空这些账本。

本版本不迁移旧库或旧恢复包。旧格式、丢失标记、缺库、空库、结构或指纹不符都在写入和启动 Provider 前拒绝。初始化只接受新的空目录，不清空或改写旧目录。空库重放及与真实 Rust 初始化结果的对照由 `scripts/verify-schema.sh` 验证。
