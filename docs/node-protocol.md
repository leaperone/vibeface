# Node 协议参考

身份、事件和对等协议版本为 `2`。产品入口为 `vibeface`，共享协议以 `crates/node/src/model.rs` 为准。本文描述 Node HTTP、对等传输、事件、Agent 任务和原生工作观察。

## 身份与数据范围

`cluster_id` 是集群根公钥原始字节的 SHA256，`peer_id` 是设备公钥原始字节的 SHA256，均编码为 64 个小写十六进制字符。地址不参与身份计算。

集群使用 `ClusterState { genesis, checkpoint, events }`。根身份签署初始 `Genesis`，其中包含根公钥及初始 Node、客户端成员。个人根私钥留在可信客户端和加密恢复包，Node 不持有根私钥。

`Member` 包含 `subject_id`、`public_key`、`kind`、`name`、`node_roles` 和 `manage`。`kind` 为 `node` 或 `client`。Node 成员只能具有本机启用角色，不能持有管理权限。客户端没有 Node 角色，只有明确授权的管理客户端能签署成员变更。

角色为 `gateway`、`relay`、`replica` 和 `scheduler`。`config.json` 控制本机能力，集群成员授权限制设备在该集群中的权限。公共中继能力独立于私人集群授权。

管理事件含 `version`、`cluster_id`、`epoch`、`parents`、`issuer`、`action` 和 `signature`。授权动作为 `grant`，撤销动作为 `revoke`。签名目录验证因果授权，不能通过并发事件自行扩大权限，已知撤销优先。管理恢复由根签名的更高 epoch checkpoint 建立新授权，旧 epoch 不能覆盖新授权。

所有共享会话、事件、附件、任务及通知按 `cluster_id` 分区。公网发现目录公开 Peer 签名声明的 Cluster ID、服务和路由，可按已知 Peer ID 或 Cluster ID 查询。声明不证明成员授权，目录不发布签名成员图、设备名称、项目或会话。一个物理 Node 可运行自己的调度执行，同时为其他集群转发密文。

## Peer ID 术语

内部类型使用 `PeerId`，身份方法使用 `peer_id()`，浏览器 SDK 使用 `peerId()`。JSON、RPC、邀请、连接记录、Peer 握手、发现记录、中转签名、维护响应和更新日志统一使用 `peer_id`。mDNS 只广告和读取 `peer_id`。

恢复挑战使用 `target_peer_id`，blob 下载清单使用 `source_peer_id`。`cluster_id` 是集群编号，`subject_id` 仍是签名成员编号。输入边界拒绝旧 `node_id` 字段，不保留旧名别名、双读或双写。当前数据目录格式为 epoch 6，旧目录不自动迁移。见 [Peer ID 命名与数据目录](peer-id-migration.md)。

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
| `daily_decision_limit` | 本 Node 每个集群的 UTC 每日自主管理调用次数，默认 20；本人主动聊天不受此额度限制 |

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

