# Node 协议参考

协议版本为 `1`。产品入口为 `vibeface`，共享协议以 `crates/node/src/model.rs` 为准。本文描述 Node HTTP、对等传输、事件、调度和工作。

## 身份与数据范围

`network_id` 是个人网络根公钥原始字节的 SHA256，`peer_id` 是设备公钥原始字节的 SHA256，均编码为 64 个小写十六进制字符。地址不参与身份计算。

个人网络使用 `NetworkState { genesis, checkpoint, events }`。根身份签署初始 `Genesis`，其中包含根公钥及初始 Node、客户端成员。个人根私钥留在可信客户端和加密恢复包，Node 不持有根私钥。

`Member` 包含 `subject_id`、`public_key`、`kind`、`name`、`node_roles` 和 `manage`。`kind` 为 `node` 或 `client`。Node 成员只能具有本机启用角色，不能持有管理权限。客户端没有 Node 角色，只有明确授权的管理客户端能签署成员变更。

角色为 `gateway`、`relay`、`replica` 和 `scheduler`。`config.json` 控制本机能力，个人网络成员授权限制设备在该网络中的权限。公共中继能力独立于私人网络授权。

管理事件含 `version`、`network_id`、`epoch`、`parents`、`issuer`、`action` 和 `signature`。授权动作为 `grant`，撤销动作为 `revoke`。签名目录验证因果授权，不能通过并发事件自行扩大权限，已知撤销优先。管理恢复由根签名的更高 epoch checkpoint 建立新授权，旧 epoch 不能覆盖新授权。

所有共享会话、事件、附件、任务及通知按 `network_id` 分区。公网发现目录不发布私人网络成员关系、项目或会话。一个物理 Node 可运行自己的调度执行，同时为其他网络转发密文。

## Peer ID 术语

内部类型使用 `PeerId`，身份方法使用 `peer_id()`，浏览器 SDK 使用 `peerId()`。JSON、RPC、邀请、连接记录、Peer 握手、发现记录、中转签名、维护响应和更新日志统一使用 `peer_id`。mDNS 只广告和读取 `peer_id`。

Peer ID 是设备公钥原始字节的 SHA256，使用 64 个小写十六进制字符。恢复挑战使用 `target_peer_id`，blob 下载清单使用 `source_peer_id`。`network_id` 是网络编号，`subject_id` 是签名成员编号。

## 本机配置

`config.json` 由 `init` 生成。网页通过加密客户端通道修改 `manager` 和 `roots` 后，新操作立即读取；其他字段手动修改后重启生效。字段名固定，不接受未知字段。

| 字段 | 含义 |
|---|---|
| `listen` | 实际 HTTP 监听地址，默认 `127.0.0.1:4790`，决定可接入的网卡 |
| `public_url` | 配对和展示使用的主入口 origin，公网为 HTTPS |
| `allowed_origins` | 额外允许的浏览器 origin 数组，默认空；`init --allowed-origin` 可重复指定 |
| `allow_insecure_lan` | 明确允许可信 LAN 上的明文 HTTP |
| `local_auto_access` | 默认 `true`，通过独立回环入口和本机凭据建立受限本机会话 |
| `roles` | 本机启用的 Node 角色 |
| `roots` | 本机 Agent 可选择的绝对项目目录列表 |
| `network` | 公共发现、候选直连地址、mDNS 与公共中继配置 |
| `manager` | 管理 SDK 配置，未启用时为 `null` |
| `codex_socket` | 已有 Codex App Server Socket，启用 Agent 且为 `null` 时可启动私有实例 |
| `codex_bin` | Codex CLI 程序路径 |
| `timeout_secs` | 单次 SDK 调用超时，默认 600 秒 |
| `daily_decision_limit` | 本 Node 每个网络的 UTC 每日自主管理调用次数，默认 20；本人主动聊天不受此额度限制 |

`gateway`、`scheduler` 依赖 `replica`，配置和配对邀请都会校验此条件。无需 Agent 的入口可启用 `gateway`、`replica` 和可选 `relay`；纯 `relay` 可以只转发消息而不保存业务副本。当前不提供无本地历史的 Gateway 代理模式。

启用管理 Agent 时，`manager` 对象包含以下全部字段。程序路径在后台运行时使用绝对路径。Cursor 模式要求非空 `model`，Codex 模式可用空字符串选择其默认模型。

```json
{
	"provider": "cursor_sdk",
	"workspace": "/absolute/manager-workspace",
	"model": "auto",
	"cursor_sdk_bin": "/absolute/bin/cursor-sdk-bridge",
	"node_bin": "/absolute/bin/node",
	"codex_sdk_script": "/absolute/sidecars/codex-manager.mjs"
}
```

该对象作为 `manager` 的值，保留 `config.json` 的其他字段。`provider` 也可为 `codex_sdk`，Cursor 模式不启动 Node.js sidecar。模型名和程序路径需替换为本机可用配置。

