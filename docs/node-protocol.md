# Node 协议参考

协议版本为 `1`，数据格式 `data_epoch=7`，数据库 `user_version=7`。共享结构以 `crates/node/src/model.rs` 为准。本文描述本机配置、HTTP 与浏览器通道、业务 RPC、调度、工作、事件 DAG、写入保护和 Node 之间的 iroh 传输。设计依据见 [统一网络设计](design/unified-network.md)。项目尚未发布正式版，不同 epoch 的数据目录和程序不能混用。

## 身份与数据范围

`network_id` 是个人网络根 Ed25519 公钥，`device_id` 是设备 Ed25519 公钥本身，均编码为 64 个小写十六进制字符。Node 的 `device_id` 同时是它的 iroh `EndpointId`。地址不参与身份计算。

个人网络使用 `NetworkState { genesis, checkpoint, events }`。根身份签署初始 `Genesis`，其中包含根公钥及初始 Node、客户端成员。个人根私钥留在可信客户端和加密恢复包，Node 不持有根私钥。

`Member` 包含 `device_id`、`kind`、`name` 和 `manage`，没有角色。`kind` 为 `node` 或 `client`。任何管理设备都能授予或撤销成员，并可把 `manage` 授予新的客户端成员；Node 成员不能持有管理权限。Node 能做什么由本机 `config.json` 的 `roles` 声明，不写入成员目录。

管理事件含 `version`、`network_id`、`epoch`、`parents`、`issuer`、`action` 和 `signature`。授权动作为 `grant`，撤销动作为 `revoke`，撤销带 `cutoff_seq`，接收方只接受被撤销设备 `seq <= cutoff_seq` 的事件。并发的授予与撤销冲突时撤销优先。根恢复由根签名的更高 epoch checkpoint 建立新授权，旧 epoch 不能覆盖新授权。

一个数据目录只属于一个个人网络，一个 Node 进程只服务一个网络。地址、成员关系、项目和会话都不发布到公共目录。

## 本机配置

`config.json` 由 `init` 生成。网页通过加密客户端通道修改 `manager` 和 `roots` 后，新操作立即读取；其他字段手动修改后重启生效。字段名固定，不接受未知字段。

| 字段 | 含义 |
|---|---|
| `listen` | HTTP 监听地址，默认 `127.0.0.1:4790`，决定可接入的网卡；iroh 默认在同一端口号上监听 UDP |
| `local_listen` | 本机会话的固定回环地址，默认 `127.0.0.1:4791`，让本机 origin 和浏览器存储跨重启不变 |
| `public_url` | 本 Node 的网页 origin，公网为 HTTPS；启用 `gateway` 时作为地址记录的 `gateway_url` |
| `allowed_origins` | 额外允许的浏览器 origin 数组，默认空；`init --allowed-origin` 可重复指定 |
| `allow_insecure_lan` | 明确允许可信 LAN 上的明文 HTTP |
| `local_auto_access` | 默认 `true`，通过独立回环入口和本机凭据建立受限本机会话 |
| `roles` | 本机启用的角色：`gateway`、`relay`、`replica`、`scheduler`；`gateway` 和 `scheduler` 依赖 `replica` |
| `public_entry` | `true` 表示本进程是公共入口，只提供网页、relay 和密文转发，不加入个人网络 |
| `roots` | 本机 Agent 和工作可选择的绝对项目目录 |
| `network` | iroh 传输、relay 和公共入口设置，见下表 |
| `manager` | 管理 SDK 配置，未启用时为 `null` |
| `codex_socket` | 外部自管的 Codex App Server Unix socket；为 `null` 时 Node 自行托管 App Server |
| `codex_bin` | 托管 App Server 使用的 Codex CLI，默认 `codex` |
| `timeout_secs` | 单次 SDK 调用超时，默认 600 秒 |
| `daily_decision_limit` | 本 Node 每个网络的 UTC 每日自主管理调用次数，默认 20；本人主动聊天不受此额度限制 |

`network` 的字段：

| 字段 | `init` 参数 | 含义 |
|---|---|---|
| `bind` | 无 | iroh 的 UDP 地址；省略时在 `listen` 端口上绑定 IPv4 与 IPv6 通配地址，IPv6 失败不阻止启动 |
| `relays` | `--relay URL` | 个人网络中其他 Node 的 relay，最多 16 个 |
| `n0_relays` | 无 | 同时使用 n0 公共 relay，默认 `false` |
| `relay_paths_only` | `--relay-paths-only` | 不绑定直连 socket，只经 relay 连接 |
| `relay_listen` | `--relay-listen ADDR` | 本 Node 运行 iroh relay 的地址，明文 HTTP，公网经反向代理提供 HTTPS |
| `relay_url` | `--relay-url URL` | 其他 Node 访问本 relay 的 URL，默认 `http://<relay_listen>`；监听全部网卡时必填 |
| `entry` | `--entry URL`、`--entry-relay URL` | 公共入口 `{ url, relay }`；`relay` 默认等于 `url`，即反向代理把 `/relay` 转给入口的 relay |

配置了 `network.bind` 或 `relay_paths_only` 时关闭 iroh 的端口映射，不发布未配置的路由器地址。`public_entry=true` 要求 `relay_listen`，且不能同时有 `entry`、`manager` 或 `roots`。

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