`manager_capabilities` 声明本机 SDK 能力。两种 Manager 均支持流式进度、取消请求和本机原生会话延续，均不支持原生 steer 或 Provider run 对账。Cursor 文本支持增量和快照，Codex 文本是按 item 替换的快照。`cancel=true` 表示可请求取消；Codex 的 `cancel_confirmation=false` 表示 CLI 退出不能证明分离的工具进程已退出。`verified=false` 不表示已通过当前环境的真实模型验收。配置存在、进程可达和已验证是不同状态。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins` 和本机 HTTP 别名组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/app.js`、`/transport.js`、`/style.css` | GET | 内置可信网页客户端 |
| `/generated/vibeface_secure.js`、`/generated/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | Node 公开身份与能力信息 |
| `/api/local/setup` | POST | 本机网页读取一次性初始化邀请或已有集群的当前入口 |
| `/api/local/join/request` | POST | 本机运行中的 Node 提交节点邀请和名称，保存申请收据 |
| `/api/local/join/accept` | POST | 本人以申请编号及完整 review hash 确认节点身份和权限 |
| `/api/local/join/status` | POST | 核对原申请并安装已授予的 Node 成员授权，不重发申请 |
| `/api/network/ws` | GET，WebSocket upgrade | 直连、发现、中继及端到端客户端通道 |
| `/api/tools` | POST | 本地管理 Agent 的受限工具调用 |
| `/sdk.v1.SdkCustomToolCallbackService/CallCustomTool` | POST | Cursor SDK 原生工具回调 |

网站不提供用户名密码注册或明文业务 RPC。公网通道始终验证签名成员身份。独立回环入口另用本机启动凭据兑换绑定浏览器 Noise 公钥的 HttpOnly 会话 Cookie，不授予永久成员资格。网页的密钥存储需要安全上下文，普通 LAN HTTP 页面不能降级为无密钥认证。

`/api/local/setup` 要求正文为 JSON 空对象 `{}`。全部本机设置及加入路径要求实际 TCP 来源为 loopback、Host 为允许的字面 localhost 或 loopback 地址，且 Origin 与该入口完整一致。不接受 `Forwarded`、`X-Forwarded-*` 或 `X-Real-IP` 请求头，`0.0.0.0`、`::` 和公网入口不能使用。响应禁止缓存，初始化材料不进入公开的 `/api/info`。

未初始化时返回 `{ kind: "uninitialized", invitation: { url, peer_id, capability } }`，其中一次性能力仍由原有签名建网或恢复协议消耗。已有集群时返回 `{ kind: "initialized", entry: { address, peer_id }, local_launch?: { url, peer_id, token } }`。先尝试原入口的签名身份；可选启动凭据仅用于本人明确选择本机自动访问时导航到独立回环入口，不自动切换浏览器来源。该响应不授予永久成员资格，也不重建协作集群。

节点加入的响应状态为 `idle`、`review`、`pending`、`joined` 或 `uncertain`。`review` 与 `pending` 带 `request_id`、现有 `PairReview` 及其 canonical JSON 的 SHA256 `review_hash`。接受需要明确提交同一编号与 hash，原集群的管理客户端仍须批准。`joined` 只表示 Node 入网，不授予浏览器客户端权限。未知结果先查 `status`，不重新创建请求身份。

WebSocket 建立后先接收文本 `welcome`。直连客户端发送 `{"type":"direct","service":"client"}`。通过中继时发送 `{"type":"dial","target":"PEER_ID","service":"client"}`。收到 `ready` 后，真实两端执行 Noise XX 握手，协议 context 为 `vibeface:client:v1`。客户端钉住目标 Peer ID，服务端在加密通道内验证客户端协作集群授权。

握手后的应用消息均为二进制加密记录。文本应用消息、身份错配、篡改及重放关闭通道。大消息通过 `sealMessage` 和 `openRecord` 分片重组，不能直接把一个超长 JSON 塞入单个 Noise record。

解密后的请求为 `Request { id: string, method, params }`，响应为 `Response { id, result, error? }`。`error` 是错误说明字符串，不包含成功结果。请求 ID 仅用于响应匹配，业务写入仍依靠稳定的消息 ID、任务引用及原目标去重。

| method | params | 用途 |
| --- | --- | --- |
| `info` | `{}` | 当前 Node 的公开身份、角色、版本和初始化状态 |
| `prepare_genesis` | `owner_public_key`、`client_name` | 构造首个成员目录及待签名内容 |
| `bootstrap` | `capability`、`state` | 一次性本机授权安装根签名目录 |
| `authenticate` | `cluster_id` | 验证握手客户端的成员授权，或独立入口已验证的本机会话 |
| `local_access` | 可选 `enabled` | 查询本机自动访问；管理客户端可开关，本机会话可关闭 |
| `directory` | `{}` | 返回当前签名目录及投影 |
| `node_settings` | `{}` | 管理客户端或有效本机会话读取当前节点 manager、roots、默认程序路径及角色和依赖准备状态 |
| `configure_node` | `manager`、`roots` | 管理客户端或有效本机会话验证并保存当前节点设置，更新后续操作读取的本机快照 |
| `rpc` | 下节 RPC 对象 | 执行业务读写 |
| `subscribe` | `{}` | 订阅本集群的 `changed` 消息及其语义 payload |
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
| `recovery_challenge` | `cluster_id`、`owner_public_key`、可选 `genesis` | 生成根身份持钥挑战 |
| `recovery_context` | `challenge_id`、`signature`、新节点的 `state` 与 `capability` | 验证根签名后读取恢复上下文 |
| `prepare_recovery` | `challenge_id`、`members` | 生成更高 epoch checkpoint 及根待签名内容 |
| `recover` | `challenge_id`、`checkpoint`、新节点的 `state` 与 `capability` | 安装根签 checkpoint 并授权新管理客户端 |

`prepare_genesis` 返回 `state` 和 `signing { domain, payload }`。客户端使用本地根身份签名并写入 `state.genesis.signature`。`bootstrap` 和 `authenticate` 返回 `principal`、`state` 和 `view`。`principal` 包含 `cluster_id`、`subject_id`、`epoch`、`can_manage` 和 `source`，不能由客户端自行声明。`source=membership` 使用签名成员目录；`source=local_session` 绑定本机 Node、当前集群和浏览器 Noise 公钥，`can_manage=false`，不参加成员治理。本机设置写入在取得配置锁后重新核对 Cookie 的有效性、Node、集群及浏览器身份；关闭本机访问后，旧 Cookie 不能保存设置。已有签名管理身份优先保留其成员权限。

邀请含管理客户端签名的 `InviteEnvelope`，新设备还要签署持钥证明和最终 review。管理客户端分别签署 review 与治理事件。只有一次性邀请，或只取得未确认的申请，都不能成为成员。在线新增成员不授予管理权限，管理客户端由根签初始化或恢复 checkpoint 明确建立。

恢复包外层和加密正文的版本为 `2`。外层为 `{ version, cluster_id, recovery_epoch, envelope }`。加密正文包含 `version`、`cluster_id`、`root_secret_key`、`genesis`、`recovery_epoch` 和 `decryption_keys`。恢复加密复用 Rust/WASM 的 Argon2id 与 XChaCha20-Poly1305，实现同时验证格式、KDF 参数、身份及完整性。口令和明文根私钥不发给 Node。

恢复流程先验证根签 challenge，再返回可核对的设备目录。客户端明确选择保留成员，生成新的独立管理客户端，并使用本地解锁的根身份签署 checkpoint。已初始化 Node 只能恢复该集群。新 Node 另需本机 capability，持有其他集群的根私钥不能抢占公共主机。业务正文和项目文件不在根恢复包中。

未初始化时，本机 `access --json` 输出含 Peer ID 与一次性 bootstrap capability。集群建立且本机自动访问开启时，它输出独立回环入口，URL fragment 包含短期单次 token。凭据不放入服务日志、公共目录或 DAG。知道 Node 地址或公开身份不能抢占初始化，也不能取得私人集群权限。

独立回环 listener 提供 `/api/local/info`、`/api/local/launch`、`/api/local/exchange` 和 `/api/local/ws`。`launch` 仅接受本机私有启动凭据，`exchange` 消费单次 token 后签发绑定浏览器公钥的 Cookie，`ws` 仍执行 Noise 握手。公开 router 不挂这些入口。仅检查来源 IP、Host 或代理头不足以建立本机授权。关闭 `local_auto_access` 会撤销本机会话、待兑换 token 和连接，普通成员会话保留。

`/api/tools` 只接受本机 SDK 调用期间签发的短期 bearer token，权限固定到一个集群和业务会话。它不是供外部 Bot 保存的长期身份，不能添加或撤销设备、回答审批或验收工单。管理 SDK 使用独立随机端口的回环 listener，不依赖公网或 LAN 地址。

HTTP 成功或通道返回结果表示本次请求已处理。消息是否在目标入队以版本化调度回执为准，工作是否完成仍查询持久任务、准确原生 turn 及结果。

## RPC 方法

`rpc` 通道方法的 `params` 使用 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。已验证客户端确定 `cluster_id`，操作请求中的集群必须一致。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | 无 | 已授权 Node、可达性和当前可查能力 |
| `bots` | 无 | 当前集群的持久机器人定义 |
| `create_bot` | `bot_id`、`conversation_id`、`name`、`instructions`、可选 `project` | 本人显式创建机器人并绑定独立聊天历史；完整请求精确去重 |
| `conversations` | 无 | 本地已知业务会话 |
| `create_conversation` | `conversation_id`、`title` | 以稳定 ID 创建会话，同 ID 同内容可去重 |
| `conversation` | `conversation_id`、可选 `basis_heads` | 本机已知消息、机器人定义、`agent_tasks`、`native_sessions`、观察目标、heads、missing、调度队列及事实新鲜度 |
| `message` | `conversation_id`、`message_id`、`text`、可选 `basis_heads`、`scheduler_node`、`report_results` | 原子保存用户消息、汇报意图和本机 inbox 或 outbox，返回调度回执 |
| `create_task` | `conversation_id`、`task_id`、`title`、`goal`、`acceptance` | 创建显式长期任务；`auto_followup=true` 被拒绝，授权使用 `set_followup` |
| `update_task` | `conversation_id`、`task_id`、`status`、`detail` | 追加长期任务状态变化 |
| `set_followup` | `conversation_id`、`task_id`、`mode`、`expected_version`、可选 `scheduler_node` | 本人设置具体任务的后续权限，模式为 `off`、`report` 或 `continue` |
| `cancel_manager` | `conversation_id`、`trigger` | 本人请求停止指定管理轮，返回取消请求状态，不表示后台任务已停止 |
| `resolve_manager_unknown` | `conversation_id`、`trigger`、`evidence` | 本人核对旧管理轮、子任务及原生工作后解除管理写冻结，不重做旧触发 |
| `cancel_agent_task` | `conversation_id`、`task_ref` | 沿原任务向目标 Agent 请求取消，核对实际停止后回传终态 |
| `native_interrupt` | `conversation_id`、`work_id`、`target_node` | 请求中断已登记的准确原生 turn |
| `native_respond` | `conversation_id`、`work_id`、`target_node`、`request_id`、`response` | 本人回答该原生 turn 当前仍有效的请求 |
| `read_artifact` | `conversation_id`、`blob_hash`、可选 `offset`、`limit` | 分页读取本集群、该会话实际引用的 UTF-8 文本附件 |
| `notifications` | `client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id` | 记录该客户端已读 |
| `export` | `conversation_id` | 导出包含签名事件的快照 |

长期任务状态为 `open`、`working`、`blocked`、`needs_review`、`accepted` 或 `cancelled`。`accepted` 和 `cancelled` 只能由本人设置，Agent 不能将它们改回活动状态。

`bot_id`、`message_id`、`conversation_id`、`task_id`、`task_ref` 和 `call_ref` 标识业务写入及原调用。重试保留原身份、内容和目标；同 ID 换内容会被拒绝。协调工具为 `nodes`、`context`、`send_task`、`task_status`、`call_lookup`、`cancel_task` 和 `observe_session`。只读结果回调只得到查询工具，本机 Shell 和文件工作使用宿主 SDK 工具。

Bot 定义包含独立的 `bot_id`、内部 `conversation_id`、`name`、`instructions`、可选 `project` 和来源 `event_hash`。`project` 为 `{ target_node, cwd }`，只表达执行偏好，不授予权限。`BotCreated` 进入对应聊天的签名 DAG，Store 从事件重建投影。创建新机器人时，新聊天与 Bot 定义在同一事务保存。导入或重建同版本定义不启动模型。

Node 的 manager 配置保存本机 SDK adapter、模型和目录，不保存模型凭证。Provider 凭证由本机运行环境或 Provider 自身保存。Bot 定义保存职责。每个 Bot 的内部聊天 scope 独立，原生 SDK 缓存仍按集群、scope 和本机节点保存。缓存不是共享聊天的事实来源。

`send_task` 在同一事务保存带稳定调用引用的签名任务及调度记录，固定原目标。恢复先用 `call_lookup` 或 `task_status` 查询原任务，不能根据同一文本猜测是否重复，也不能换目标重派。宿主 SDK 的 Shell 和原生写请求没有旧 Operation 账本，结果不明时必须核对真实副作用。

`conversation.scheduler` 包含 `jobs`、`provider`、`model`、`capabilities` 和 `write_frozen`。每个 job 保留 `trigger`、`target_node`、`kind`、`sequence`、`state` 及可用的 `message_id`、`read_only`、`reply_message_id`。`kind` 为 `human`、`autonomous`、`agent_task` 或 `agent_result`。完成消息替换同一调用的临时文字，不把临时文字当成已持久保存的回答。

长期任务保留 `followup_mode` 和 `followup_version`。`agent_tasks` 包含 `task_ref`、来源与目标 Node、`origin_trigger`、`state`、任务文本和结果说明。`native_sessions` 包含 `work_id`、可选 `task_ref`、目标 Node、准确 thread 与 turn、状态、说明和 `pending_requests`。远端摘要保留缓存或未知标记，`facts_freshness=local_known_history` 不证明全网最新。

原生控制按已登记的 `work_id` 找到原 thread 和 turn，再核对目标、会话及当前有效请求。审批仅本人可回答，过期请求被拒绝。中断请求发出或 SDK 进程退出都不直接证明后台工作已经停止。

`read_artifact` 返回 `{ blob_hash, text, offset, next_offset, size }`。偏移与长度按 UTF-8 字节计算，`limit` 范围为 4 至 65536，默认 65536。偏移必须位于字符边界；下一页使用返回的 `next_offset`，为 `null` 时结束。Node 核对签名附件引用、长度和哈希，正文未同步、非 UTF-8 或仅凭其他会话的哈希猜读都会拒绝。

## 可靠调度回执

可靠调度版本为 `1`。入口在同一事务保存消息、不可改写的汇报意图以及 inbox 或 outbox。远端投递前先查询目标的 `reliable_schedule.version`，不向旧版目标发送可能启动执行的旧协议请求。

`message` 返回 `accepted`、`queued`、`state`、`trigger`、`message_id`、`scheduler_node` 和 `receipt`。目标调度节点持久入队后，回执结构为：

```json
{
	"version": 1,
	"cluster_id": "NETWORK_FINGERPRINT",
	"conversation_id": "conversation-id",
	"trigger": "SIGNED_EVENT_HASH",
	"target_node": "SCHEDULER_PEER_ID",
	"state": "queued"
}
```

入口仅写入 outbox 时，`accepted=true`、`queued=false`、`state=pending_delivery`。只有目标提交 inbox 后的匹配回执才能确认已入队，HTTP 成功或本机已保存不替代这个确认。`queued=true` 表示已经可靠接收，查询时 `state` 可能已推进至运行或终态。

投递以集群、会话、trigger、原目标和版本核对回执。未确认的 outbox 先向原目标查询同一 trigger 的 `manager_status`，只有经过完整身份、版本、历史和状态校验的新鲜 `not_found` 才允许投递原 trigger。已有匹配持久 job 和回执时直接确认接单，job 为 `unknown` 也不能重派。查询失败、过期或矛盾结果不能作为不存在。发送前再次读取持久 outbox，已经确认的条目不再投递。已投递项继续向原目标查询进展和结果，不因离线或超时改派另一台。

每个会话的本机管理队列有界，最多保留 256 个待处理触发；本机每个集群的未投递 outbox 也上限为 256。单消费者按持久状态领取，本人待处理消息优先于自主后续。提示通知只负责唤醒，周期扫描补上丢失的唤醒。每轮按持久固定的依据准备上下文，后发消息不会隐式算作已处理。

job 状态为 `queued`、`claimed`、`running`、`cancelling`、`completed`、`failed`、`cancelled` 或 `unknown`。`queued` 和确认尚未跨启动边界的 `claimed` 可恢复首次执行。跨重启的 `running` 与 `cancelling` 进入 `unknown`，不自动重新调用 SDK。最终回答和 job 终态一次提交。导入 DAG 或重建视图不创建 job，也不重放模型、工具或执行操作。

取消先封存旧 attempt 的工具权限并排空已接纳工具，再检查 SDK 和关联原生工作的取消证据。`cancelling` 不是停止成功，无法证明终止时进入 `unknown`。旧管理轮保持未知，工具权限隔离后可创建新的只读管理轮。`resolve_manager_unknown` 核对原 attempt 关联的任务及原生工作，仍在运行或结果未知时不解除冻结。人工结论不重跑旧 trigger，已有只读 token 不自动获得写权限。

本机版本化队列、outbox、原生 SDK 会话及观察记录不通过 DAG 迁移。维护空闲检查包含待投递、排队、活动和未知项，新进程通过更新健康门后才消费。存在新调度记录时不承诺任意降级旧程序。

## Agent 任务与原生会话

跨设备任务是签名 `agent_message` 事件，目标设备的协调 Agent 使用自己的宿主工具执行。没有通用远程 Shell RPC，也没有独立 Engine、Binding 或 Operation 协议。

```json
{
	"type": "agent_message",
	"message": {
		"message_id": "stable-agent-message-id",
		"target_node": "TARGET_PEER_ID",
		"origin_trigger": "ORIGINAL_TRIGGER_HASH",
		"in_reply_to": null,
		"kind": "task",
		"text": "要在目标设备完成的工作"
	}
}
```

来源取事件签名作者，不能由消息正文冒充。`task_ref` 是原任务事件 hash。回信使用 `in_reply_to=task_ref` 和同一 `origin_trigger`，回到原来源设备。`kind` 为 `task`、`progress`、`completed`、`failed` 或 `interrupted`。普通进度不启动模型，终态必须来自原目标并通过原任务关联校验。

`send_task` 在同一管理 attempt 内固定 `call_ref`、目标和内容。相同引用返回原任务，同引用改目标或内容拒绝。接纳、inbox、outbox 和回执复用可靠调度事务。导入签名历史只重建视图，不能产生新的执行；丢回执、重连或重启先查原 trigger，不重复启动 SDK。

任务内继续委派时，若目标在同一聊天中已有未结束的 Agent 任务，目标在接纳事务中回传未启动的 `interrupted` 结果，避免两个 Agent 互相等待。本人发起的任务仍可正常排队。

协调 Agent 直接使用宿主 SDK 的 Shell 和文件工具，也可用 `vibeface codex METHOD --json 'JSON_OBJECT' --socket /absolute/codex.sock` 调用 App Server 原生方法。参数不翻译成 Operation。`--watch` 持续输出通知，stdin 可接收本人明确提供的 `{ "id": ..., "result": ... }` 响应。不自动批准、不自动重试写请求。

原生工作启动后，用 `observe_session` 登记准确的 `thread_id` 和 `turn_id`。Node 保存所属会话、来源 trigger、管理 attempt 和可选任务引用，观察进度、审批和终态。它只订阅当前 Socket 已加载的线程；历史补查不能把其他 stdio 进程的线程视为已接管。无有效控制端点的会话保持不可确认。

原生状态为 `running`、`waiting`、`interrupting`、`completed`、`failed`、`interrupted` 或 `unknown`。只有对应 thread 和 turn 的精确事件或只读补查可确认终态。协调 SDK 一轮返回、原生 turn 结束和用户任务完成分别表达，不能互相替代。SDK 已返回而子任务或原生工作仍运行时，调度任务继续等待；工作结束后恢复同一 SDK 会话做只读收尾。

取消沿原任务和原目标发送。排队任务可在启动前原子取消；运行中的任务封存旧工具权限并请求中断真实工作。`cancelling` 或 `interrupting` 仅表示请求中，不能直接回报已停止。无法证明 SDK 或后台进程停止时保留 `unknown`，不换 ID 或设备重跑。一个原生 turn 结束不能解除存在其他未知副作用的管理轮。

本地自动访问与任务、授权记录都使用当前数据格式。单一 `node.sqlite3` 使用 `user_version=6`，制品声明 `data_epoch=6`。Atlas HCL 为 `crates/node/schema.hcl`，`migrations/` 只保留 `202610180001_initial.sql` 和 `atlas.sum`。此版本不迁移或清理旧数据目录。

## 事件 DAG

`SignedEvent` 包含三个字段：

- `hash` 为 `canonical_body` UTF-8 字节的 SHA256。
- `canonical_body` 为原始规范 JSON 字符串，校验时保留并核对这些字节。
- `signature` 为作者 Ed25519 签名，签名输入是 `vibeface:event:v2:` 加十六进制 hash 的 ASCII 字节。

正文结构为：

```text
Event {
  version, cluster_id, scope_id, author,
  sequence, previous, parents, created_at_ms,
  body, blobs
}
```

`previous` 和 `sequence` 校验同一作者的来源链，`parents` 引用当前事件依据的因果前项。父引用和附件列表按序去重。`created_at_ms` 不决定唯一全局顺序，也不能覆盖并发分支。

正文类型包括 `conversation_created`、`bot_created`、`message_posted`、`task_created`、`task_changed`、`agent_message`、`decision_recorded`、`notification_raised`、`notification_acknowledged` 和 `context_checkpoint`。

投影前检查集群、作者证书、签名、hash、来源链、因果依赖及附件。缺口和来源分叉分别保留在 `missing.events`、`missing.blobs` 与 `missing.conflicts`。必要历史未齐或冲突未处理时，禁止继续调度和执行。

事件上限为 1 MiB。附件使用 `BlobRef { hash, size }`，单件上限为 64 MiB，传输分块并校验总长度及 SHA256。附件内容按集群保存，不能跨集群凭 hash 读取。

节点交换 scope 的 heads，按需拉取事件、前驱和附件，再重建 SQLite 视图。并发历史保留为不同 heads。快照和摘要不替代原始依据，历史导入和视图重建不触发 Agent 或外部写操作。

同步先保存已验签的事件，缺少父事件或附件时保留为 `pending`。后台按集群和 scope 保存未完成项，分批继续拉取，也可向其他已授权副本请求同一 hash。单个 scope 失败不阻止其他 scope 推进。内部同步结果区分 `complete`、`partial` 和 `unreachable`，并保留缺口及错误；取得事件不代表其依赖已经完整。

相同 envelope 的重复导入不重建投影。新事件每批最多 1024 项、正文合计最多 8 MiB，同批只重建一次。快照导入使用相同批处理。附件补齐、成员状态改变和实际新增历史仍触发必要重建。完整前沿绑定投影代次，新增历史或恢复后重新核验，已有事件不能代替完整性证明。

对等节点的 `capabilities` 和 `inventory` 可在既有 JSON payload 中声明 `history_batch: { version: 1, max_events: 128, max_bytes: 2096128 }`。接收方仅在来源声明版本 `1` 后发送批读请求：

```json
{
	"type": "history_batch",
	"hash": "REQUESTED_EVENT_HASH",
	"limit": 128,
	"max_bytes": 2096128
}
```

批读返回 `{ version: 1, hash, events }`，包含请求事件及其部分祖先。事件数和实际 JSON 字节量同时受请求与服务端上限约束。接收方核对请求 hash、逐项验签，并验证每项都能从请求事件沿该批的 `parents` 或 `previous` 到达。它不接受不相关事件、重复事件或越界响应。

能力响应未声明版本 `1` 的旧来源继续使用原 `event` 请求。单个事件装不进批预算时，来源可明确返回 `{ version: 1, hash, single_event: true }`，接收方再请求同一 hash 的单事件。能力探测失败、错误或非法批读响应不会触发该回退。外层 Packet、握手及协议版本保持不变。单事件仍受既有 2 MiB Packet 上限约束，部分极端转义的最大正文可能超限，此时保留缺口，不报告完整。

附件请求仍使用 `{ type: "blob", hash, offset, limit }`，响应为 `{ data, size }`，`data` 为 hex。`offset` 按正文的字节计，`limit` 为 1 至 65536。来源核对当前集群、本机及请求方的 Replica 授权，再读取指定范围。来源使用独立只读 SQLite 连接保留读取位置，分页不重复物化或 hash 全文。新建读取者并从非零偏移开始时，SQLite 仍可能扫描一次前缀。现有 `get_blob` 的全文校验保留，接收端仍必须验证最终大小和 hash。

来源最多保留 4 个范围读取者，每个请求方身份最多 2 个，所引用对象合计不超过 128 MiB。该额度不表示进程内存或 WAL 大小。读取者空闲 30 秒或存活 300 秒后退出，额度不足返回 busy。同对象正文修复及成员变更使相关读取者失效，缓存不能代替逐页授权。底层磁盘 I/O 不受这些期限强制中断，超时读取者在真正退出前仍占额度。

接收端按 `(cluster_id, hash, expected_size)` 保存连续前缀，正文位于数据目录的私有 `.part` 文件，进度使用 `local_values` 的 `net1:v1:blob:` 保留前缀。每块先同步文件，再提交偏移。断线或重启后重新核验授权，截去未提交尾部，从已提交位置继续。文件短于进度时重置已有进度，不能通过补零伪造前缀。单对象上限仍为 64 MiB，全网暂存合计最多 256 MiB、64 个对象，未清理和异常占用也计入额度。

收齐只代表传输完成。接收端核对最终大小和 hash，并在同一事务发布完整 blob、重建必要投影和记录 `Cleanup`，之后删除所属暂存文件。启动及周期维护重试清理，完整对象缺失或损坏时保留暂存并报告异常。纯本机清理不要求旧来源仍获授权，继续下载和发布则必须通过当前授权。未完成内容始终不进入正常附件读取或执行上下文。

Agent 附件分页先核对当前会话的签名引用，再完整校验正文并保存私有不可变快照。后续页继续检查当前授权和引用，只从已验证快照取片。快照最多 4 个、正文合计 128 MiB，终页或所属管理调用排空后回收。该路径保留完整性校验，人工 `read_artifact` 仍使用原 UTF-8 和字符边界规则。

DAG 能验证已知历史位置及依赖完整，不能证明全网最新。原始副本全部失联或丢失时，不承诺恢复缺失内容，也不迁移其他设备 SDK 的隐藏状态。

## 发现、对等连接与公共中继

`network` 配置包含 `bootstrap_urls`、`advertise_urls`、`mdns` 和 `relay_enabled`。地址使用 `/api/network/ws` 的完整 WS 或 WSS URL。`bootstrap_urls` 提供可替换的会合入口，`advertise_urls` 是本 Node 声明的候选直连地址。Node 可通过 mDNS 获取局域网候选地址，随后仍要核验身份。

mDNS 按完整服务实例名保存来源，最多 256 个实例，每个实例最多 8 个候选地址。同一设备的多个实例可以提供相同地址，移除一个实例不会撤销其他来源。来源有效期沿用发现库的缓存和撤销通知，Node 轮换复核原实例名，不用最后一次解析时间推定固定租期。接口变化会清除旧代来源，旧发现服务确认退出后才重建。关闭无法确认时停止重建并告警，不能保证强制终止库内线程。

固定 IP 监听只宣告该 IP，回环监听只发现其他设备而不宣告自身。wildcard 监听的宣告与发现都限制在对应地址族，不使用没有接口范围的 IPv6 link-local 候选。接口变化不会自动重绑固定监听地址，也不提供 NAT 打洞。实例冲突后的撤销依赖实际缓存复核，不保证即时生效。

`Reachability` 包含 `peer_id`、`public_key`、可选 `cluster`、`direct_urls`、`relay_urls`、`issued_at`、`expires_at` 和 `signature`。`cluster` 为 `{ cluster_id, services }`，服务为规范排序的 `peer`、`client`。仅当本机当前成员授权和启用配置同时允许 Gateway 时声明 `client`。未加入集群或本机已撤销时不声明集群。成员变动由现有维护循环重新投影，并通过同一发布通知刷新公告。

设备将空签名记录按 canonical JSON 编码，使用 Ed25519 的 `reachability-v2` domain 签名。公告最多 8 KiB，有效期最多 180 秒。签名证明设备声明该路径，不证明设备属于目标集群或路径此刻可达。
普通客户端只保存入口地址和目标 `peer_id`，不选择直连或中继。浏览器将 HTTP 或 HTTPS 入口转换为 `/api/network/ws`，Rust Client 也接受保留完整路径的 WS 或 WSS 端点。客户端首先发送 `dial`，携带 `target` 和 `service: "client"`。目标等于入口本机时，入口接入本地 Client 服务，无需启用公共 `relay_enabled`；其他目标仍须启用公共中继并持有其活动预约。原 `direct` 控制继续用于已验明的直连候选。

本机启动器授予的 `local_session` 保留独立的同源 `/api/local/ws` 通道，由 Cookie 和原目标 Noise 身份约束。该通道直接开始 Noise 握手，不发送网络控制消息，也不查询目录或回退到网络入口。内部 `local` 标记不是用户路由偏好。

若显式入口的 `dial` 失败，客户端才将同一输入端点的 `direct` 路径加入既有队列。该路径仍核验原目标 Peer ID，不向其他身份发送业务请求，也不恢复用户的中继偏好。新版入口正常接入时不增加备用数据通道。完整 WS/WSS 端点保持原路径，包括根路径 `/`；HTTP/HTTPS origin 才补 `/api/network/ws`。

客户端同时向入口发送 `lookup`，核验记录的目标、公钥指纹、签名和时间范围，再尝试记录中的直连或其他中继。普通 Peer 查询与 Cluster 查询共用八字段 canonical JSON 与 `reachability-v2` 验签，`signature` 为 `""`，`cluster` 为 `null` 或合法声明，拒绝旧七字段与旧签名域。有效期最多 180 秒，签发时间最多领先本机 30 秒，每份记录最多包含 8 个直连和 8 个中继 URL。浏览器先验证整份记录，再过滤 HTTPS 页面不能使用的非回环 `ws://` 候选，保留其中可用的 `wss://` 路径。