`manager_capabilities` 声明本机 SDK 能力。两种 Manager 均支持流式进度、取消请求和本机原生会话延续，均不支持原生 steer 或 Provider run 对账。Cursor 文本支持增量和快照，Codex 文本是按 item 替换的快照。`cancel=true` 表示可请求取消；`cancel_confirmation=true` 表示取消可被确认：Codex 以停止时受管进程树（含另建进程组的子进程）全部退出为证据，Cursor 以 Provider 终态或受管进程退出为证据。`verified=false` 不表示已通过当前环境的真实模型验收。配置存在、进程可达和已验证是不同状态。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins` 和本机 HTTP 别名组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/app.js`、`/transport.js`、`/style.css` | GET | 内置可信网页客户端 |
| `/generated/vibeface_secure.js`、`/generated/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | Node 公开身份与能力信息 |
| `/api/local/setup` | POST | 本机网页读取一次性初始化邀请或已有网络的当前入口 |
| `/api/local/join/request` | POST | 本机运行中的 Node 提交节点邀请和名称，保存申请收据 |
| `/api/local/join/accept` | POST | 本人以申请编号及完整 review hash 确认节点身份和权限 |
| `/api/local/join/status` | POST | 核对原申请并安装已授予的 Node 成员授权，不重发申请 |
| `/api/network/ws` | GET，WebSocket upgrade | 直连、发现、中继及端到端客户端通道 |
| `/api/tools` | POST | 本地管理 Agent 的受限工具调用 |
| `/sdk.v1.SdkCustomToolCallbackService/CallCustomTool` | POST | Cursor SDK 原生工具回调 |

网站不提供用户名密码注册或明文业务 RPC。公网通道始终验证签名成员身份。独立回环入口另用本机启动凭据兑换绑定浏览器 Noise 公钥的 HttpOnly 会话 Cookie，不授予永久成员资格。网页的密钥存储需要安全上下文，普通 LAN HTTP 页面不能降级为无密钥认证。

`/api/local/setup` 要求正文为 JSON 空对象 `{}`。全部本机设置及加入路径要求实际 TCP 来源为 loopback、Host 为允许的字面 localhost 或 loopback 地址，且 Origin 与该入口完整一致。不接受 `Forwarded`、`X-Forwarded-*` 或 `X-Real-IP` 请求头，`0.0.0.0`、`::` 和公网入口不能使用。响应禁止缓存，初始化材料不进入公开的 `/api/info`。

未初始化时返回 `{ kind: "uninitialized", invitation: { url, peer_id, capability } }`，其中一次性能力仍由原有签名建网或恢复协议消耗。已有网络时返回 `{ kind: "initialized", entry: { address, peer_id }, local_launch?: { url, peer_id, token } }`。先尝试原入口的签名身份；可选启动凭据仅用于本人明确选择本机自动访问时导航到独立回环入口，不自动切换浏览器来源。该响应不授予永久成员资格，也不重建个人网络。

节点加入的响应状态为 `idle`、`review`、`pending`、`joined` 或 `uncertain`。`review` 与 `pending` 带 `request_id`、现有 `PairReview` 及其 canonical JSON 的 SHA256 `review_hash`。接受需要明确提交同一编号与 hash，原网络的管理客户端仍须批准。`joined` 只表示 Node 入网，不授予浏览器客户端权限。未知结果先查 `status`，不重新创建请求身份。

WebSocket 建立后先接收文本 `welcome`。直连客户端发送 `{"type":"direct","service":"client"}`。通过中继时发送 `{"type":"dial","target":"PEER_ID","service":"client"}`。收到 `ready` 后，真实两端执行 Noise XX 握手，协议 context 为 `vibeface:client:v1`。客户端钉住目标 Peer ID，服务端在加密通道内验证客户端个人网络授权。

握手后的应用消息均为二进制加密记录。文本应用消息、身份错配、篡改及重放关闭通道。大消息通过 `sealMessage` 和 `openRecord` 分片重组，不能直接把一个超长 JSON 塞入单个 Noise record。

解密后的请求为 `Request { id: string, method, params }`，响应为 `Response { id, result, error? }`。`error` 是错误说明字符串，不包含成功结果。请求 ID 仅用于响应匹配，业务写入仍依靠稳定的消息 ID、任务引用及原目标去重。

| method | params | 用途 |
| --- | --- | --- |
| `info` | `{}` | 当前 Node 的公开身份、角色、版本和初始化状态 |
| `prepare_genesis` | `owner_public_key`、`client_name` | 构造首个成员目录及待签名内容 |
| `bootstrap` | `capability`、`state` | 一次性本机授权安装根签名目录 |
| `authenticate` | `network_id` | 验证握手客户端的成员授权，或独立入口已验证的本机会话 |
| `local_access` | 可选 `enabled` | 查询本机自动访问；管理客户端可开关，本机会话可关闭 |
| `directory` | `{}` | 返回当前签名目录及投影 |
| `node_settings` | `{}` | 管理客户端或有效本机会话读取当前节点 manager、roots、默认程序路径及角色和依赖准备状态 |
| `configure_node` | `manager`、`roots` | 管理客户端或有效本机会话验证并保存当前节点设置，更新后续操作读取的本机快照 |
| `rpc` | 下节 RPC 对象 | 执行业务读写 |
| `subscribe` | `{}` | 订阅本网络的 `changed` 消息及其语义 payload |
| `prepare_invite` | `kind`、`node_roles`、`request_id` | 生成邀请正文与管理客户端待签名内容 |
| `commit_invite` | `invite`、`capability` | 验证管理客户端签名并登记一次性邀请 |
| `prepare_pair_request` | `invite`、`capability`、`name`、可选 `request_id` | 构造新设备的持钥证明 |
| `pair_request` | `request` | 提交新设备签名的配对申请 |
| `pair_accept` | `request_id`、`signature` | 新设备签名确认本次 review |
| `pair_requests` | `{}` | 管理客户端查看待确认申请 |
| `prepare_pair` | `request_id` | 返回 review、治理事件及两份待签名内容 |
| `pair_confirm` | `request_id`、`inviter_signature`、`event` | 管理客户端签名后授予设备权限 |
| `pair_status` | `request_id` | 查询当前持钥设备的 pending 或 granted 状态 |
| `prepare_revoke` | `subject_id` | 生成客户端待签名撤销事件 |
| `revoke` | `event` | 验证并同步签名撤销 |
| `recovery_challenge` | `network_id`、`owner_public_key`、可选 `genesis` | 生成根身份持钥挑战 |
| `recovery_context` | `challenge_id`、`signature`、新节点的 `state` 与 `capability` | 验证根签名后读取恢复上下文 |
| `prepare_recovery` | `challenge_id`、`members` | 生成更高 epoch checkpoint 及根待签名内容 |
| `recover` | `challenge_id`、`checkpoint`、新节点的 `state` 与 `capability` | 安装根签 checkpoint 并授权新管理客户端 |

`prepare_genesis` 返回 `state` 和 `signing { domain, payload }`。客户端使用本地根身份签名并写入 `state.genesis.signature`。`bootstrap` 和 `authenticate` 返回 `principal`、`state` 和 `view`。`principal` 包含 `network_id`、`subject_id`、`epoch`、`can_manage` 和 `source`，不能由客户端自行声明。`source=membership` 使用签名成员目录；`source=local_session` 绑定本机 Node、当前网络和浏览器 Noise 公钥，`can_manage=false`，不参加成员治理。本机设置写入在取得配置锁后重新核对 Cookie 的有效性、Node、网络及浏览器身份；关闭本机访问后，旧 Cookie 不能保存设置。已有签名管理身份优先保留其成员权限。

邀请含管理客户端签名的 `InviteEnvelope`，新设备还要签署持钥证明和最终 review。管理客户端分别签署 review 与治理事件。只有一次性邀请，或只取得未确认的申请，都不能成为成员。在线新增成员不授予管理权限，管理客户端由根签初始化或恢复 checkpoint 明确建立。

恢复包外层为 `{ version, network_id, recovery_epoch, envelope }`。加密正文包含 `version`、`network_id`、`root_secret_key`、`genesis`、`recovery_epoch` 和 `decryption_keys`。恢复加密复用 Rust/WASM 的 Argon2id 与 XChaCha20-Poly1305，实现同时验证格式、KDF 参数、身份及完整性。口令和明文根私钥不发给 Node。

恢复流程先验证根签 challenge，再返回可核对的设备目录。客户端明确选择保留成员，生成新的独立管理客户端，并使用本地解锁的根身份签署 checkpoint。已初始化 Node 只能恢复该个人网络。新 Node 另需本机 capability，持有其他网络的根私钥不能抢占公共主机。业务正文和项目文件不在根恢复包中。

未初始化时，本机 `access --json` 输出含 Peer ID 与一次性 bootstrap capability。个人网络建立且本机自动访问开启时，它输出独立回环入口，URL fragment 包含短期单次 token。凭据不放入服务日志、公共目录或 DAG。知道 Node 地址或公开身份不能抢占初始化，也不能取得私人网络权限。

独立回环 listener 提供 `/api/local/info`、`/api/local/launch`、`/api/local/exchange` 和 `/api/local/ws`。`launch` 仅接受本机私有启动凭据，`exchange` 消费单次 token 后签发绑定浏览器公钥的 Cookie，`ws` 仍执行 Noise 握手。公开 router 不挂这些入口。仅检查来源 IP、Host 或代理头不足以建立本机授权。关闭 `local_auto_access` 会撤销本机会话、待兑换 token 和连接，普通成员会话保留。

`/api/tools` 只接受本机 SDK 调用期间签发的短期 bearer token，权限固定到一个个人网络和业务会话。它不是供外部 Bot 保存的长期身份，不能添加或撤销设备、回答审批或修改后续授权。管理 SDK 使用独立随机端口的回环 listener，不依赖公网或 LAN 地址。

HTTP 成功或通道返回结果表示本次请求已处理。消息是否在目标入队以版本化调度回执为准，工作是否完成仍查询持久任务、准确原生 turn 及结果。

## RPC 方法

`rpc` 通道方法的 `params` 使用 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。已验证客户端确定 `network_id`，操作请求中的网络必须一致。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | 无 | 已授权 Node、可达性和当前可查能力 |
| `bots` | 无 | 当前个人网络的持久机器人定义 |
| `create_bot` | `bot_id`、`conversation_id`、`name`、`instructions`、可选 `project` | 本人显式创建机器人并绑定独立聊天历史；完整请求精确去重 |
| `conversations` | 无 | 本地已知业务会话 |
| `create_conversation` | `conversation_id`、`title` | 以稳定 ID 创建会话并把本机记为负责人，同 ID 同内容可去重 |
| `conversation` | `conversation_id` | 会话（含负责人）、机器人定义、消息、`works`、`autonomy`、`pending_checks`、`local_node` 和调度队列 |
| `message` | `conversation_id`、`message_id`、`text`、可选 `report_results` | 同一事务保存用户消息和调度 job，返回调度回执 |
| `set_autonomy` | `conversation_id`、`mode`、`expected_version` | 本人设置该聊天的后续权限，模式为 `off`、`report` 或 `continue` |
| `cancel_manager` | `conversation_id`、`trigger` | 本人请求停止指定管理轮，返回取消请求状态，不表示工作已停止 |
| `interrupt_work` | `conversation_id`、`work_id` | 中断该工作的原生 turn，并一并中断它委派出的工作；返回请求状态，终态由执行层确认 |
| `answer_approval` | `conversation_id`、`work_id`、`request_id`、`response` | 本人回答该工作当前仍有效的审批或输入请求 |
| `reconcile` | `conversation_id`、`check_id` | 只读复查一条待核对记录，不重发 |
| `resolve` | `conversation_id`、`check_id`、`evidence` | 本人附证据结清待核对记录，不重做原触发 |
| `read_artifact` | `conversation_id`、`blob_hash`、可选 `offset`、`limit` | 分页读取本网络、该会话实际引用的 UTF-8 文本附件 |
| `notifications` | `client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id` | 写入 `NotificationAcked`，记录该客户端已读 |

`bot_id`、`message_id`、`conversation_id` 和 `call_ref` 标识业务写入及原调用。重试保留原身份和内容；同 ID 换内容会被拒绝。协调工具为 `nodes`、`context`、`dispatch_work`、`work_status`、`interrupt_work` 和 `call_lookup`，只读的结果汇报轮只得到查询工具。宿主 SDK 工具只读取本机，本机修改通过 `dispatch_work` 派发的原生工作完成。

Bot 定义包含独立的 `bot_id`、内部 `conversation_id`、`name`、`instructions`、可选 `project` 和来源 `event_hash`。`project` 为 `{ target_node, cwd }`，只表达执行偏好，不授予权限。`BotCreated` 进入对应聊天的签名日志，Store 增量投影。创建新机器人时，新聊天与 Bot 定义在同一事务保存。导入同版本定义不启动模型。

Node 的 manager 配置保存本机 SDK adapter、模型和目录，不保存模型凭证。Provider 凭证由本机运行环境或 Provider 自身保存。Bot 定义保存职责。每个 Bot 的内部聊天 scope 独立，原生 SDK 缓存按网络和会话保存，不是共享聊天的事实来源。

`conversation.scheduler` 包含 `jobs`、`provider`、`model` 和 `capabilities`。每个 job 保留 `trigger`、`kind`、`sequence`、`basis`、`state`、`attempt`、`pending_check`、`read_only`、`reply_message_id` 及可用的 `message_id`。`kind` 为 `human` 或 `work_result`。负责人不是本机的消息显示为 `state=pending_delivery`。完成消息替换同一调用的临时文字，不把临时文字当成已持久保存的回答。

`autonomy` 包含 `mode` 和 `version`。`works` 的每项包含 `work_id`、`conversation_id`、`target_node`、`trigger`、`dispatcher`、`executor`、`cwd`、`prompt`、`phase`、`summary`、`thread_id`、`turn_id`、`outcome`、`result_text`、`failure_reason`、`open_approvals` 和本机状态 `state`。目标为本机时，`open_approvals` 每项附 `form`（可选的 `choices` 与 `questions`）和 `answering`。

审批只能由本人回答。Node 按 `work_id` 找到原 thread 和 turn，校验回答属于请求提供的选项，App Server 重启后旧请求作废。中断请求发出或 SDK 进程退出都不直接证明工作已经停止。

`read_artifact` 返回 `{ blob_hash, text, offset, next_offset, size }`。偏移与长度按 UTF-8 字节计算，`limit` 范围为 4 至 65536，默认 65536。偏移必须位于字符边界；下一页使用返回的 `next_offset`，为 `null` 时结束。Node 核对签名附件引用、长度和哈希，非 UTF-8 或仅凭其他会话的哈希猜读都会拒绝。

## 可靠调度回执

可靠调度版本为 `2`。`message` 在同一事务保存用户消息和调度 job，job 以 `(conversation_id, trigger)` 为键，`trigger` 为 `message:<message_id>` 或 `work:<work_id>`，重试不会产生第二个 job。

`message` 返回 `accepted`、`state`、`trigger`、`message_id` 和 `receipt`：

```json
{
	"version": 2,
	"network_id": "NETWORK_FINGERPRINT",
	"conversation_id": "conversation-id",
	"trigger": "message:message-id",
	"state": "queued"
}
```

会话负责人由 `ConversationOwner` 决定。负责人不是本机时，消息保存在本机 outbox，`state=pending_delivery`；第一期没有跨设备投递。

每个会话的本机管理队列有界，最多保留 256 个待处理触发。单消费者按持久状态领取，本人消息优先于工作结果。提示通知只负责唤醒，周期扫描补上丢失的唤醒。每轮按领取时固定的版本向量 `basis` 准备上下文，后发消息不会隐式算作已处理。

job 状态为 `queued`、`claimed`、`running`、`cancelling`、`completed`、`failed`、`cancelled` 或 `interrupted`，后四种是终态。`queued` 和确认尚未跨启动边界的 `claimed` 可恢复首次执行。跨重启的 `running` 与 `cancelling` 进入 `interrupted` 并登记待核对，不自动重新调用 SDK。助手回答 `MessagePosted(assistant)` 与 job 终态在同一事务提交，`status` 为 `completed`、`failed`、`stopped` 或 `interrupted`，停止或中断时保留已输出的部分文字。在开始前取消的 job 没有回答。导入事件或投影不创建 job，也不重放模型、工具或执行操作。

```text
queued → claimed → running ─→ completed | failed
             │        └─ cancelling ─→ cancelled（SDK 确认受管进程已退出）
             │                     └─→ interrupted（进程退出未确认，登记待核对）
             └ 重启时 running / cancelling ─→ interrupted（登记待核对）