`provider` 也可为 `codex_sdk`，Cursor 模式不启动 Node.js sidecar。`manager_capabilities` 声明本机 SDK 能力：两种 Manager 均支持流式进度、取消请求和本机原生会话延续，均不支持原生 steer 或 Provider run 对账。`cancel_confirmation=true` 表示取消可被确认，Codex 以停止时受管进程树全部退出为证据，Cursor 以 Provider 终态或受管进程退出为证据。配置存在、进程可达和真实模型可用是不同状态。

## 数据目录

| 文件 | 内容 |
|---|---|
| `identity.json` | 设备私钥，0600 |
| `config.json` | 本机配置，0600 |
| `data-epoch.json` | `{"data_epoch":7}`，程序拒绝其他 epoch |
| `node.sqlite3` | 成员目录、签名日志、投影和本机表，`user_version=7` |
| `bootstrap.json` | 建网前的一次性本机 capability，公共入口没有 |
| `entry-token` | 公共入口的运营者令牌，64 个十六进制字符，0600；公共入口和使用它的成员 Node 各有一份 |
| `blobs/sha256/<前两位>/<hash>` | 附件正文，`blobs/partial/` 为续传暂存 |
| `local-sessions.json` | 本机会话的 cookie 摘要 |
| `join.json` | `vibeface join` 进行中的请求，重试时沿用同一请求 |
| `codex-app-server.pid` | 托管 App Server 的进程组号 |
| `node.log`、`node.log.1` | Node 日志，0600，启动时超过 16 MiB 轮转一次 |
| `service.*.log`、`update.*.log` | macOS 服务和更新器的标准输出与错误 |