每次连接最多尝试 16 个直连和 8 个中继候选，输入入口占用一个中继候选；最多同时拨号两条数据路径，整次拨号最多 15 秒。候选失效或失败时继续其他已知路径，首个完成固定目标 Noise 握手的路径胜出，其余尝试和目录查询关闭。页面不跳转到候选地址，浏览器密钥仍属于原客户端站点。入口没有预约或有效可达记录时，连接明确失败，不承诺仅凭 Peer ID 搜索全网。公共中继不替客户端主动拨号目录 URL。

保存连接、邀请和待配对记录只使用地址与 `peer_id`，不写入路由偏好。断线后以相同目标重新选路、认证并订阅。身份不符或成员授权被撤销时拒绝访问。未确认写操作保留原编号并先对账，重连只恢复读取与订阅。

`lookup { target }` 仍按 Peer ID 查询。`lookup_cluster { cluster_id, service }` 扫描同一公共公告目录，返回 `cluster_records { records, truncated }`。响应最多 8 条，并按实际序列化字节限制在 16 KiB 控制帧内。候选按查询轮换，不另存集群公告或授权索引。查询不返回成员姓名、会话或完整授权图。

Web 的稳定目标为 Cluster ID 和最多 8 个明确配置的发现服务。HTTP/HTTPS origin 与完整 WS/WSS 端点分别解析，WS 路径原样保留；邀请分享链接使用对应 HTTP/HTTPS 网页 origin。浏览器最多同时查询两条目录连接，跨来源公平合并，最多尝试 16 个 Peer、每 Peer 两条路径，认证串行。Cluster 每次拨号只走一条选定的 direct 或 relay 路径，不触发普通 Peer 的 lookup、自动扩展或回退；最近成功路径也保存模式并计入同一预算。最近 relay 路径仍须属于当前明确目录，直连前重读最新已知 Gateway 授权。同 Peer 的路径使用已合并公告中最早的过期时间，较新的公告不能为旧路径续期。未知 Peer 只能通过给出它的已配置目录建立中继 Noise 通道，不连接公告提供的任意地址。浏览器已经验证过的有效 Gateway 可以使用公告中的直连地址，包括私网地址。公告不能增加发现服务或获得 `local` 标志。这个策略不阻止已配置域名的 DNS 重绑定，也不保证恶意目录下的可用性。