```

停止回复只停止管理轮。Node 封存旧 attempt 的工具权限并排空已接纳工具，SDK 确认受管进程已退出后 job 进入 `cancelled`；已派发的工作继续运行，停止工作用 `interrupt_work`。`cancelling` 不是停止成功。SDK 进程退出无法确认时 job 进入 `interrupted`，并登记一条 `manager:<attempt>` 待核对记录。`interrupted` 是终态，不冻结会话，后续消息照常处理。

待核对记录统一列在 `conversation.pending_checks`，`kind` 为 `manager_process` 或 `work`。管理进程项（`manager:<attempt>`）记录该 attempt 的 SDK 进程组，`reconcile` 探测这些进程组是否已退出；`resolve` 要求它们均已退出。工作项（`work:<work_id>`）是读不到真实结果的工作，`reconcile` 只读复查原 turn，读到终态即结清；`resolve` 由本人附证据结清。模型不能结清待核对，结论不重跑旧 trigger。

本机调度 job、outbox、工作状态日志和原生 SDK 会话不进入事件日志。维护空闲检查包含排队、活动和待核对项，新进程通过更新健康门后才消费。

## 工作

协调 Agent 用宿主 SDK 的 Shell 和文件工具只读访问本机，不持有 App Server Socket。修改本机通过 `dispatch_work { prompt, cwd, call_ref, parent?, continues? }`。`work_id` 由 `(会话, trigger, call_ref)` 派生，同一轮重试同一 `call_ref` 返回原工作；同 `call_ref` 换内容被拒绝。`cwd` 必须位于授权 `roots` 内。第一期目标只能是本机，执行器为 Codex App Server。

目标 Node 接纳时写入 `WorkDispatched`，在本机日志记下 thread 后再启动 turn。`thread/start` 使用 `approvalPolicy=on-request`、`approvalsReviewer=user`、`sandbox=workspace-write`，`turn/start` 另加 `sandboxPolicy={type:workspaceWrite, writableRoots:[cwd], networkAccess:false}`。Agent 不能回答审批。不自动批准、不自动重试写请求。

工作状态为 `accepted`、`starting`、`running`、`waiting_approval`、`interrupting`、`finished` 和 `unverified`。状态转换只来自执行层的结果或事件，每次转换在同一事务更新本机日志并追加 `WorkProgress`、`ApprovalRequested`、`ApprovalAnswered` 或 `WorkFinished`。`WorkFinished.outcome` 为 `completed`、`failed`、`interrupted`、`rejected`（从未启动）或 `unverified`。

管理轮派发后即结束。工作结束时，若原消息的 `report_results` 不为 `false`，Node 在同一事务把 `work:<work_id>` 入队，下一轮向本人汇报；`report` 模式下这一轮只读。

审批的 `request_id` 为 `hash(work_id, executor_epoch, rpc_id)`。回答先标记为发送中，送达 App Server 后才写 `ApprovalAnswered`。App Server 每次重连 executor epoch 加一，旧请求不能再回答。

Node 未配置 `codex_socket` 时托管 App Server：独立进程组，进程组号写入数据目录的 `codex-app-server.pid`，启动时先结束上次遗留的进程组，退出时 SIGTERM，3 秒后 SIGKILL，异常退出按 1 至 30 秒退避重启。重连后 Node 对未结束的工作调用 `thread/read` 只读复查；读到终态则结束，读不到原 turn 则进入 `unverified` 并列入待核对。都不换 ID 重跑。

本地自动访问与授权记录都使用当前数据格式。单一 `node.sqlite3` 使用 `user_version=6`，制品声明 `data_epoch=6`。Atlas HCL 为 `crates/node/schema.hcl`，`migrations/` 只保留 `202610180001_initial.sql` 和 `atlas.sum`。此版本不迁移或清理旧数据目录。

## 事件 DAG

`SignedEvent` 包含三个字段：

- `hash` 为 `canonical_body` UTF-8 字节的 SHA256。
- `canonical_body` 为原始规范 JSON 字符串，校验时保留并核对这些字节。
- `signature` 为作者 Ed25519 签名，签名输入是 `vibeface:event:v1:` 加十六进制 hash 的 ASCII 字节。

正文结构为：

```text
Event {
  version, network_id, author, seq, prev, deps,
  scope_id, created_at_ms, body, blobs
}
```

每台 Node 在网络中只有一条日志：`seq` 从 1 连续递增，`prev` 为同一作者 `seq - 1` 的 hash。`deps` 引用写入时依据的其他作者事件，按 hash 排序去重。`created_at_ms` 只用于展示。

正文类型为 `conversation_created`、`conversation_owner`、`bot_created`、`message_posted`、`work_dispatched`、`work_progress`、`approval_requested`、`approval_answered`、`work_finished`、`autonomy_set`、`notification_acked` 和 `context_checkpoint`。通知由 `approval_requested` 和 `work_finished` 派生，通知 ID 即该事件 hash。

入库逐个检查 hash、网络、作者证书与签名、`seq` 与 `prev` 的连续性，以及 `deps` 是否已入库。签名只在入库时验证一次。重复事件忽略；序号跳过的事件拒绝；同序号不同内容写入 `log_conflicts`，不锁住会话；缺少 `deps` 的事件进入 `pending_events`，依赖到齐后自动入库。`log_heads` 保存每个作者的 `max_seq` 和 `head_hash`，即本机版本向量。

入库时计算 `lamport = 1 + max(prev, deps 的 lamport)`，读取顺序为 `(lamport, created_at_ms, hash)`，与到达顺序无关且满足因果顺序。投影由 reducer 对每个事件增量执行一次，合并规则满足交换律：同一 `message_id` 取 rank 最小者；`ConversationOwner` 取 `epoch` 最大者；`AutonomySet` 取 `version` 最大者，同版本取更严格的模式。

事件上限为 1 MiB。附件使用 `BlobRef { hash, size }`，单件上限为 64 MiB，正文保存为数据目录下 `blobs/sha256/<前两位>/<hash>` 文件，按个人网络和会话引用授权读取。

第一期删除了节点间的历史同步和附件传输，多 Node 同步在后续阶段按版本向量重建。DAG 能验证已知历史位置及依赖完整，不能证明全网最新。

## 发现、对等连接与公共中继

`network` 配置包含 `bootstrap_urls`、`advertise_urls`、`mdns` 和 `relay_enabled`。地址使用 `/api/network/ws` 的完整 WS 或 WSS URL。`bootstrap_urls` 提供可替换的会合入口，`advertise_urls` 是本 Node 声明的候选直连地址。Node 可通过 mDNS 获取局域网候选地址，随后仍要核验身份。

mDNS 按完整服务实例名保存来源，最多 256 个实例，每个实例最多 8 个候选地址。同一设备的多个实例可以提供相同地址，移除一个实例不会撤销其他来源。来源有效期沿用发现库的缓存和撤销通知，Node 轮换复核原实例名，不用最后一次解析时间推定固定租期。接口变化会清除旧代来源，旧发现服务确认退出后才重建。关闭无法确认时停止重建并告警，不能保证强制终止库内线程。

固定 IP 监听只宣告该 IP，回环监听只发现其他设备而不宣告自身。wildcard 监听的宣告与发现都限制在对应地址族，不使用没有接口范围的 IPv6 link-local 候选。接口变化不会自动重绑固定监听地址，也不提供 NAT 打洞。实例冲突后的撤销依赖实际缓存复核，不保证即时生效。

`Reachability` 包含 `peer_id`、`public_key`、`direct_urls`、`relay_urls`、`issued_at`、`expires_at` 和 `signature`。设备使用 Ed25519 的 `reachability` domain 签名。记录有效期受限，只按已知 Peer ID 查询，不提供全网枚举。签名证明设备声明该路径，不证明设备属于本人或路径此刻可达。

普通客户端只保存入口地址和目标 `peer_id`，不选择直连或中继。浏览器将 HTTP 或 HTTPS 入口转换为 `/api/network/ws`，Rust Client 也接受保留完整路径的 WS 或 WSS 端点。客户端首先发送 `dial`，携带 `target` 和 `service: "client"`。目标等于入口本机时，入口接入本地 Client 服务，无需启用公共 `relay_enabled`；其他目标仍须启用公共中继并持有其活动预约。`direct` 控制用于已验明的直连候选。WS 或 WSS 根地址沿用默认 `/api/network/ws`，显式非根路径保持原值。

本机启动器授予的 `local_session` 保留独立的同源 `/api/local/ws` 通道，由 Cookie 和原目标 Noise 身份约束。该通道直接开始 Noise 握手，不发送网络控制消息，也不查询目录或回退到网络入口。内部 `local` 标记不是用户路由偏好。

客户端同时向入口发送 `lookup`，核验记录的目标、公钥指纹、签名和时间范围，再尝试记录中的直连或其他中继。签名字节使用上述字段顺序的紧凑 JSON，`signature` 为 `""`，与 Rust `Reachability::signing_bytes` 一致。有效期最多 180 秒，签发时间最多领先本机 30 秒，每份记录最多包含 8 个直连和 8 个中继 URL。浏览器先验证整份记录，再过滤 HTTPS 页面不能使用的非回环 `ws://` 候选，保留其中可用的 `wss://` 路径。