运行中的 Node 用目录锁独占数据目录，并在私有维护 socket 上回答 `vibeface status`。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins`、本机 HTTP 别名，以及地址簿中未撤销成员 Node 的 `gateway_url` 组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口；只有 `/api/client` 接受跨源连接。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/assets/*`、`/transport.js` | GET | 内置网页客户端；`/assets/` 下为带哈希的不可变资源 |
| `/sw.js`、`/manifest.webmanifest`、`/icon.svg` | GET | 离线应用外壳和 PWA 清单，`no-cache` |
| `/generated/vibeface_secure.js`、`/generated/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | `device_id`、`version`、`roles`、`manager_available` 和 `public_entry` |
| `/api/local/setup` | POST | 本机网页读取一次性初始化邀请或已有网络的当前入口；公共入口回 404 |
| `/api/client` | GET，WebSocket upgrade | 浏览器网关 |
| `/api/tools`、`/sdk.v1.SdkCustomToolCallbackService/CallCustomTool` | POST | 只在独立的随机回环端口上提供，供本机管理 SDK 调用协调工具 |

网站不提供用户名密码注册或明文业务 RPC。`/api/local/setup` 要求正文为 `{}`，实际 TCP 来源为 loopback，Host 为字面 localhost 或 loopback 地址，Origin 与该入口完整一致，且不带 `Forwarded`、`X-Forwarded-*` 或 `X-Real-IP`。未初始化时返回 `{ kind: "uninitialized", invitation: { url, device_id, capability } }`，已有网络时返回 `{ kind: "initialized", entry, local_launch? }`。该响应不授予成员资格。

独立回环 listener（`local_listen`）提供 `/api/local/info`、`/api/local/launch`、`/api/local/exchange` 和 `/api/local/ws`。`launch` 只接受本机私有启动凭据，`exchange` 消费 URL fragment 中的单次 token 后签发绑定浏览器公钥的 HttpOnly Cookie，`ws` 仍执行 Noise 握手。公开 router 不挂这些入口。关闭 `local_auto_access` 会撤销本机会话、待兑换 token 和连接。

### 浏览器通道

浏览器连接 `/api/client` 后先发送文本帧 `{"type":"connect","target":"DEVICE_ID"}`：

- `target` 是本 Node 且本 Node 不是公共入口时，Noise 在这里终止。
- `target` 是其他成员 Node 时，网关经 iroh `vibeface/client/1` 打开一条流，双向原样搬运二进制帧，单帧最多 1 MiB。目标不是成员时回 `{"type":"error","code":"target_not_member"}`，连不上时回 `target_unreachable`，随后关闭。
- 公共入口没有地址簿，它把目标当作挂在自己 relay 上的设备去连接。只有持有入口令牌的 Node 能连上这台 relay，其他目标得到 `target_unreachable`。
- 每台网关同时最多转发 256 个会话，超出时回 `gateway_busy` 并关闭。`connect` 首帧须在 10 秒内到达。

随后浏览器与目标执行 `Noise_XX_25519_ChaChaPoly_SHA256`，prologue 为 `vibeface:client:v1`。双方每次会话生成临时 Noise 静态密钥，并在握手 payload 中携带 `Proof { device_id, signature }`，签名覆盖 `"vibeface:noise-static:v1" ‖ prologue ‖ 本次静态公钥`。浏览器的签名来自不可导出的 WebCrypto Ed25519 `CryptoKey`。浏览器钉住目标 `device_id`，编号不符时关闭连接。目标验证浏览器的 `Proof` 后按成员目录授权。转发会话的网关、它前面的 TLS 终止方和 relay 只能看到 Noise 密文。提供网页的 origin 不在此列：它分发浏览器运行的代码，必须是本人自己的 Gateway 或本人自己服务器上的公共入口。

握手后的应用消息均为二进制加密记录，大消息分片重组。解密后的请求为 `{ id, method, params }`，响应为 `{ id, result, error? }`。请求 ID 只用于匹配响应，写入去重依靠业务 ID。Noise 握手须在 10 秒内完成。连接每 20 秒心跳，90 秒无数据关闭，单次调用 120 秒超时，超时的写操作结果未知，需要按业务 ID 对账。每台 Node 同时最多保留 32 个未加入设备的会话，超出时回 `too many devices are joining this Node; try again later`；未加入设备的会话最长 15 分钟，加入后不再计入。

| method | params | 用途 |
| --- | --- | --- |
| `info` | `{}` | 公开身份、主机名、角色、版本、初始化状态和 `connection_entry` |
| `authenticate` | 可选 `network_id` | 验证成员授权或本机会话 |
| `directory` | `{}` | 当前签名目录及投影 |
| `entrances` | `{}` | 地址簿中的签名地址记录，浏览器自行验签 |
| `subscribe` | 可选 `vector` | 订阅 `changed` 推送；浏览器带已见的版本向量，入口落后时回 `behind: true` 并立即同步 |
| `rpc` | 下节业务对象 | 业务读写 |
| `write_state` | `{}` | 写入状态和本机记下的冲突，见 [写入保护](#写入保护) |
| `confirm_writable` | `{}` | 本人确认数据目录不是从备份恢复的，只读或对齐中的 Node 进入可写 |
| `diagnostics` | `{}` | 脱敏诊断包，见 [排障](troubleshooting.md) |
| `node_settings` | `{}` | 读取本机 manager、roots 和依赖准备状态 |
| `configure_node` | `manager`、`roots` | 保存本机设置 |
| `local_access` | 可选 `enabled` | 查询或开关本机自动访问 |
| `invite` | `record` | 管理设备登记签名邀请，返回 `join_id`、`entry_id`、`entry { relay_url, direct_addrs }` 和 `gateway_url` |
| `join` | `secret`、`public_key`、`kind`、`name`、`commit` | 新设备提交加入请求，`public_key` 必须是本通道设备 |
| `join_status` | 可选 `join_id`、`reveal` | 新设备查询或揭示随机数；不带 `join_id` 时管理设备列出所有入口上的待处理请求 |
| `approve` | `join_id` 加 `admin_nonce`、`event` 或 `reject: true` | 管理设备推进或拒绝加入，请求在其他入口时转发过去 |
| `revoke` | `event` | 管理设备签名撤销成员 |
| `recovery_state` | `expires_at_ms`、`signature` | 根身份以绑定本通道设备的两分钟签名读取当前目录 |
| `recover` | `genesis`、`checkpoint`、`capability` | 空 Node 安装根签目录，已建网 Node 安装更高 epoch checkpoint |

未加入的设备只能调用 `info`、`authenticate`、`join`、`join_status`、`recovery_state` 和 `recover`，其他方法要求已验证的成员或本机会话。`recovery_state` 和 `recover` 是根恢复的专用准入：前者要求根私钥对 `{device_id, expires_at_ms, network_id}` 的签名，`device_id` 为本通道设备，有效期最长两分钟；后者在安装前验证 `RootCheckpoint` 的根签名并要求 epoch 更高，验证前没有副作用。所有管理浏览器都丢失时，新浏览器凭恢复文件经这两个方法找回管理权限。每个请求都重新查成员目录；目录变化时，已订阅或正在观察进度的会话重新核对，设备被移除后会话关闭。`confirm_writable`、`diagnostics`、`node_settings` 和 `configure_node` 另要求管理设备或本机会话，`invite`、`approve` 和 `revoke` 要求管理设备。

加入使用承诺加短码。管理设备生成 ticket（入口 `EndpointAddr` 和一次性 secret），以 `https://<gateway>/join#vibeface1…` 链接或二维码交给新设备。新设备提交 `commit = SHA256(public_key ‖ joiner_nonce)`，管理设备随后给出 `admin_nonce`，新设备再揭示 `joiner_nonce`，揭示值与承诺不符时请求作废。两端用双方公钥和随机数各自算出 6 位短码，本人核对一致后管理设备签名 `grant`。加入请求的视图带 `commit`。管理设备在发出 `admin_nonce` 前记下所见请求的设备公钥、`invite_id` 和 `commit`，揭示后自己核对承诺；新设备在揭示前记下 `network_id`、`admin_id` 和 `admin_nonce`。短码只用这些自己固定的值计算，入口之后改动任何一项，管理端不显示短码，新设备停止加入。ticket 默认 10 分钟过期，只能被一个加入请求消耗。

浏览器代本人写入时，`MessagePosted(user)`、`BotCreated`、`ConversationOwner`、`ApprovalAnswered`、本人结清的 `WorkFinished`、`AutonomySet` 和 `NotificationAcked` 的 body 带 `attestation { device_id, signature }`。签名输入为 `vibeface:message:v1:attestation:` 加 `SHA256(canonical_json({network_id, scope_id, body}))` 的十六进制，body 去掉 `attestation`，所以签名绑定网络、会话、事件类型和业务 ID。签名设备必须是未撤销的客户端成员。Node 和协调 Agent 不能代写这些事件。