进入 Ready 前，浏览器核对公告签名、Cluster、Peer 指纹和 TTL，Noise 实际身份及公钥，以及合并后的签名成员目录。`VerifiedMembership.mergeStateJson` 复用 Rust 合并规则，拒绝恢复 epoch 回滚或冲突 checkpoint，保留已知撤销，无需入口覆盖全部无关 heads。验证成功的签名状态先持久保存，即使这个候选随后被拒绝也保留。实际入口须是有效 Gateway，浏览器须是有效 Client，principal 不能扩大已知权限。服务端在认证和后续 membership 请求时持续核对本机 Gateway 授权与配置。成员展示与 nodes 选择同样受浏览器最新已验证投影限制。

单个入口拒绝或未同步授权只淘汰候选。只有有效签名状态确认浏览器撤销才停止自动接入。目录故障或候选耗尽保留目标并退避重试。重连只恢复读取和订阅，pending 保留原 message_id、entry_node 和主体/集群范围，新入口查无记录不解除未知写。`local_session` 只重连原本机 Peer，不参与集群入口切换。

Node 按已知 Peer ID 解析目录结果时逐项验签并核对目标身份，过期或非法记录只影响对应来源。候选交付及拨号前再次检查有效期，健康来源不必等待其他目录全部返回。Web 则在本轮有界查询完成后公平合并各来源。公共发布目录最多保存 2048 个身份，本机解析缓存另有 256 个名额；缓存满时仍可使用本次有效候选，公共查询不读取该私有缓存。