每次连接最多尝试 16 个直连和 8 个中继候选，输入入口占用一个中继候选；最多同时拨号两条数据路径，整次拨号最多 15 秒。候选失效或失败时继续其他已知路径，首个完成固定目标 Noise 握手的路径胜出，其余尝试和目录查询关闭。页面不跳转到候选地址，浏览器密钥仍属于原客户端站点。入口没有预约或有效可达记录时，连接明确失败，不承诺仅凭 Peer ID 搜索全网。公共中继不替客户端主动拨号目录 URL。

断线后以相同目标重新选路、认证并订阅。身份不符或成员授权被撤销时拒绝访问，不能通过换路绕过。未确认写操作保留原编号并先对账，重连本身只恢复读取与订阅。

目录结果逐项验签并核对目标身份，过期或非法记录只影响对应来源。候选交付及拨号前再次检查有效期，健康来源不必等待其他目录全部返回。公共发布目录最多保存 2048 个身份，本机解析缓存另有 256 个名额；缓存满时仍可使用本次有效候选，公共查询不读取该私有缓存。

每个去重后的会合入口分别维护目录发布和 relay 预约。入口拒绝中继仍可提供目录服务，预约重试不会反复触发发布。慢发布只影响对应入口，活跃 relay 地址变化在下一次发布中合并，已有预约继续处理心跳和连接请求。