恢复包外层为 `{ version, network_id, recovery_epoch, envelope }`，加密正文包含根私钥、`genesis`、`recovery_epoch` 和解密材料，使用 Argon2id 与 XChaCha20-Poly1305。口令和明文根私钥不发给 Node。根恢复默认保留现有管理设备。业务正文和项目文件不在恢复包中。

`/api/tools` 只接受本机 SDK 调用期间签发的短期 bearer token，权限固定到一个个人网络和业务会话，不能添加或撤销设备、回答审批或修改后续授权。

## RPC 方法

`rpc` 的 `params` 是 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。已验证的客户端确定 `network_id`。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | 无 | 成员 Node、角色、在线状态、`max_seq` 和能力；同步正在失败的 Node 直接记为离线，不等待探测 |
| `bots` | 无 | 当前网络的机器人定义，名称和说明已合并聊天中的修改 |
| `create_bot` | `bot_id`、`conversation_id`、`name`、`instructions`、可选 `project`、`attestation` | 创建机器人和它的会话，完整请求精确去重 |
| `conversations` | 无 | 本地已知会话 |
| `create_conversation` | `conversation_id`、`title`、首个负责人 | 以稳定 ID 创建会话 |
| `conversation` | `conversation_id` | 会话（含负责人与 `owner_epoch`）、机器人、消息、`works`、`autonomy`、`changes`、`assistant`、`pending_checks`、`local_node` 和调度队列 |
| `message` | `conversation_id`、`message_id`、`text`、可选 `attachments`、`report_results`、`attestation` | 保存本人消息并返回调度回执 |
| `set_owner` | `conversation_id`、`scheduler_node`、`epoch`、`attestation` | 换会话负责人，`epoch` 必须等于当前 `owner_epoch + 1` |
| `set_autonomy` | `conversation_id`、`mode`、`expected_version`、`attestation` | 设置该聊天的后续权限：`off`、`report` 或 `continue` |
| `cancel_manager` | `conversation_id`、`trigger` | 请求停止指定管理轮，不停止已派发的工作 |
| `interrupt_work` | `conversation_id`、`work_id` | 中断工作及它委派出的工作；目标在其他 Node 时由入口转发和重试 |
| `answer_approval` | `conversation_id`、`work_id`、`request_id`、`response`、`attestation` | 回答仍有效的审批，转发到执行它的 Node |
| `reconcile` | `conversation_id`、`check_id` | 只读复查一条待核对 |
| `resolve` | `conversation_id`、`check_id`、选择与可选备注 | 本人结清待核对，工作项写一条带本人签名的 `WorkFinished`，不重做原触发 |
| `upload_attachment` | `conversation_id`、`hash`、`size`、`offset`、`data` | 按 1 MiB 分块上传附件，返回已落盘的偏移 |
| `read_artifact` | `conversation_id`、`blob_hash`、可选 `offset`、`limit`、`binary` | 分页读取该会话实际引用的附件 |
| `notifications` | `client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id`、`attestation` | 写入 `NotificationAcked` |

`bot_id`、`message_id`、`conversation_id` 和 `call_ref` 标识业务写入。重试保留原身份和内容，同 ID 换内容会被拒绝。协调 Agent 的 token 只能调用 `nodes`。

`read_artifact` 默认按 UTF-8 文本分页，返回 `{ blob_hash, text, offset, next_offset, size }`，`limit` 为 4 至 65536 字节，偏移必须位于字符边界。`binary: true` 时每页 512 KiB，只读取请求的区间，返回 `{ blob_hash, offset, size, next_offset, data }`，`data` 为 base64。`next_offset` 为 `null` 时结束。浏览器拼好全部字节后用 SHA-256 与 `blob_hash` 比对，一致才保存文件。附件随引用它的消息由同步按哈希拉到其他成员 Node，还没拉到的入口读取失败，稍后再试。

`changes` 按 rank 列出本会话的助手修改，每项为 `{ change_id, cause, event_hash, created_at_ms, change }`，`change` 即 `AssistantChanged.change`。`assistant` 为 `{ skills: [{ skill_id, name, description }], routines: [{ routine_id, name, prompt, first_run_at_ms, every_minutes, enabled, next_run_at_ms }] }`，skill 是全网共用的库，routines 只列本会话的例行任务，暂停时 `next_run_at_ms` 为 `null`。

`works` 的每项包含 `work_id`、`conversation_id`、`target_node`、`trigger`、`dispatcher`、`executor`、`cwd`、`title`、`prompt`、`phase`、`summary`、`thread_id`、`turn_id`、`outcome`、`result_text`、`failure_reason`、`open_approvals` 和本机状态 `state`。网页以 `title` 为任务名，`prompt` 放在详细信息里。

## 可靠调度回执

可靠调度版本为 `2`。调度 job 以 `(conversation_id, trigger)` 为键，`trigger` 为 `message:<message_id>`、`work:<work_id>`、`task:<work_id>` 或 `routine:<routine_id>:<slot_ms>`，重试和同步都不会产生第二个 job。`message` 返回 `accepted`、`state`、`trigger`、`message_id` 和 `receipt`：