每个去重后的会合入口分别维护目录发布和 relay 预约。入口拒绝中继仍可提供目录服务，预约重试不会反复触发发布。慢发布只影响对应入口，活跃 relay 地址变化在下一次发布中合并。目录发布使用独立短控制连接，预约连接只处理心跳和连接请求。

NAT 后的 Node 主动连接会合入口，在随机 challenge 上签名后预约 relay。中继使用单独的出站 attach 通道连接目标，建立通道后只搬运二进制记录。中继限制为 128 个连接、64 份预约和 32 条电路。每条电路最多传输 64 MiB、持续 30 分钟，并限制每秒流量。中继不解释工单或执行命令。

同一身份在新连接上完成 challenge 后，可以用新的 `Reservation.id` 接管预约。旧连接实际释放前仍占连接额度。新来源自身满额时，拒绝本次接管并保留旧预约。同一身份每个固定 60 秒窗口最多安装 6 代，相邻安装至少间隔 1 秒，断开后仍保留限频历史。接管撤回旧代未完成的 attach，旧任务清理只影响自己的代次，已经 Ready 的数据电路继续工作。新代安装后的连接失败会清理本代，不恢复旧预约。协议不增加代次字段。

连接额度在升级前按真实 TCP 来源 IP 计算，不读取 `X-Forwarded-For` 或 `Forwarded`。初始握手及短控制合计最多 16 个，每来源最多 4 个；已建立连接最多 112 个，每来源最多 16 个。全部连接共用 128 上限，实际连接释放后才归还额度。入站请求每分钟全局最多 600 次、每来源最多 120 次；来源限制触发后不继续消耗全局请求额度。反向代理后的连接共用代理的来源 IP，本规则不推断代理背后的不同用户。