NAT 后的 Node 主动连接会合入口，在随机 challenge 上签名后预约 relay。中继使用单独的出站 attach 通道连接目标，建立通道后只搬运二进制记录。中继限制为 128 个连接、64 份预约和 32 条电路。每条电路最多传输 64 MiB、持续 30 分钟，并限制每秒流量。中继不解释工单或执行命令。

同一身份在新连接上完成 challenge 后，可以用新的 `Reservation.id` 接管预约。旧连接实际释放前仍占连接额度。新来源自身满额时，拒绝本次接管并保留旧预约。同一身份每个固定 60 秒窗口最多安装 6 代，相邻安装至少间隔 1 秒，断开后仍保留限频历史。接管撤回旧代未完成的 attach，旧任务清理只影响自己的代次，已经 Ready 的数据电路继续工作。新代安装后的连接失败会清理本代，不恢复旧预约。协议不增加代次字段。

连接额度在升级前按真实 TCP 来源 IP 计算，不读取 `X-Forwarded-For` 或 `Forwarded`。初始握手及短控制合计最多 16 个，每来源最多 4 个；已建立连接最多 112 个，每来源最多 16 个。全部连接共用 128 上限，实际连接释放后才归还额度。入站请求每分钟全局最多 600 次、每来源最多 120 次；来源限制触发后不继续消耗全局请求额度。反向代理后的连接共用代理的来源 IP，本规则不推断代理背后的不同用户。