```json
{
	"version": 2,
	"network_id": "NETWORK_ID",
	"conversation_id": "conversation-id",
	"trigger": "message:message-id",
	"state": "queued"
}
```

会话负责人由 `ConversationOwner` 决定。入口不是负责人时，消息写入入口自己的日志并留在本机 outbox，回执 `state=pending_delivery`，入口每 3 秒经 `vibeface/rpc/1` 的 `deliver` 投递一次，负责人按触发事件幂等接纳。投递时才解析负责人，所以换负责人后旧负责人尚未开始的 job 交给新负责人。浏览器收到入口回执即删除自己的 outbox 项。

每个会话的本机管理队列最多 256 个待处理触发，本人消息优先于工作结果和例行任务。每轮按领取时固定的版本向量 `basis` 准备上下文。

负责人每 30 秒检查本机负责的会话中已启用的例行任务，只取不晚于当前时间的最近一个时刻 `slot_ms`，错过的不补跑。本机记下已触发的最近时刻；DAG 中已有该 trigger 的回复（`message_id` 由会话和 trigger 派生）或同一例行任务仍在排队时跳过。早于最近一次修改 5 分钟以上的时刻不运行，所以暂停后恢复不会补跑。例行任务轮可以派发工作，不提供修改助手的工具，回复的 `in_reply_to` 为该 trigger，由它派发的工作结果也回复这个 trigger。领取前例行任务已暂停或删除时 job 取消。换负责人时未开始的例行任务 job 直接丢弃，由新负责人按自己的检查运行。

job 状态为 `queued`、`claimed`、`running`、`cancelling`、`completed`、`failed`、`cancelled` 或 `interrupted`，后四种是终态。跨重启的 `running` 与 `cancelling` 进入 `interrupted` 并登记待核对，不自动重新调用 SDK。助手回答 `MessagePosted(assistant)` 与 job 终态在同一事务提交，`status` 为 `completed`、`failed`、`stopped` 或 `interrupted`，停止或中断时保留已输出的部分文字。

```text
queued → claimed → running ─→ completed | failed
             │        └─ cancelling ─→ cancelled（SDK 确认受管进程已退出）
             │                     └─→ interrupted（进程退出未确认，登记待核对）
             └ 重启时 running / cancelling ─→ interrupted（登记待核对）
```

待核对统一列在 `conversation.pending_checks`，`kind` 为 `manager_process` 或 `work`。同一工作只显示一条：`executor=agent` 的工作结果未知时，目标上那一轮管理进程的待核对不再单独显示，结清工作项时一并结清。模型不能结清待核对。

负责人每轮结束后，会话消息超过 20000 字节时写一条 `ContextCheckpoint`。写检查点时只读取最新检查点之后、它的 `covers` 没有覆盖的事件，结果与读取整个会话相同。检查点是确定性摘录，不是模型写的总结。

本机调度 job、outbox、工作状态日志和原生 SDK 会话不进入事件日志。

## 工作

协调 Agent 用宿主 SDK 的工具只读访问本机，修改通过协调工具 `dispatch_work { title, prompt, cwd, call_ref, target_node?, executor?, parent?, continues? }` 完成。`title` 是给本人看的几个字，最长 120 字符。`work_id` 由 `(会话, trigger, call_ref)` 派生，同一轮重试同一 `call_ref` 返回原工作；结果不明时用 `call_lookup` 查询。其他协调工具为 `nodes`、`context`、`work_status`、`interrupt_work` 和 `read_skill { skill_id }`，只读的结果汇报轮只得到查询工具。

修改助手的工具为 `update_profile { call_ref, name?, instructions? }`、`save_skill { call_ref, skill_id?, name, description, body }`、`remove_skill { call_ref, skill_id }`、`save_routine { call_ref, routine_id?, name, prompt, first_run_at_ms, every_minutes, enabled? }` 和 `remove_routine { call_ref, routine_id }`，只提供给本人消息触发的非只读轮。Node 再核对 trigger 是 `message:<id>` 且该消息是本会话的本人消息，`cause` 取自 trigger，`change_id` 由 `(会话, trigger, call_ref)` 派生，同一 `call_ref` 同内容返回原记录，换内容拒绝，`call_lookup` 也能查到修改。改前值取自投影，没有变化的修改被拒绝。参数不接受其他字段，模型、AI 服务、文件夹和自主度只能在设置里改。修改名称和说明需要会话有机器人；例行任务只能由创建它的会话修改。

目标 Node 接纳 `dispatch_work` 时核对派发方是成员 Node、触发消息带本人签名或会话 `AutonomySet` 为 `continue`，并核对 `cwd` 位于目标自己的 `roots` 内。接纳后写 `WorkDispatched`，在本机日志记下 thread 再启动 turn。`executor=codex` 在目标的 Codex App Server 上开 turn，`executor=agent` 把任务作为目标管理 Agent 的一轮执行，这一轮只能在目标本机启动 Codex 工作。`continues` 在原工作的 thread 上开新 turn，原目标不可达时报错，不改派。

`thread/start` 使用 `approvalPolicy=on-request`、`approvalsReviewer=user`，`turn/start` 另加 `sandboxPolicy={type:workspaceWrite, writableRoots:[cwd], networkAccess:false}`。Agent 不能回答审批，Node 不自动批准、不自动重试写请求。工作改动了 `cwd` 之外的文件时，Node 只在 `node.log` 记一行 `work changed a file outside its directory`，不拦截、不告警。