等待 attach 和已建立转发共用电路额度，每来源及每目标各最多 4 条。来源、目标及全局限制分别拒绝当前请求，短控制连接仍受自己的额度限制。HTTP 升级前的速率拒绝返回 429，容量拒绝返回 503；升级后的失败关闭当前连接，不新增 `Control` 消息。64 份预约与 32 条双端电路不能保证同时达到各自上限，实体连接总量及控制余量仍生效。

转发的两个方向独立读写，每向队列最多 4 帧，单帧最多 65535 字节。计入两个方向的队列、读取者和写入者持有的正文后，应用层帧正文最多 786420 字节；该数值不含对象结构、WebSocket 和 TCP 缓冲。两个方向共用 64 MiB 总字节、每秒 2 MiB 和每分钟 30720 帧的额度。每端独立检查 90 秒接收活性，另一端持续发送不能代替该端活性。等待队列空位或单次写入超过 10 秒会关闭本条电路，慢方向不阻止反向读写、独立心跳检查或其他电路继续处理。

私网 Node 在该流上执行独立 Noise 握手并验证成员授权，业务报文还保留集群、来源、目标、请求 ID、签名和去重约束。公共 relay 不需要也不得获得私人集群成员身份。TLS 保护逐跳传输，Noise 保护真实通信两端之间的业务内容。