等待 attach 和已建立转发共用电路额度，每来源及每目标各最多 4 条。来源、目标及全局限制分别拒绝当前请求，短控制连接仍受自己的额度限制。HTTP 升级前的速率拒绝返回 429，容量拒绝返回 503；升级后的失败关闭当前连接，不新增 `Control` 消息。64 份预约与 32 条双端电路不能保证同时达到各自上限，实体连接总量及控制余量仍生效。

转发的两个方向独立读写，每向队列最多 4 帧，单帧最多 65535 字节。计入两个方向的队列、读取者和写入者持有的正文后，应用层帧正文最多 786420 字节；该数值不含对象结构、WebSocket 和 TCP 缓冲。两个方向共用 64 MiB 总字节、每秒 2 MiB 和每分钟 30720 帧的额度。每端独立检查 90 秒接收活性，另一端持续发送不能代替该端活性。等待队列空位或单次写入超过 10 秒会关闭本条电路，慢方向不阻止反向读写、独立心跳检查或其他电路继续处理。

私网 Node 在该流上执行独立 Noise 握手并验证成员授权，业务报文还保留网络、来源、目标、请求 ID、签名和去重约束。公共 relay 不需要也不得获得私人网络成员身份。TLS 保护逐跳传输，Noise 保护真实通信两端之间的业务内容。