工作状态为 `accepted`、`starting`、`running`、`waiting_approval`、`interrupting`、`finished` 和 `unverified`。每次转换在同一事务更新本机日志并追加对应事件。`WorkFinished.outcome` 为 `completed`、`failed`、`interrupted`、`rejected`（从未启动）或 `unverified`；只有 `unverified` 可以被本人签名的 `WorkFinished` 取代。工作结束时若原消息的 `report_results` 不为 `false`，负责人把 `work:<work_id>` 入队，下一轮向本人汇报。

审批的 `request_id` 为 `hash(work_id, executor_epoch, rpc_id)`。回答先标记为发送中，送达 App Server 后才写 `ApprovalAnswered`。App Server 每次重连 executor epoch 加一，旧请求不能再回答。

Node 未配置 `codex_socket` 时托管 App Server：执行 `codex app-server --listen unix://<socket>`，socket 位于 0700 的 `/tmp/vibeface-<identity>/` 目录，App Server 放入独立进程组，进程组号写入 `codex-app-server.pid`。启动时先结束上次遗留且命令相符的进程组，退出时 SIGTERM，3 秒后 SIGKILL，异常退出按 1 至 30 秒退避重启。重连后对未结束的工作调用 `thread/read` 只读复查，读不到原 turn 时进入 `unverified`。同一用户的本机进程可以连接这个 socket。

## 事件 DAG

`SignedEvent` 包含三个字段：

- `hash` 为 `canonical_body` UTF-8 字节的 SHA256。
- `canonical_body` 为原始规范 JSON 字符串，校验时保留并核对这些字节。
- `signature` 为作者 Ed25519 签名，签名输入是 `vibeface:event:v1:` 加十六进制 hash 的 ASCII 字节。

```text
Event {
  version, network_id, author, seq, prev, deps,
  scope_id, created_at_ms, body, blobs
}
```

每台 Node 在网络中只有一条日志：`seq` 从 1 连续递增，`prev` 为同一作者 `seq - 1` 的 hash。`deps` 引用写入时依据的其他作者事件。`created_at_ms` 用于展示，排序时只在 `lamport` 相同时作为第二键。浏览器不写日志，它发起的写入由所连 Node 作为作者写入并附 `attestation`。

| 类型 | 主要字段 | 作者 |
|---|---|---|
| `conversation_created` | `title` | 入口 Node |
| `conversation_owner` | `scheduler_node`、`epoch`、`attestation` | 入口 Node |
| `bot_created` | `bot_id`、`name`、`instructions`、`project?`、`attestation` | 入口 Node |
| `message_posted` | `message_id`、`role`、`text`、`attachments`、`report_results?`、`in_reply_to?`、`status?`、`attestation?` | 本人消息由入口写，回答由负责人写 |
| `work_dispatched` | `work_id`、`trigger`、`dispatcher`、`call_ref`、`executor`、`cwd`、`title`、`prompt`、`continues?`、`parent?` | 目标 Node |
| `work_progress` | `work_id`、`phase`、`summary`、`thread_id?`、`turn_id?` | 目标 Node |
| `approval_requested` | `work_id`、`request_id`、`kind`、`summary` | 目标 Node |
| `approval_answered` | `work_id`、`request_id`、`decision`、`attestation` | 入口 Node |
| `work_finished` | `work_id`、`outcome`、`result_text`、`failure_reason?`、`attestation?` | 目标 Node，本人结清时由入口写 |
| `autonomy_set` | `mode`、`version`、`attestation` | 入口 Node |
| `notification_acked` | `notification_id`、`client_id`、`attestation` | 入口 Node |
| `assistant_changed` | `change_id`、`cause`、`change` | 负责人 |
| `context_checkpoint` | `covers`、`summary`、`standing_instructions`、`open_work`、`decisions` | 负责人 |

`assistant_changed.change` 按 `kind` 区分：`profile { name?, instructions? }` 的每项为 `{ before, after }`；`skill { skill_id, before, after }` 与 `routine { routine_id, before, after }` 的 `after` 为 `null` 表示删除。skill 为 `{ name, description, body }`，名称最多 64 字符，说明最多 512 字节，正文最多 16 KiB；例行任务为 `{ name, prompt, first_run_at_ms, every_minutes, enabled }`，名称最多 120 字符，说明最多 8 KiB，`every_minutes` 为 5 至 40320。说明（instructions）最多 8192 字节。这个事件不带 `attestation`，入库时要求 `prev` 或 `deps` 中有同一会话、`message_id` 等于 `cause` 且带 `attestation` 的本人消息，否则拒绝。投影只比较 rank：名称和说明逐项后写者胜并合并进机器人定义，skill 全网按 `skill_id`、例行任务按 `routine_id` 后写者胜并保留删除，修改记录按 `change_id` 取 rank 最小者。

通知由 `approval_requested` 和 `work_finished` 派生，通知 ID 即该事件 hash。执行层内部的命令、diff、推理和工具输出不进入 DAG。