Node 的一次自动目标拨号总期限为 15 秒，同时最多尝试两条路径，整次最多尝试 16 个直连地址和 8 个中继地址。不同来源共用这些上限。仍有效且最近完成认证的直连路径可在首次尝试时领先最多 250 毫秒，随后允许其他候选竞争。只有完成固定目标的 Noise、Hello 和当前成员授权检查后才能胜出，之后取消其余本地尝试。本机当前成员授权检查拒绝时终止整次拨号，不能换路绕过；路径失败只惩罚对应路径，取消不记为路径失败。

后台维护与按需请求共用按集群和目标隔离的连接任务，登记表最多 128 项，包含正在拨号和仍有效的已就绪连接。链路失效后可建立新代连接，尚未退出的旧连接继续占用对等连接和 socket 准入额度。取消单个等待者不会取消其他等待者的连接任务。连接任务不保存业务请求，等待者只在恢复路径后首次发送自己的请求，发送前重新核对当前授权。

Presence 的每个来源绑定邻居和 `Link.id`，有效期由原签名时间限制为最多 90 秒，重复观察不续租。同一已签名 Presence 从另一合法邻居抵达时仍可保留备用来源。检测到邻居断线、连接代次变化或邻居失去中继授权后，在线判断和发送选路会排除对应间接路径，其他无关连接不能维持该目标的在线状态。