Node 的一次自动目标拨号总期限为 15 秒，同时最多尝试两条路径，整次最多尝试 16 个直连地址和 8 个中继地址。不同来源共用这些上限。仍有效且最近完成认证的直连路径可在首次尝试时领先最多 250 毫秒，随后允许其他候选竞争。只有完成固定目标的 Noise、Hello 和当前成员授权检查后才能胜出，之后取消其余本地尝试。本机当前成员授权检查拒绝时终止整次拨号，不能换路绕过；路径失败只惩罚对应路径，取消不记为路径失败。

后台维护与按需请求共用按个人网络和目标隔离的连接任务，登记表最多 128 项，包含正在拨号和仍有效的已就绪连接。链路失效后可建立新代连接，尚未退出的旧连接继续占用对等连接和 socket 准入额度。取消单个等待者不会取消其他等待者的连接任务。连接任务不保存业务请求，等待者只在恢复路径后首次发送自己的请求，发送前重新核对当前授权。

Presence 的每个来源绑定邻居和 `Link.id`，有效期由原签名时间限制为最多 90 秒，重复观察不续租。同一已签名 Presence 从另一合法邻居抵达时仍可保留备用来源。检测到邻居断线、连接代次变化或邻居失去中继授权后，在线判断和发送选路会排除对应间接路径，其他无关连接不能维持该目标的在线状态。