入库逐个检查，顺序为 hash、网络、事件形状与 `attestation`，重复，作者成员资格与 `cutoff_seq`，作者签名，然后才看 `seq` 与 `prev` 的连续性和 `deps` 是否已入库。签名只在入库时验证一次，未通过验证的事件不会写入 `log_conflicts`，也不会触发 `forked`。重复事件忽略；序号跳过的事件丢弃并从缺口重新拉取；同作者同序号不同内容写入 `log_conflicts`，不锁住会话；缺少 `deps` 的事件进入 `pending_events`，依赖到齐后自动入库。`log_heads` 保存每个作者的 `max_seq` 和 `head_hash`，即本机版本向量。

入库时计算 `lamport = 1 + max(prev, deps 的 lamport)`，读取顺序为 `(lamport, created_at_ms, hash)`。投影由 reducer 对每个事件增量执行一次：同一 `message_id` 取因果最早者；`ConversationOwner` 取 `epoch` 最大者；`AutonomySet` 取 `version` 最大者，同版本取更严格的模式；同一审批的回答取更严格的决定；工作状态只前进。这三类同级再相同时取请求身份较小者，请求身份为 `SHA256(attestation 签名输入 ‖ 签名设备)`。同一份已签名请求经不同入口重试时请求身份相同，外层事件 hash 不参与仲裁，重试不会改变结果。

事件上限为 1 MiB。附件使用 `BlobRef { hash, size }`，单件上限 64 MiB，正文是 `blobs/sha256/<前两位>/<hash>` 文件，按网络和会话引用授权读取。

## 写入保护

Node 的写入状态为 `aligning`、`read_only`、`writable` 或 `forked`。只有 `writable` 时 Node 追加自己的事件，其他状态下所有本地写入在追加时被拒绝，调度和执行在进入 `writable` 前不启动。读取、同步和入库其他设备的事件不受影响。

| 状态 | 进入条件 | 离开条件 |
|---|---|---|
| `aligning` | 启动时成员目录中还有其他 Node | 某个对端的 Hello 表明它记录的本机 `max_seq` 不大于本机，且同序号 hash 一致；对端更多时先拉回本机自己的日志 |
| `read_only` | 对齐中且所有其他 Node 都同步失败 | 对端恢复后照常对齐，或本人调用 `confirm_writable` |
| `writable` | 没有其他 Node、对齐完成或本人确认 | 发现分叉 |
| `forked` | 某个对端持有本机作者同序号的不同事件 | 不离开；撤销这台设备后用新数据目录重新加入 |

拒绝写入时的错误为 `this Node is aligning with other Nodes; try again shortly`、`this Node is read-only until you confirm it was not restored from a backup` 和 `this Node's log has forked and it no longer writes`，网页翻译成中文。`confirm_writable` 只记录在本机，不进入 DAG，对 `forked` 无效。

对端发现某个作者的分叉时，保留先到的一条，后到的写入 `log_conflicts`，其他会话照常写入。分叉的 Node 自己在 Hello 中看到同序号不同 hash 时拉取对方那条，记下冲突后进入 `forked` 并停止写入。`write_state` 返回 `{ state, conflicts }`，每条冲突含作者、序号、保留的和放在一边的版本。状态变化时向已订阅的浏览器推送 `{"type":"changed","payload":{"type":"write_state","state":...}}`。

## Node 之间的传输

### iroh Endpoint

每台 Node 运行一个 iroh 1.3 `Endpoint`，`SecretKey` 就是设备密钥。Endpoint 用 Minimal 预设、`MemoryLookup` 和 `RelayMode::Custom`：relay 来自 `network.relays`、本 Node 自己的 relay、公共入口的 relay，以及可选的 n0 公共 relay。Node 不向 n0 DNS、pkarr 或局域网发布地址，对端地址只来自 ticket 和个人网络内部交换的签名地址记录。连接优先打洞直连，失败时经 relay 转发 QUIC 密文。

`Router` 按 ALPN 分发连接。每个请求占一条 QUIC 双向流，帧为 4 字节大端长度前缀加 JSON，单帧最多 16 MiB。

| ALPN | 用途 | 接受的对端 |
|---|---|---|
| `vibeface/sync/1` | `hello`、`membership`、`pull`、`have` | 未撤销的成员 Node |
| `vibeface/blob/1` | 按哈希和偏移读取附件 | 未撤销的成员 Node |
| `vibeface/rpc/1` | 投递、派发、中断、审批转发、进度读取、地址簿交换、加入请求汇总 | 未撤销的成员 Node |
| `vibeface/client/1` | 网关转发的浏览器 Noise 会话，每条流一个会话 | 任意设备，会话本身由 Noise 认证 |
| `vibeface/join/1` | 只开放 `join` 和 `join_status` | 任意设备 |

`sync`、`blob` 和 `rpc` 的每个请求带 `network_id`，与本机不一致时关闭连接。连接接受时按 `remote_id()` 核对成员资格，非成员 Node 被拒绝；成员目录变化时已接受的连接重新核对，对端不再是成员就关闭。撤销还会关闭到被撤销 Node 的出站连接并删除它的地址。公共入口不接受任何入站 iroh 连接。

### 同步