定向 request 与 response 优先使用目标直连，再使用有效下一跳，最后最多向两个合法邻居探索。广播和 Presence 保留独立的转发规则。响应必须来自原请求的预期 Node，仍校验签名、跳数和请求去重。只有本机所有队列都未接纳时，才能明确报告无路径或繁忙；队列接纳后的超时或取消保留为未知结果。

路径变化不能改变已投递任务的目标，也不触发业务写入重放。未知结果须使用原目标和原业务 ID 对账。连接目录和在线状态是有时效的软状态。

## 通知和自动后续

加密通道保留外层 `{ "type": "changed", "payload": ... }`。payload 区分历史变更、`manager_progress`、`native_progress` 与失效提醒。客户端读取 `notifications`，使用稳定 `client_id` 确认 `notification_id`，同一客户端换路由后仍可去重。不同浏览器具有不同 client ID，各自确认。

管理进度包含 `conversation_id`、`trigger`、`invocation_id`、`source_node`、递增 `sequence` 和 `progress`。`progress.type` 为 `text_delta`、`text_snapshot`、`status` 或 `tool`。文字按 `item_id` 关联，delta 追加，snapshot 替换全文。原生进度包含 `conversation_id`、`work_id`、`target_node`、`thread_id`、`turn_id`、可选 `task_ref`、`sequence` 和 `progress`，不能串入其他任务。临时进度不逐 token 写 DAG，持久回答和任务结果仍来自 Store 及签名事件。

客户端读取 `conversation` 时，入口为该已授权会话建立有界的只读观察，从原管理节点和执行节点读取进度。节点间 `progress_read` 校验集群、来源和会话，响应包含 `version`、`cursor`、`reset` 和 `events`。游标缺口或源进程重启要求重新读取持久 snapshot，不生成重放写操作。连接关闭、权限撤销或观察租约过期后停止观察，代理进度不再次转发形成循环。

普通进度、审批提醒和结果通知不调用管理模型。无人在线时保留通知，重新连接后读取。浏览器系统提醒是可选客户端功能，不等于网页关闭后的手机推送。

后续权限使用 `DecisionRecorded.tool_configuration` 中带类型的签名记录，类型为 `followup_intent_v1`、`task_origin_v1` 和 `task_followup_grant_v1`。自然语言或模型传入的参数不构成授权。

`message.report_results` 默认 `true`。汇报意图固定原消息 hash、集群、会话和目标 Scheduler，Agent 任务通过签名消息中的 `origin_trigger` 关联原消息，终态只触发只读总结，不创建隐藏 Task。`false` 保存明确的 `off` 意图，重试不能改变。原始结果与通知始终保留，模型不能借回调获得新的写权限。

`set_followup` 仅本人可用，固定到现有任务的 `task_ref`。`off` 禁止自主后续，`report` 只允许读取结果并保存最终 assistant 回答，`continue` 允许在本任务范围内继续业务工作。稳定 grant ID 配合单调版本，`expected_version` 必须使用会话快照返回的当前 `followup_version`。并发或旧版本修改被拒绝，既有授权不能用重试改变目标。

领取后续、签发工具权限及每次业务写入前都复核本机已验证的最新授权、任务状态和版本。撤销封存旧权限；离线节点尚未获知撤销时，不保证全网立即停止。审批、验收与未知确认始终由本人处理。

目标 Agent 的终态、关联结果事件及回投 outbox 在同一事务保存。来源验证原任务和汇报政策后调度一次只读收尾。原生 turn 结束与协调 SDK 结束分别记录，只有所有已登记工作均有真实终态才完成任务。结果未知时保留状态并查询原调用，不因超时再派给另一台。未授权模型后续不影响原始结果与通知保存。

`daily_decision_limit` 只限制每个 Node 上、每个集群的 UTC 每日自主管理 SDK 调用。本人主动聊天不受这个额度限制。预算先预留，失败调用不保证退回；它不限制宿主 SDK 工具启动的其他模型调用，也不是 token 或账单上限。额度耗尽不把执行结果标为未知。

每条用户消息在签名 `MessagePosted.scheduler_node` 中固定本次调度目标。客户端重试必须保留原 `message_id` 和 `scheduler_node`，网页将两者一同持久化。同一目标通过本地调用账本去重。已取得该消息的入口拒绝改写其目标；未同步节点之间没有全局消息唯一性共识，调用方不能用同一编号改换目标。切换入口后继续工作时提交一条新消息，可选择任意已授权调度节点。这个目标只属于该消息，不是会话或工单的负责人。

消息请求中的 `entry_node` 固定为首次接收的 Gateway。未确认的同一消息只能回原入口核对或重试；换入口可只读查看历史，不能重签原请求。Node 在落盘前核对该字段。