定向 request 与 response 优先使用目标直连，再使用有效下一跳，最后最多向两个合法邻居探索。广播和 Presence 保留独立的转发规则。响应必须来自原请求的预期 Node，仍校验签名、跳数和请求去重。只有本机所有队列都未接纳时，才能明确报告无路径或繁忙；队列接纳后的超时或取消保留为未知结果。

路径变化不能改变已投递任务的目标，也不触发业务写入重放。未知结果须使用原目标和原业务 ID 对账。连接目录和在线状态是有时效的软状态。

## 通知和自动后续

加密通道保留外层 `{ "type": "changed", "payload": ... }`。payload 区分历史变更、`manager_progress`、`native_progress` 与失效提醒。客户端读取 `notifications`，使用稳定 `client_id` 确认 `notification_id`，同一客户端换路由后仍可去重。不同浏览器具有不同 client ID，各自确认。

管理进度包含 `conversation_id`、`trigger`、`invocation_id`、`source_node`、递增 `sequence` 和 `progress`。`progress.type` 为 `text_delta`、`text_snapshot`、`status` 或 `tool`。文字按 `item_id` 关联，delta 追加，snapshot 替换全文。原生进度包含 `conversation_id`、`work_id`、`target_node`、`sequence` 和 `progress`，不能串入其他工作。临时进度不逐 token 写入事件日志，持久回答和工作结果仍来自签名事件。

客户端读取 `conversation` 时，入口为该已授权会话建立有界的只读观察，从原管理节点和执行节点读取进度。节点间 `progress_read` 校验网络、来源和会话，响应包含 `version`、`cursor`、`reset` 和 `events`。游标缺口或源进程重启要求重新读取持久 snapshot，不生成重放写操作。连接关闭、权限撤销或观察租约过期后停止观察，代理进度不再次转发形成循环。

普通进度、审批提醒和结果通知不调用管理模型。无人在线时保留通知，重新连接后读取。浏览器系统提醒是可选客户端功能，不等于网页关闭后的手机推送。

`message.report_results` 默认 `true`，保存在签名的用户消息中，重试不能改变。为 `false` 时，该消息引出的工作结束后不再入队汇报。原始结果与通知始终保留，模型不能借汇报获得新的写权限。

`set_autonomy` 仅本人可用，Agent token 不能调用，写入 `AutonomySet { mode, version }`。`off` 停止该聊天的自主后续，`report` 让结果汇报轮只读，`continue` 允许汇报轮继续派发工作。未设置时等同版本 `0` 的 `report`。`expected_version` 必须等于会话快照中的当前 `autonomy.version`，否则拒绝。同一版本出现并发记录时取更严格的模式。

领取 job、签发工具权限及每次写工具调用前都复核本机最新的会话授权，写入还要求非只读。撤销封存旧权限。审批、验收与待核对始终由本人处理。

`daily_decision_limit` 只限制每个 Node 上、每个个人网络的 UTC 每日结果汇报轮。本人主动聊天不受这个额度限制。预算先预留，失败调用不保证退回；它不限制宿主 SDK 工具启动的其他模型调用，也不是 token 或账单上限。

会话负责人由 `ConversationOwner { scheduler_node, epoch }` 决定，`epoch` 最大者生效，相同时取 hash 较小者。消息本身不携带调度目标。