1. 双方交换 `{"type":"hello", network_id, vector: {author: [max_seq, head_hash]}, boot, membership_epoch, membership}`。成员目录摘要不同时交换 `membership`，epoch 不低于本机的目录被合并。
2. 对每个对方领先的作者发 `{"type":"pull", author, from_seq, limit}`，对方按 `seq` 升序返回，每批最多 1024 条、合计 8 MiB。
3. 本机追加事件后向已连接的成员发 `{"type":"have", author, seq, hash}`，对方落后时立即同步。空闲连接每 60 秒重新 Hello。
4. `boot` 是对端本次启动的编号。编号变化说明对端重启，本机立即关闭到它的旧连接并重新同步，不等 QUIC 空闲超时。

同步只写入历史和投影，不重新执行历史中的操作。同步失败按退避重试，最长间隔 30 秒。

### 地址簿

每台 Node 为自己签一条地址记录 `{ version, network_id, device_id, seq, endpoint, gateway_url }`，以 `address` 域签名，`seq` 严格递增。`gateway_url` 在配置了 `network.entry` 时取入口 URL，否则在启用 `gateway` 时取 `public_url`。同步循环每 5 秒检查本机地址，变化后重签并唤醒同步；成员之间经 `rpc` 的 `addresses` 交换记录，最多保留 256 条。浏览器经 `entrances` 读取，用 WASM 验签，丢弃非成员、已撤销、其他网络和带路径或凭证的 `gateway_url`。浏览器为每台 Node 保存已接受记录的 `seq` 高水位，撤回地址的记录也保存，刷新后不接受更旧的记录。HTTPS 且主机不是回环、私网或单段名称的入口算公网，二维码只用公网入口。

### 附件

接收方在 `vibeface/blob/1` 上发送 `{ network_id, hash, size, offset }`。来源核对请求方是成员 Node、该哈希被本机某个事件引用且大小一致，然后从 `offset` 起按 64 KiB 发送。接收方写入 `blobs/partial/<hash>.part`，每 1 MiB `fsync` 并在 SQLite 中提交偏移，断线或重启后从已提交位置继续。收齐后核对大小和 SHA256 再原子改名。一次只下载一个附件，暂存最多 256 MiB、64 个对象。

### 节点间 RPC

`vibeface/rpc/1` 的请求为 `{ type, network_id, ... }`：

| type | 用途 |
|---|---|
| `capabilities` | 角色、版本和管理 Agent 是否可用 |
| `addresses` | 交换签名地址记录 |
| `deliver` | 入口 outbox 向负责人投递消息或 job |
| `manager_status` | 查询某个触发在负责人上的状态 |
| `progress_read` | 读取管理轮和原生工作的临时进度 |
| `dispatch_work` | 派发方请求目标接纳工作 |
| `interrupt_work` | 停止目标上的工作 |
| `approval_forms`、`answer_approval` | 读取审批表单，转发带本人签名的回答 |
| `cancel_manager`、`reconcile`、`resolve` | 转发本人的管理控制，要求 `human: true` |
| `pending_joins`、`approve_join` | 汇总其他入口上的加入请求，把批准转发到保存请求的 Node |

### Relay 与公共入口

启用 `relay_listen` 的 Node 运行 `iroh-relay` 服务。个人网络里的 relay 只接纳当前成员 Node，以及带有效邀请令牌 `SHA256("vibeface:relay:v1" ‖ invite_hash)` 的加入者，所以只能经 relay 连通的新 Node 要用运行 relay 的 Node 发出的邀请。relay 看不到内容，但能看到哪些设备连接它、彼此何时通信和流量大小。

公共入口（`init --public-entry`）不加入个人网络。它的 relay 只接纳持有 `entry-token` 的设备，它不运行调度、执行和同步，不保存业务数据，本机设置路径回 404。成员 Node 用 `--entry URL --relay-token-file PATH` 指向它：把入口的 relay 加为自己的 relay 并带上令牌，地址记录的 `gateway_url` 写入口 URL，于是邀请链接和二维码打开的是入口的 origin。浏览器经入口的 `/api/client` 连到成员 Node，入口只搬运 Noise 密文。部署步骤见 [配置公共入口](node.md#配置公共入口)。

## 通知和自动后续

加密通道推送 `{ "type": "changed", "payload": ... }`。payload 区分历史变更、`manager_progress`、`native_progress`、`write_state` 与失效提醒。客户端读取 `notifications`，用稳定 `client_id` 确认 `notification_id`，不同浏览器各自确认。

管理进度包含 `conversation_id`、`trigger`、`invocation_id`、`source_node`、递增 `sequence` 和 `progress`，`progress.type` 为 `text_delta`、`text_snapshot`、`status` 或 `tool`。原生进度包含 `conversation_id`、`work_id`、`target_node`、`sequence` 和 `progress`。临时进度不写入事件日志。客户端读取 `conversation` 时，入口为该会话建立有界的只读观察，经 `progress_read` 从负责人和执行节点读取进度。

`message.report_results` 默认 `true`，保存在签名的用户消息中。为 `false` 时，该消息引出的工作结束后不再入队汇报。

`set_autonomy` 仅本人可用，写入 `AutonomySet { mode, version }`。`off` 停止自主后续，`report` 让结果汇报轮只读，`continue` 允许汇报轮继续派发工作。未设置时等同版本 `0` 的 `report`。`expected_version` 必须等于当前 `autonomy.version`。

`daily_decision_limit` 只限制每个 Node、每个个人网络的 UTC 每日结果汇报轮，预算先预留，失败调用不保证退回，不是 token 或账单上限。
