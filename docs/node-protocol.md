# Node 协议参考

协议版本为 `1`，数据格式 `data_epoch=3`，数据库 `user_version=3`。共享结构以 `crates/node/src/model.rs` 为准。本文描述本机配置、HTTP 与浏览器通道、业务 RPC、调度、工作、事件 DAG、写入保护和 Node 之间的 iroh 传输。项目尚未发布正式版，不同 epoch 的数据目录和程序不能混用。

## 身份与数据范围

`network_id` 是个人网络根 Ed25519 公钥，`device_id` 是设备 Ed25519 公钥本身，均编码为 64 个小写十六进制字符。Node 的 `device_id` 同时是它的 iroh `EndpointId`。地址不参与身份计算。

个人网络使用 `NetworkState { genesis, checkpoints, events }`，保留所有根 checkpoint 和各 epoch 的管理事件，两份状态的合并是并集。根身份签署初始 `Genesis`，其中包含根公钥及初始 Node、客户端成员。每个 `RootCheckpoint` 带下一把恢复密钥 `recovery_key`，由上一把签名，第一把是根密钥。恢复私钥只在加密恢复文件里，Node 不持有。

`Member` 包含 `device_id`、`kind`、`name` 和 `manage`，没有角色。`kind` 为 `node` 或 `client`。任何管理设备都能授予或撤销成员，并可把 `manage` 授予新的客户端成员；Node 成员不能持有管理权限。Node 只有两种：成员 Node 承担网页、同步、调度和执行，`network.relay_listen` 在 `forward` 模式决定是否运行 relay；`public_entry` 是不入网的公共入口，也可选择只发现而不转发业务的模式。

管理事件含 `version`、`network_id`、`epoch`、`parents`、`issuer`、`action` 和 `signature`。授权动作为 `grant`，撤销动作为 `revoke`，撤销带 `cutoff_seq` 和 `cutoff_hash`，即发起撤销时所连 Node 上该设备主分支的头，设备没写过时为 `0` 和 `null`。被撤销设备只有不超过这个上限的事件计入投影，超出的仍可入库但不计入。并发的授予与撤销冲突时撤销优先。根恢复由当前恢复密钥签名、epoch 加一的 checkpoint 建立新授权并换恢复密钥；`directory` 为空的 checkpoint 只换恢复密钥，目录沿用上一 epoch，立即生效；checkpoint 的 `retired` 列出没有保留的设备及其最终上限，旧 epoch 的事件不能放宽当前上限。

换目录的 checkpoint 带签名时间 `issued_at_ms` 和 `heads`（签名时恢复密钥所在 epoch 的管理事件 heads，rotation 为空），从这时起等 48 小时（`RECOVERY_DELAY_MS`）才生效，之前视图仍是原目录，`pending` 给出它的哈希、`effective_at_ms` 和待生效成员。等待期内，原目录里的管理设备可以在原 epoch 签 `veto`：`{"type":"veto","checkpoint":<checkpoint 哈希>,"at_ms":<签名时间>}`。否决算不算数只看它自己和它的因果祖先，之后到达的事件改变不了：因果祖先须包含该 checkpoint 的全部 `heads`，签发者在这些祖先里是管理设备且未被撤销（否则整份状态无效），`at_ms` 早于生效时刻；目标不符或祖先不全的否决不起作用。所以改写 `parents` 绕不过找回前的撤销，而先否决、后被撤销的设备，否决仍然有效。`heads` 含本机没有的事件的 checkpoint 不生效，`recover` 直接拒绝（`this recovery rests on device changes this Node has not received; sync it first`），同步合并时它保持等待，补齐事件后再按签名时间判定。同一把恢复密钥在找回生效前签出的 rotation（`issued_at_ms` 早于该找回的生效时刻）也会让它作废，换恢复密码不受等待中的找回阻塞。被否决或被 rotation 取代的 checkpoint 及建在它上面的 checkpoint 作废，哈希列入视图的 `stopped`，原目录继续有效。同一把恢复密钥只能出现一次；一把密钥再签时，此前签的每个找回要么有合法否决，要么被这次 rotation 取代，否则整份状态无效。最多 64 个未被否决的 checkpoint。生效不是终局：生效目录是签名记录和墙钟的纯函数，Node 不拒绝任何合法历史。窗口内签的否决（或倒签到窗口内的 rotation）晚到时，已按墙钟翻转的 Node 回到旧目录，与其他 Node 收敛；回滚前，它可能短暂按新目录放行。所以翻转时不往共享日志写不可撤回的记录：找回移出的 Node 上未完成的工作不记为放弃，它超出上限的写入只是在投影里不计入，回滚后重新计入；只有管理设备签名的 `revoke` 才把被移除设备的工作记为放弃。作废邀请、`identity_meta.recovery_since`（首次见到某把密钥生效的时间，只用于展示）和地址簿清理都是本机可重做的。认为对方已被移除的 Node 仍接受对方推来的成员状态，所以被找回移出、又因否决回到目录的 Node 也能收敛。未被移除的被偷管理设备可以随时拦下找回（含补签迟到否决），这个限制早已存在，补救是先移除它再找回。曾考虑过的方案：生效即终局会让休眠错过窗口的 Node 拒绝否决、永久分叉；成员时钟会让丢失的 Node 永远拦住找回，且两台都过期的 Node 互相交换后仍会分叉；否决按 epoch 最终投影计数会让事后撤销否决人立即复活找回。视图的 `epoch` 是生效的 epoch。`attestation` 的签名设备须在任一 epoch 做过客户端。

一个数据目录只属于一个个人网络，一个 Node 进程只服务一个网络。成员关系、项目和会话不发布到公共目录。地址默认只在个人网络内传播；`network.discovery` 是明确选择公开临时传输地址的配置。

## 本机配置

`config.json` 由 `init` 生成。网页通过加密客户端通道修改 `manager`、`roots`、`providers` 和 `codex_bin` 后，新操作立即读取；其他字段手动修改后重启生效。字段名固定，不接受未知字段。

| 字段 | 含义 |
|---|---|
| `listen` | HTTP 监听地址，默认 `127.0.0.1:4790`，决定可接入的网卡；iroh 默认在同一端口号上监听 UDP |
| `local_listen` | 本机会话的固定回环地址，默认 `127.0.0.1:4791`，让本机 origin 和浏览器存储跨重启不变 |
| `public_url` | 本 Node 的网页 origin，公网为 HTTPS；没有公共入口且网页开启时作为地址记录的 `gateway_url` |
| `allowed_origins` | 额外允许的浏览器 origin 数组，默认空；`init --allowed-origin` 可重复指定 |
| `allow_insecure_lan` | 明确允许可信 LAN 上的明文 HTTP |
| `local_auto_access` | 默认 `true`，通过独立回环入口和本机凭据建立受限本机会话 |
| `web_enabled` | 是否提供网页和浏览器访问，默认 `true`。启动时 `--no-web` 只关闭本次进程的浏览器访问 |
| `public_entry` | `true` 表示不加入个人网络。默认提供网页、relay 和密文转发；`discovery-only` 时仅提供发现服务和可选静态网页 |
| `roots` | 本机 Agent 和工作可选择的绝对项目目录 |
| `network` | iroh 传输、relay 和公共入口设置，见下表 |
| `manager` | Bot 配置，`provider` 是运行 Bot 的服务（`codex_sdk` 或 `cursor_sdk`），关闭时为 `null` |
| `providers.execution` | 默认 `false`，显式启用后由 Codex App Server 执行派发的工作；`false` 时不提供派发执行能力。Codex Bot 独立控制其所需的 App Server 连接 |
| `providers.codex_app_server` | `{mode, socket}`：`system` 要求明确 `socket`；`spawn` 时 Node 自行托管，要求 Provider 环境明确设置 `CODEX_HOME` |
| `codex_bin` | 托管 App Server 使用的 Codex CLI，默认 `codex` |
| `timeout_secs` | 单次 SDK 调用超时，默认 600 秒 |
| `daily_decision_limit` | 本 Node 每个网络的 UTC 每日非本人触发的管理调用次数，默认 20；本人主动聊天不受此额度限制 |

`network` 的字段：

| 字段 | `init` 参数 | 含义 |
|---|---|---|
| `bind` | 无 | iroh 的 UDP 地址；省略时在 `listen` 端口上绑定 IPv4 与 IPv6 通配地址，IPv6 失败不阻止启动 |
| `relays` | `--relay URL` | 个人网络中其他 Node 的 relay，最多 16 个 |
| `n0_relays` | 无 | 同时使用 n0 公共 relay，默认 `false` |
| `relay_listen` | `--relay-listen ADDR` | `forward` 模式的 TCP relay 地址，或 `discovery-only` 模式的 UDP 发现地址 |
| `relay_mode` | `--relay-mode MODE` | `forward` 默认，或 `discovery-only`；后者要求 `public_entry=true` |
| `discovery` | `--discovery URL` | 最多 4 个 `vibeface://<device-id>@<host>:<UDP-port>`，显式钉住发现服务身份 |
| `relay_url` | `--relay-url URL` | 其他 Node 访问本 relay 的 URL，明确提供其他 Node 可访问的 URL |
| `entry` | `--entry URL`、`--entry-relay URL` | 公共入口 `{ url, relay }`；`relay` 默认等于 `url`，即反向代理把 `/relay` 转给入口的 relay |

配置了 `network.bind` 时关闭 iroh 的端口映射，不发布未配置的路由器地址。`public_entry=true` 要求 `relay_listen`，且不能同时有 `entry`、`manager` 或 `roots`。`discovery-only` 还要求非零 UDP 端口，不能配置 `bind`、`relays`、`n0_relays`、`relay_url` 或其他发现服务。

启用管理 Agent 时，`manager` 对象包含以下全部字段。程序路径在后台运行时使用绝对路径。Cursor 模式要求非空 `model`，Codex 模式可用空字符串选择其默认模型。

```json
{
	"provider": "cursor_sdk",
	"workspace": "/absolute/manager-workspace",
	"model": "auto",
	"cursor_sdk_bin": "/absolute/bin/cursor-sdk-bridge"
}
```

`provider` 也可为 `codex_sdk`，此时 Bot 回合在 Codex App Server 上运行（`turn/start`，`clientUserMessageId` 为 attempt ID）。`manager_capabilities` 声明本机 SDK 能力：两种 Manager 均支持流式进度、取消请求和本机原生会话延续，只有 Codex 支持原生 steer，均不支持 Provider run 对账。`cancel_confirmation=true` 表示取消可被确认，Codex 以 App Server 回合结束为证据，Cursor 以 Provider 终态或受管进程退出为证据。配置存在、进程可达和真实模型可用是不同状态。

## 数据目录

| 文件 | 内容 |
|---|---|
| `identity.json` | 设备私钥，0600 |
| `config.json` | 本机配置，0600 |
| `data-epoch.json` | `{"data_epoch":3}`，程序拒绝其他 epoch |
| `node.sqlite3` | 成员目录、签名日志、投影和本机表，`user_version=3` |
| `bootstrap.json` | 建网前的一次性本机 capability，公共入口没有 |
| `entry-token` | 公共入口的运营者令牌，64 个十六进制字符，0600；公共入口和使用它的成员 Node 各有一份 |
| `blobs/sha256/<前两位>/<hash>` | 附件正文，`blobs/partial/` 为续传暂存 |
| `local-sessions.json` | 本机会话的 cookie 摘要 |
| `join.json` | `vibeface join` 进行中的请求，重试时沿用同一请求 |
| `codex-app-server.pid` | 托管 App Server 的进程组号 |
| `cursor-sdk-bridge.pid` | 常驻 Cursor SDK Bridge 的进程组号和程序路径，Node 启动时结束上次遗留且命令相符的进程组 |
| `node.log`、`node.log.1`…`node.log.4` | Node 日志，JSON lines，0600，8 MiB 轮转，共保留 5 个 |
| `service.*.log`、`update.*.log` | macOS 服务和更新器的标准输出与错误 |

运行中的 Node 用目录锁独占数据目录，并在私有维护 socket 上回答 `vibeface status`。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins`、本机 HTTP 别名，以及地址簿中未撤销成员 Node 的 `gateway_url` 组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口；只有 `/api/client` 接受跨源连接。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/assets/*` | GET | 内置网页客户端；`/assets/` 下为带哈希的不可变资源 |
| `/sw.js`、`/manifest.webmanifest`、`/icon.svg` | GET | 离线应用外壳和 PWA 清单，`no-cache` |
| `/generated/vibeface_secure.js`、`/generated/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | `device_id`、`version` 和 `public_entry` |
| `/api/local/setup` | POST | 本机网页读取一次性初始化邀请或已有网络的当前入口；公共入口回 404 |
| `/api/client` | GET，WebSocket upgrade | 浏览器网关 |
| `/sdk.v1.SdkCustomToolCallbackService/CallCustomTool` | POST | 只在独立的随机回环端口上提供，是协调工具的唯一入口；Cursor SDK 回调和 `vibeface tool` 都发 `{toolName, args, agentId}`，回 `{result}`；Cursor 回调带 Node 级令牌，按 `agentId` 找到所属管理轮 |

网站不提供用户名密码注册或明文业务 RPC。`/api/local/setup` 要求正文为 `{}`，实际 TCP 来源为 loopback，Host 为字面 localhost 或 loopback 地址，Origin 与该入口完整一致，且不带 `Forwarded`、`X-Forwarded-*` 或 `X-Real-IP`。未初始化时返回 `{ kind: "uninitialized", invitation: { url, device_id, capability } }`，已有网络时返回 `{ kind: "initialized", entry, removed, local_launch? }`，`removed` 表示这台电脑已被移出。该响应不授予成员资格。

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
| `info` | `{}` | 访客仅得到通用名称 `Node`、`device_id`、`version`、`initialized`、`public_entry` 和 `network_id: null`；有效成员或本机会话还可读取真实名称、`network_id` 和 `connection_entry` |
| `authenticate` | 可选 `network_id` | 验证成员授权或本机会话；不是成员但在找回目录里做管理设备的，回 `recovery: {status: "waiting", effective_at_ms}` 或 `{status: "stopped"}` |
| `directory` | `{}` | 当前签名目录及投影 |
| `entrances` | `{}` | 地址簿中的签名地址记录，浏览器自行验签 |
| `subscribe` | 可选 `vector` | 订阅 `changed` 推送；浏览器带已见的版本向量，入口落后时回 `behind: true` 并立即同步 |
| `rpc` | 下节业务对象 | 业务读写 |
| `write_state` | `{}` | 写入状态和本机记下的冲突，见 [写入保护](#写入保护) |
| `confirm_writable` | `{}` | 本人确认数据目录不是从备份恢复的，只读或对齐中的 Node 进入可写 |
| `diagnostics` | 可选 `since_ms`、`until_ms`，默认最近一天 | 脱敏诊断包，见 [排障](troubleshooting.md) |
| `log` | `records`：1 到 20 条 `{level, event, message, conversation_id?, work_id?, fields?}` | 浏览器写日志，来源记为 `web`，每条 JSON 不超过 8 KiB，每台设备每分钟 120 条 |
| `logs` | 可选 `since_ms`、`until_ms`、`level`、`sources`、`conversation_id`、`work_id`、`text`、`limit` | 读本机日志，返回 `records`，最多 20000 条 |
| `node_settings` | `{}` | 读取本机 `settings {roots, manager}`、`defaults`、`readiness {manager, missing}` 和 `providers`。`providers` 每项为 `{provider, roles, supports, state, issues, bin, app_server, env}`：`roles` 是它现在的用途（运行 Bot 为 `agent`，Codex 执行工作为 `execution`），`state` 为 `ready`、`needs_setup`、`starting` 或 `off`（没有用途），`env` 只说明变量是否已设置 |
| `configure_node` | `manager`、`roots` | 保存可用文件夹和当前 Bot 的模型、工作目录和程序路径；不能开关 Bot 或更换它的服务 |
| `configure_provider` | `provider`，可选 `bot`、`execution`、`bin`、`app_server`、`env` | 唯一选择用途的方法。`bot: true` 让这个服务运行 Bot，保留已有的 Bot 设置，没有时用 `defaults`；Cursor 模型为空时取 `auto`，换到 Codex 时 `auto` 改为默认。`bot: false` 只在 Bot 运行于这个服务时关闭它。`execution` 只用于 Codex。`bin` 和 `app_server` 只用于 Codex。`env` 中字符串设置变量，`null` 删除，值只写入本机私有文件 |
| `local_access` | 可选 `enabled` | 查询或开关本机自动访问 |
| `invite` | `record` | 管理设备登记签名邀请，返回 `join_id`、`entry_id`、`entry { relay_url, direct_addrs }` 和 `gateway_url` |
| `join` | `secret`、`public_key`、`kind`、`name`、`commit` | 新设备提交加入请求，`public_key` 必须是本通道设备 |
| `join_status` | 可选 `join_id`、`reveal` | 新设备查询或揭示随机数；不带 `join_id` 时管理设备列出所有入口上的待处理请求 |
| `approve` | `join_id` 加 `admin_nonce`、`event` 或 `reject: true` | 管理设备推进或拒绝加入，请求在其他入口时转发过去 |
| `revoke` | `event` | 管理设备签名撤销成员，`epoch` 须是生效 epoch |
| `veto` | `event` | 管理设备签名否决正在等待的找回：签发者是本通道设备，`epoch` 是生效 epoch，`at_ms` 与 Node 时间相差不超过 10 分钟，目标须是当前 `pending`，且离生效至少还有 10 分钟；同一事件重复提交返回原结果 |
| `recovery_state` | `expires_at_ms`、`signature`、`checkpoints`、`events` | 恢复密钥以绑定本通道设备的两分钟签名读取当前目录 `state` 和本机各作者主分支的头 `heads`；先合并恢复文件带来的 `checkpoints` 和 `events`；新 checkpoint 据此为没有保留的 Node 写 `retired`，并从 `state` 取签名 epoch 的管理事件 heads 写入 `heads` |
| `recover` | `genesis`、`checkpoints`、`events`、`capability` | `checkpoints` 是完整链，最后一个是新 checkpoint，`events` 是恢复文件里的管理事件。空 Node 凭 `capability` 安装整条链和全部事件；已建网 Node 只取指向所带 checkpoint 的 `veto`，其余事件丢弃，最后一个 checkpoint 本机已有时什么也不改，否则合并后安装它。新的换目录 checkpoint 的 `issued_at_ms` 须在 Node 时间 10 分钟内（太早回 `this recovery was signed too long ago; start it again`，太晚回时钟偏差）；本机没有的旧 checkpoint 已作废的直接放过，仍会生效的同样检查 |

未加入的设备只能调用 `info`、`authenticate`、`join`、`join_status`、`recovery_state` 和 `recover`，其他方法要求已验证的成员或本机会话。`recovery_state` 和 `recover` 是根恢复的专用准入：前者要求当前恢复密钥对 `{device_id, expires_at_ms, network_id}` 的签名，`device_id` 为本通道设备，有效期最长两分钟；后者在安装前验证新 `RootCheckpoint` 由当前恢复密钥签名且 epoch 加一，验证前没有副作用。签名来自已被替换的恢复密钥时两者都回 `this recovery key was replaced by a newer recovery`，有找回在等时回 `another recovery is waiting to take effect`，来自被否决的找回时回 `one of your devices stopped this recovery`。所有管理浏览器都丢失时，新浏览器凭恢复文件经这两个方法找回，48 小时后得到管理权限，等待期内它只是未加入的设备。`authenticate`、`directory` 和 `recovery_state` 的回复带 `now_ms`，回复里的视图按这个时间算出，浏览器用它复算核对，签否决时也用 Node 时间。到点或迟到的否决改变生效目录时，Node 重新安装一次签名状态，重算投影并作废邀请；定时任务每次最多睡 15 分钟，醒来按墙钟重算。每个请求都重新查成员目录；目录变化时，已订阅或正在观察进度的会话重新核对，设备被移除后会话关闭。`confirm_writable`、`diagnostics`、`logs`、`node_settings`、`configure_node` 和 `configure_provider` 另要求管理设备或本机会话，`invite`、`approve`、`revoke` 和 `veto` 要求管理设备。

加入使用承诺加短码。管理设备生成 ticket（入口 `EndpointAddr` 和一次性 secret），以 `https://<gateway>/join#vibeface1…` 链接或二维码交给新设备。新设备提交 `commit = SHA256(public_key ‖ joiner_nonce)`，管理设备随后给出 `admin_nonce`，新设备再揭示 `joiner_nonce`，揭示值与承诺不符时请求作废。两端用双方公钥和随机数各自算出 6 位短码，本人核对一致后管理设备签名 `grant`。加入请求的视图带 `commit`。管理设备在发出 `admin_nonce` 前记下所见请求的设备公钥、`invite_id` 和 `commit`，揭示后自己核对承诺；新设备在揭示前记下 `network_id`、`admin_id` 和 `admin_nonce`。短码只用这些自己固定的值计算，入口之后改动任何一项，管理端不显示短码，新设备停止加入。ticket 默认 10 分钟过期，只能被一个加入请求消耗。

浏览器代本人写入时，`MessagePosted(user)`、`BotCreated`、网页删除的 `BotDeleted`、`ConversationOwner`、本人结清的 `WorkFinished` 和 `NotificationAcked` 的 body 带 `attestation { device_id, signature }`；回答提问的 `MessagePosted(user, answers)` 签名可选，Grok 经 decision token 的回答由 Node 无签名写入。签名输入为 `vibeface:message:v1:attestation:` 加 `SHA256(canonical_json({network_id, scope_id, body}))` 的十六进制，body 去掉 `attestation`，所以签名绑定网络、会话、事件类型和业务 ID。签名设备必须是未撤销的客户端成员。除回答外，Node 和协调 Agent 不能代写这些事件。

恢复包外层为 `{ version, network_id, envelope }`，加密正文为 `{ version, network_id, root_secret_key, genesis, checkpoints }`，`root_secret_key` 是当前恢复私钥，使用 Argon2id 与 XChaCha20-Poly1305。口令和明文恢复私钥不发给 Node。根恢复默认保留现有管理设备。根恢复和换恢复密码都先下载新恢复文件再提交，完成后旧文件失效。业务正文和项目文件不在恢复包中。

协调工具入口只接受本机 SDK 调用期间签发的短期 bearer token，权限固定到一个个人网络和业务会话，不能添加或撤销设备或修改后续授权。

## RPC 方法

`rpc` 的 `params` 是 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。已验证的客户端确定 `network_id`。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | 无 | 成员 Node、角色、在线状态、`max_seq`、`head_hash` 和能力，已撤销的 Node 给出撤销上限；同步正在失败的 Node 直接记为离线，不等待探测 |
| `bots` | 无 | 当前网络的机器人定义，名称和说明已合并聊天中的修改；`primary` 为主 Bot 的会话 ID |
| `set_primary_bot` | `conversation_id`、`attestation` | 本人选定主 Bot，写入 `bots` scope |
| `create_bot` | `bot_id`、`conversation_id`、`name`、`instructions`、可选 `project`、`attestation` | 创建机器人和它的会话，完整请求精确去重 |
| `control_schedule` | `conversation_id`、`schedule_id`、`action`（`pause`、`resume` 或 `delete`）、`attestation` | 本人暂停、恢复或删除一个 schedule，写 `ScheduleControlled`；已是该状态时不写，返回 `{ written }` |
| `conversations` | 无 | 本地已知会话 |
| `create_conversation` | `conversation_id`、`title`、首个负责人 | 以稳定 ID 创建会话 |
| `conversation` | `conversation_id` | 会话（含负责人与 `owner_epoch`）、机器人、消息、`works`（DAG 投影，含 `state`）、`deliveries`（本机还在转交的派发和停止请求）、`changes`、`bot_settings`、`sent`（本 bot 发给其他 bot 的消息 `{message_id, to, text, created_at_ms}`，`to` 是对方会话）、`checks`、`questions`（本会话的提问及回答）、`sessions`（归给本 Bot 的外来会话，含 `muted`）、`local_node`、调度队列和 `observation_targets`（本次观察进度的设备）。负责人是其他设备时，入口限时向它读取 `scheduler_jobs`，带 `source_node` 合并进调度队列，只用于显示、不写本地；`scheduler.remote` 为 `{owner, reachable}`，联系不上时 `reachable: false` |
| `message` | `conversation_id`、`message_id`、`text`、可选 `attachments`、`report_results`、`attestation` | 保存本人消息并返回调度回执 |
| `set_owner` | `conversation_id`、`scheduler_node`、`epoch`、`attestation` | 换会话负责人，`epoch` 必须等于当前 `owner_epoch + 1` |
| `cancel_manager` | `conversation_id`、`trigger` | 请求停止指定管理轮，不停止已派发的工作；排队中的回合记为已停止并写一条 `stopped` 回复。本机不持有该回合时转给会话负责人，超时返回 `{cancel_requested: false, unconfirmed: true, owner}`，不重发，客户端重读会话对账 |
| `reply_unanswered` | `conversation_id`、`message_ids` | 本机作为负责人，为其中仍未回复的消息排队回复，返回 `{receipts}`；重试不会再排 |
| `dismiss_unanswered` | `conversation_id`、`message_ids` | 本机作为负责人，把其中仍未回复的消息记为开始前已取消，不再列出也不执行，返回 `{receipts}`；重试不会再记 |
| `interrupt_work` | `conversation_id`、`work_id` | 中断工作及它委派出的工作；目标在其他 Node 时由入口转发和重试 |
| `codex` | `method`、可选 `params`、`node` | 调用 App Server 名单里非 `hidden` 的方法，结果经 `sanitize`；`node` 为其他设备时经成员连接转发 |
| `codex_state` | 可选 `node` | App Server 状态摘要 `{connection, threads: [{thread_id, name, status, cwd, plan, reasoning, token_usage}], rate_limits}`；网页不调用 App Server 方法 |
| `questions` | 无 | 全网未回答的提问：`{questions: [{question_id, severity, text, choices: [{label}], about, conversation_id, bot, author, created_at_ms, answer}]}` |
| `answer` | `question_id`、`text`、`attestation` | 写该提问唯一的回答消息 `a-<question_id>`，签名作用域是提问所在会话。会话里的回答按 `answer` 触发准入，或转交调度 Node。返回 `{status, question_id, message, receipt}`；已有不同回答时 `status = "already_answered"`，返回已有回答 |
| `resolve` | `conversation_id`、`check_id`、工作项另带 `outcome` 与可选备注 | 本人一键结清：管理进程确认已停止；工作项写一条带本人签名的 `WorkFinished`。不重做原触发 |
| `upload_attachment` | `conversation_id`、`hash`、`size`、`offset`、`data` | 按 1 MiB 分块上传附件，返回已落盘的偏移 |
| `read_artifact` | `conversation_id`、`blob_hash`、可选 `offset`、`limit`、`binary` | 分页读取该会话实际引用的附件 |
| `notifications` | `client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id`、`attestation` | 写入 `NotificationAcked` |

`bot_id`、`message_id`、`conversation_id` 和 `call_ref` 标识业务写入。重试保留原身份和内容，同 ID 换内容会被拒绝。协调 Agent 的 token 只能调用 `nodes`。

`read_artifact` 默认按 UTF-8 文本分页，返回 `{ blob_hash, text, offset, next_offset, size }`，`limit` 为 4 至 65536 字节，偏移必须位于字符边界。`binary: true` 时每页 512 KiB，只读取请求的区间，返回 `{ blob_hash, offset, size, next_offset, data }`，`data` 为 base64。`next_offset` 为 `null` 时结束。浏览器拼好全部字节后用 SHA-256 与 `blob_hash` 比对，一致才保存文件。附件随引用它的消息由同步按哈希拉到其他成员 Node，还没拉到的入口读取失败，稍后再试。

`changes` 按 rank 列出本会话的 bot 修改，每项为 `{ change_id, conversation_id, cause, event_hash, created_at_ms, change }`，`conversation_id` 是被修改的 bot 的会话，`cause` 为 `{ conversation_id, message_id }` 或 `null`（bot 自己定的提醒），`change` 即 `BotChanged.change`。`bot_settings` 为 `{ skills: [{ skill_id, name, description }], schedules: [{ schedule_id, name, prompt, first_run_at_ms, every_minutes?, enabled, depth?, next_run_at_ms }], memory: [{ memory_id, text }] }`，skill 是全网共用的库，schedules 只列本会话还会运行或已暂停的 schedule，暂停时 `next_run_at_ms` 为 `null`，没有 `every_minutes` 的是一次性提醒，`depth` 省略即 0。`messages` 中 `role=bot` 的是另一个 bot 发来的消息，带 `sender { conversation_id, name }`。

`works` 的每项包含 `work_id`、`conversation_id`、`target_node`、`trigger`、`dispatcher`、`kind`、`cwd`、`title`、`prompt`、`phase`、`summary`、`thread_id`、`turn_id`、`outcome`、`result_text`、`failure_reason` 和本机状态 `state`。网页以 `title` 为任务名，`prompt` 放在详细信息里。

## 可靠调度回执

调度 job 以 `(conversation_id, trigger)` 为键，`trigger` 为 `message:<message_id>`、`work:<work_id>`、`task:<work_id>`、`schedule:<schedule_id>:<slot_ms>` 或 `bot_message:<message_id>`，重试和同步都不会产生第二个 job。`message` 返回 `accepted`、`state`、`trigger`、`message_id` 和 `receipt`：

```json
{
	"network_id": "NETWORK_ID",
	"conversation_id": "conversation-id",
	"trigger": "message:message-id",
	"state": "queued"
}
```

会话负责人由 `ConversationOwner` 决定。入口不是负责人时，消息写入入口自己的日志并留在本机 outbox，回执 `state=pending_delivery`，入口每 3 秒经 `vibeface/rpc/1` 的 `deliver` 投递一次，负责人按触发事件幂等接纳。投递时才解析负责人，所以换负责人后旧负责人尚未开始的 job 交给新负责人。浏览器收到入口回执即删除自己的 outbox 项。

回合以 `owner_epoch` 为 fence。领取 job 时记下当时的 `owner_epoch`，之后工具调用的权限检查要求本机仍是负责人且 epoch 未变，换走再换回也算失效。旧负责人收到更高 epoch 的 `conversation_owner` 后立即拒绝该回合的后续工具写入，并走停止回复的取消路径结束它：取消得到确认时回复状态为已停止，未确认时记为中断并留待核对，不重做也不交给新负责人重放。Agent 任务在工作所在设备运行，不受负责人 fence。

新负责人从共享记录推导旧负责人留下的未回复消息，列在 `conversation` 调度队列的 `unanswered` 里，按会话顺序给出。未回复指本会话的本人消息没有 `status = completed` 的回复，没有 `message_steered` 记录，本机也没有它的 job 或 outbox 条目。只有本机是负责人且 bot 未删除时才列出。网页显示“N 条消息没等到回复”，本人选择回复后经 `reply_unanswered` 在本机排队，不会自动重放；选择“不回复”则经 `dismiss_unanswered` 在本机把它们记为开始前已取消，不执行任何操作。这条记录和队列一样只在本机：会话再换到第三台设备时，那里会重新列出，仍需本人决定。排队按 trigger 去重，重试不会再排，结果未知时重读会话即可对账。回复 ID 由 `(会话, trigger, owner_epoch)` 派生，所以新负责人的回复不会和旧负责人已写的停止或中断回复冲突。

每个会话的本机管理队列最多 256 个待处理触发，本人消息优先于工作结果、schedule 和 bot 消息。每轮按领取时固定的版本向量 `basis` 准备上下文。排在后面的消息因此能读到前一条的回复。提示总长不超过 50000 字节，bot 指令和 schedule prompt 各以 8 KB 封顶并计入总长。Codex 线程在 `thread/start` 和 `thread/resume` 的 `developerInstructions` 里收到 CLI 说明、全部工具目录和固定规则，回合消息只带本轮角色和数据：续接的线程只收到新消息、变化过的工作（`works_unchanged` 计其余条数）和变化过的 bot（未变时为 `{"unchanged":true}`），`trigger.unavailable_tools` 列出本轮不提供的工具。线程看过的 bot 和工作摘要随 `covers` 存在本机会话里。会话键含线程指令的摘要，指令变化就开新线程；模型不在 Codex 会话键里，按回合经 `turn/start` 传。Cursor 每轮提示仍带全部规则和上下文，模型变化开新会话。超出时依次收缩工作列表、检查点和较早的消息，不让这一轮失败。工作列表、记忆和消息都从新到旧取到各自预算为止，再按时间顺序给出，并附省略条数。

负责人每 30 秒检查本机负责的会话中已启用的 schedule，只取不晚于当前时间的最近一个时刻 `slot_ms`，错过的不补跑。运行前负责人写一条 `schedule_run_claimed` 领取这个时刻；时刻已过去超过 10 分钟（设备关机或休眠）时同样写领取，但不运行，只记一条 `schedule.skipped_late` 日志。任何 Node 已领取的时刻都不再排队，所以重启、同步和换负责人都不会重跑或补跑，一次性提醒也只运行一次。同一 schedule 仍在排队时不领取下一个时刻。早于最近一次修改 5 分钟以上的时刻不运行，所以暂停后恢复不会补跑。schedule 轮可以派发工作和设提醒，不提供修改 bot 的工具，回复的 `in_reply_to` 为该 trigger，由它派发的工作结果也回复这个 trigger。领取前 schedule 已暂停或删除，或这个时刻的领取胜者不是本机时，job 取消。换负责人时未开始的 schedule job 直接丢弃，由新负责人按自己的检查运行。

`bot_message:<message_id>` 由写入 `BotMessaged` 的发送方负责人接纳（对方会话也归它调度时），否则进 outbox，经 `deliver` 交给对方负责人。同步只存消息，不排 job。每个轮次有深度：本人消息和回答为 0，schedule 轮取其 `depth`，bot 消息轮取 `hops`，工作结果和 Agent 任务沿派发链继承；提醒和 bot 消息的深度为当前加一，超过 4 时拒绝。

job 状态为 `queued`、`steering`、`claimed`、`running`、`cancelling`、`completed`、`failed`、`cancelled`、`interrupted` 或 `steered`，后五种是终态。Codex 回复进行中时，同一会话排队的用户消息（无附件、需要回复）先在本机记为 `steering`，再以消息 ID 为 `clientUserMessageId` 发 `turn/steer` 并入当前回合，发送前核对目标是本进程启动的回复且未被停止或移走；App Server 明确拒绝时回到 `queued`，其余（含超时）保持 `steering`。目标回复结束后才按线程回读结算：消息所在回合 `completed` 即 `steered`（`outcome=answered`），在其他终态回合即 `steered`（`outcome=unfinished`，随宿主回复的终态），线程里没有它且目标回合已结束即回到 `queued`，绝不重发；消息只出现在其他回合也算 `unfinished`。只有回读成功且读到目标回合才结算，读取失败或读不到目标回合保持 `steering`，连续 10 分钟读不到（中间读到一次即重新计时）则记为 `steered`（`outcome=unknown`，网页显示“不确定是否已并入”）。未定期间本机仍拥有的会话不领取后续消息，会话已移走时排队消息照常交给新所有者；停止 `steering` 消息即停止它并入的回复。steer 记在本机调度状态，同步和恢复不会重放；结算为 `answered` 时在同一事务写一条 `message_steered`，让以后的负责人不把这条消息算作未回复。Cursor 始终排队。跨重启的 `running` 与 `cancelling` 进入 `interrupted` 并登记待核对，不自动重新调用 SDK。Bot 回答 `MessagePosted(assistant)` 与 job 终态在同一事务提交，`status` 为 `completed`、`failed`、`stopped` 或 `interrupted`，停止或中断时保留已输出的部分文字。

```text
queued → claimed → running ─→ completed | failed
             │        └─ cancelling ─→ cancelled（SDK 确认受管进程已退出）
             │                     └─→ interrupted（进程退出未确认，登记待核对）
             └ 重启时 running / cancelling ─→ interrupted（登记待核对）
```

Node 每隔几秒用 signal 0 检查待核对记下的进程组，Codex 回合则按记下的线程和回合 ID 复查 App Server，仍在运行就中断。都已结束时自动结清，相应的 agent 任务记为中断；仍有进程组存活时，`conversation.checks` 才列出这条，等本人一键确认。结果未知的工作由 `works[].state == "unverified"` 表示，卡片 ID 为 `work:<work_id>`。同一工作只显示一条：`kind=task` 的工作结果未知时，目标上那一轮管理进程的待核对不再单独显示，结清工作项时一并结清。模型不能结清待核对。

负责人每轮结束后，会话消息超过 20000 字节时写一条 `ContextCheckpoint`。写检查点时只读取最新检查点之后、它的 `covers` 没有覆盖的事件，结果与读取整个会话相同。检查点是确定性摘录，不是模型写的总结。`recent_user_requests` 是被覆盖的最近 20 条本人消息摘录，只是历史，不代表需要长期遵守的指令。

本机调度 job、outbox、工作状态日志和原生 SDK 会话不进入事件日志。

## 工作

每台设备只有一个协调角色：协调 Agent 加 Node 运行时。协调 Agent 的 Shell 完全访问本机，用 `vibeface codex call|send|watch` 直接经 `VIBEFACE_CODEX_SOCKET` 调用本机 App Server。需要 Node 记录和观察的工作用协调工具 `dispatch_work { title, prompt, cwd, call_ref, target_node?, parent?, continues?, thread?, model?, effort?, images? }` 派发。`title` 是给本人看的几个字，最长 120 字符。`work_id` 由 `(会话, trigger, call_ref)` 派生，同一轮重试同一 `call_ref` 返回原工作；结果不明时用 `call_lookup` 查询。`hand_over_session { node, session_id, version, to? | stop_following? }` 把本 Bot 名下的外来会话转给 `to`（另一个 Bot 的会话 ID 或名称，给自己即重新关注），或 `stop_following: true` 不再关注；`version` 取自 `bot.sessions`，是最后改动该会话归属的事件 hash 前 16 位。只有会话当前所属的 Bot 能操作，运行该会话的设备写入；状态已成立时不写、重试只读回；版本已变时不写，错误里给出当前版本。返回 `{node, session_id, conversation_id, muted, version, changed}`。其他协调工具为 `nodes`、`context`、`work_status`、`interrupt_work`、`read_skill { skill_id }`、`recall { query? }`、`fetch_url { url }`、`list_bots {}`、`read_bot { bot }`，以及 `dispatch_work` 内部使用的 `track_work` 和 `bind_work`。`fetch_url` 只读 http 和 https，解析出的每个地址都必须是公网地址，连接固定到核对过的地址，不走代理，最多 5 次跳转并逐跳核对，15 秒超时，正文最多 2 MiB，返回 `{ status, final_url, content_type, title, text, truncated }`，文字最多 20000 字符。

唤醒工具 `remind { call_ref, note, in_minutes? | at_ms? }` 和 `message_bot { call_ref, bot, text }` 提供给除 Agent 任务和路由外的所有轮次。`remind` 写一个一次性 schedule（深度为当前加一，1 分钟至 366 天后）；`message_bot` 在对方会话写 `BotMessaged`，`message_id` 由 `(会话, trigger, call_ref)` 派生，每个 bot 每小时最多 10 条，正文最多 4000 字节。

修改 bot 的工具为 `update_profile { call_ref, name?, instructions? }`、`save_skill { call_ref, skill_id?, name, description, body }`、`remove_skill { call_ref, skill_id }`、`save_schedule { call_ref, schedule_id?, name, prompt, first_run_at_ms, every_minutes?, enabled? }`、`remove_schedule { call_ref, schedule_id }`、`remember { call_ref, memory_id?, text }`、`forget { call_ref, memory_id }`、`create_bot { call_ref, name, instructions?, project_node?, project_cwd? }`、`configure_bot { call_ref, bot, name?, instructions?, model?, skills?, all_skills? }` 和 `delete_bot { bot }`，只提供给本人消息触发的轮次。Node 再核对 trigger 是 `message:<id>` 且该消息是本会话的本人消息，`cause` 取自 trigger，`change_id` 由 `(会话, trigger, call_ref)` 派生，同一 `call_ref` 同内容返回原记录，换内容拒绝，`call_lookup` 也能查到修改。改前值取自投影，没有变化的修改被拒绝。参数不接受其他字段，AI 服务和文件夹只能在设置里改。`model` 为空表示跟随设备默认；`skills` 是启用的 skill ID 集合，必须都在库里，`all_skills: true` 恢复全部，两者不能同时给。修改名称和说明需要会话有机器人；schedule 只能由创建它的会话修改。每个 bot 最多 20 个活动 schedule、50 条笔记，笔记每条最多 500 字符，像密钥的内容被拒绝。一条本人消息最多建 3 个 bot。`delete_bot` 在被删 bot 的会话写 `BotDeleted { cause }`，不能删除发起轮次的 bot 自己，重复删除不写新事件；网页由本人签名写 `BotDeleted { attestation }`，RPC 为 `delete_bot { conversation_id, attestation }`，返回 `{ written, primary }`。

Node 接纳工作时核对派发方是成员 Node 且处于进行中的调度轮次，并核对 `cwd` 位于自己的 `roots` 内。本机工作（`kind=codex`）：`track_work` 写 `WorkDispatched` 并返回 `{work_id, call_ref, state, may_send, send_by_ms, cwd, thread_id?}`，只有第一次登记 `may_send=true`；CLI 用 `thread/start`（`approvalPolicy=never`、`danger-full-access`）开线程，`bind_work {work_id, thread_id}` 绑定后才发 `turn/start`，`clientUserMessageId` 为 `work_id`，再 `bind_work {work_id, turn_id | unknown | failed}`。`send_by_ms` 为登记后 60 秒，之后不再发送；再过 30 秒仍没有回合时 Node 按 `clientId` 复查线程，找不到就记为 `rejected`。`target_node` 是其他设备时，任务（`kind=task`）作为那台设备协调 Agent 的一轮执行，由它在本机直连派发。`continues` 在原工作的 thread 上开新 turn，原目标不可达时报错，不改派。`thread` 是本机已有的 App Server 线程（`vibeface codex send`）：CLI 先 `thread/read` 取 `cwd`，Node 登记时即为 `starting`，不核对 `roots`，与 `continues` 互斥；发送时回合进行中则 `turn/steer`，否则 `turn/start`。几项工作可共用一个回合，各自结束并各自回报。

Codex 默认 bypass。Node 订阅的线程上的服务端请求和不属于线程的请求由 Node 挂起并交给协调 Agent：Agent 用 `vibeface codex requests` 查看、`vibeface codex respond <request_id> '<result-json>'` 回答，需要本人时用 `vibeface triage raise --request-id … --question … --choice <标签>` 在负责该会话的 Bot 聊天里提问，提问挂住请求直到有回答，回答后 Bot 有 120 秒用 `vibeface codex respond … --node <设备>` 应答。不属于任何 Bot 的会话先归给主 Bot 再按归属处理；没有 Bot、不属于线程或后台线程（`codex exec`、子代理）上的请求立即兜底。没有协调 Agent 或 120 秒内无人处理时按方法拒绝并记录（见 `docs/codex-app-server.md`）。其他客户端的请求留给原客户端。Node 不自动重试写请求。

工作状态为 `accepted`、`starting`、`running`、`interrupting`、`finished` 和 `unverified`。每次转换在同一事务更新本机日志并追加对应事件。`WorkFinished.outcome` 为 `completed`、`failed`、`interrupted`、`rejected`（从未启动）或 `unverified`；只有 `unverified` 可以被本人签名的 `WorkFinished` 取代。工作结束时若原消息的 `report_results` 不为 `false`，负责人把 `work:<work_id>` 入队，下一轮向本人汇报。

`providers.codex_app_server.mode` 为 `spawn` 时 Node 托管 App Server：执行 `codex app-server --listen unix://<socket>`，socket 位于 0700 的 `/tmp/vibeface-<identity>/` 目录，App Server 放入独立进程组，进程组号写入 `codex-app-server.pid`。启动时先结束上次遗留且命令相符的进程组，退出时 SIGTERM，3 秒后 SIGKILL，异常退出按 1 至 30 秒退避重启。App Server 每次重连 epoch 加一。重连后对未结束的工作调用 `thread/read` 只读复查，读不到原 turn 时进入 `unverified`。同一用户的本机进程可以连接这个 socket，协调 Agent 经 `VIBEFACE_CODEX_SOCKET` 得到它。

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
| `conversation_owner` | `scheduler_node`、`epoch`、`attestation?`、`cause?` | 入口 Node；bot 建 bot 时由负责人写，带 `cause` 不带签名 |
| `bot_created` | `bot_id`、`name`、`instructions`、`project?`、`attestation?`、`cause?` | 同上 |
| `bot_messaged` | `message_id`、`from { conversation_id, name }`、`text`、`hops` | 发送方会话的负责人，写在对方会话 |
| `schedule_controlled` | `schedule_id`、`action`、`attestation` | 入口 Node |
| `message_steered` | `message_id`、`reply` | steer 这条本人消息的 Node，在宿主回复 `reply` 完成且消息并入其中时写 |
| `message_posted` | `message_id`、`role`、`text`、`attachments`、`report_results?`、`in_reply_to?`、`status?`、`answers?`、`attestation?` | 本人消息由入口写，回答由负责人写；`answers` 只在本人消息上，`message_id` 为 `a-<question_id>` |
| `work_dispatched` | `work_id`、`trigger`、`dispatcher`、`call_ref`、`kind`（`codex` 或 `task`）、`cwd`、`title`、`prompt`、`continues?`、`parent?`、`thread?` | 目标 Node |
| `work_progress` | `work_id`、`phase`、`summary`、`thread_id?`、`turn_id?` | 目标 Node |
| `work_finished` | `work_id`、`outcome`、`result_text`、`failure_reason?`、`attestation?` | 目标 Node，本人结清时由入口写 |
| `notification_acked` | `notification_id`、`client_id`、`attestation` | 入口 Node |
| `question_asked` | `question_id`、`severity`、`text`、`choices: [{label}]`、`about {node, agent, session_id, cwd, summary, request_id?}` | 提问的 Node；写在拥有该会话的 Bot 会话 |
| `session_attributed` | `node`、`agent`、`session_id`、`cwd`、`summary` | 运行该会话的 Node，写在接手 Bot 的会话；rank 最大者为准 |
| `session_muted` | `node`、`session_id` | 运行该会话的 Node，写在当前接手 Bot 的会话：不再关注，新的 `session_attributed` 撤销 |
| `bot_changed` | `change_id`、`cause`、`change` | 负责人 |
| `context_checkpoint` | `covers`、`summary`、`recent_user_requests`、`open_work`、`decisions` | 负责人 |
| `schedule_run_claimed` | `schedule_id`、`slot_ms`、`owner_epoch` | 负责人，写在 schedule 所在会话 |

`bot_changed.change` 按 `kind` 区分：`profile { name?, instructions? }` 的每项为 `{ before, after }`；`skill { skill_id, before, after }`、`schedule { schedule_id, before, after }` 与 `memory { memory_id, before, after }` 的 `after` 为 `null` 表示删除。skill 为 `{ name, description, body }`，名称最多 64 字符，说明最多 512 字节，正文最多 16 KiB；schedule 为 `{ name, prompt, first_run_at_ms, every_minutes?, enabled, depth? }`，名称最多 120 字符，说明最多 8 KiB，`every_minutes` 为 5 至 40320，省略时只运行一次。说明（instructions）最多 8192 字节。这个事件不带 `attestation`。`cause` 为 `{ conversation_id, message_id }`，入库时要求 `prev` 或 `deps` 中有该会话、该 `message_id` 且带 `attestation` 的本人消息，否则拒绝；带 `cause` 的 `conversation_owner` 与 `bot_created` 按同一规则核对，`conversation_owner` 还必须是 epoch 1 且 `prev` 是同一会话的 `conversation_created`。没有 `cause` 的只能是新建的一次性 schedule，且 `depth` 至少为 1。投影只比较 rank：名称和说明逐项后写者胜并合并进机器人定义，skill 全网按 `skill_id`、schedule 按 `schedule_id`、笔记按 `memory_id` 后写者胜并保留删除，`schedule_controlled` 按 rank 改 `enabled` 或删除，修改记录按 `change_id` 取 rank 最小者。

`context_checkpoint`、没有 `cause` 的 `bot_changed`（bot 自设提醒）、`schedule_run_claimed` 和 `bot_messaged` 只能由对应会话的负责人写：前三者看写入的会话，`bot_messaged` 看发送方会话 `from.conversation_id`，写 `bot_messaged` 时 `deps` 也包含发送方会话里各作者的最新事件。入库和本机写入都按作者写入时的认知核对：只看作者自己此前的事件和每个 `deps` 所在日志到该依赖为止的 `conversation_owner`，其中 epoch 最大的若有一条指名作者即通过，否则拒绝并记 `sync.event_rejected` 日志。`schedule_run_claimed` 的 `owner_epoch` 还必须等于这个最大 epoch。作者还没看到的换负责人不算违规；乱序到达的事件先等依赖入库再核对，所以各副本结论一致。换负责人后旧负责人结束回合时只写回复，不再写检查点。同一 `(会话, schedule_id, slot_ms)` 的领取投影为 `schedule_claims`，`owner_epoch` 大者胜，相同时作者较小者胜，与到达顺序无关。

通知由 `work_finished` 和会话里的 `question_asked` 派生，通知 ID 即该事件 hash。会话归属投影 `session_owners` 来自 `work_progress.thread_id`（作者即设备）和 `session_attributed`：工作记录 rank 最小者胜，`session_attributed` rank 最大者胜并覆盖工作记录，`session_muted` 置 `muted`；所属 Bot 已删除的行读取时视为无主。Codex 回合内部的命令、diff、推理和工具输出不进入 DAG。

入库逐个检查，顺序为 hash、网络、事件形状与 `attestation`，重复，作者是当前成员或带上限的已撤销设备，作者签名，然后才看 `seq` 与 `prev` 的连续性和 `deps` 是否已入库。签名只在入库时验证一次，未通过验证的事件不会入库，也不会触发 `forked`。重复事件忽略；序号跳过的事件丢弃并从缺口重新拉取；缺少 `deps` 的事件进入 `pending_events`，依赖到齐后自动入库。所有事件存在同一张 `events` 表：`prev` 是本机主分支头的为 `main=1`，同作者同序号的另一支为 `main=0`，作为分叉证据，不锁住会话。`log_heads` 保存每个作者主分支的 `max_seq` 和 `head_hash`，即本机版本向量。

`effective` 标记事件是否计入投影，只由签名证据决定：作者从分叉序号起两支都不计入；当前成员 Node 计入此前的主分支事件；被撤销设备计入不超过上限的事件，分叉后沿 `cutoff_hash` 往回的那一支也计入。依赖只决定顺序，不传递是否计入。成员目录变化或分叉证据到达时，同一事务内重新计算 `effective`，有变化时清空并按顺序重放 reducer 写的投影表，本机账本不变，不重放副作用，然后推送一条不带会话的 `changed`。

入库时计算 `lamport = 1 + max(prev, deps 的 lamport)`，读取顺序为 `(lamport, created_at_ms, hash)`。投影由 reducer 对每个事件增量执行一次：同一 `message_id` 取因果最早者；`ConversationOwner` 取 `epoch` 最大者；工作状态只前进。负责人同级再相同时取请求身份较小者，请求身份为 `SHA256(attestation 签名输入 ‖ 签名设备)`。同一份已签名请求经不同入口重试时请求身份相同，外层事件 hash 不参与仲裁，重试不会改变结果。

事件上限为 1 MiB。事件的 `blobs` 使用 `BlobRef { hash, size }`；本人消息的 `attachments` 为 `{ hash, size, name }`，按哈希升序、每个文件一次，与事件 `blobs` 一一对应，`name` 为 1 至 255 字节、不含路径分隔符和控制字符的文件名。附件单件上限 64 MiB，正文是 `blobs/sha256/<前两位>/<hash>` 文件，按网络和会话引用授权读取。

## 写入保护

Node 的写入状态为 `aligning`、`read_only`、`writable` 或 `forked`。只有 `writable` 时 Node 追加自己的事件，其他状态下所有本地写入在追加时被拒绝，调度和执行在进入 `writable` 前不启动。读取、同步和入库其他设备的事件不受影响。

| 状态 | 进入条件 | 离开条件 |
|---|---|---|
| `aligning` | 启动时成员目录中还有其他 Node | 本次启动以来每台其他当前成员 Node 都确认过：一次同步结束时它记录的本机 `max_seq` 不大于本机，本机在该序号上的事件就是它的头，本机日志也没有分叉；对端更多时先拉回本机自己的日志。成员目录变化时重新判断，被撤销的 Node 不再需要确认 |
| `read_only` | 对齐中且还没确认的 Node 都同步失败 | 这些 Node 恢复后补齐确认，或本人调用 `confirm_writable` |
| `writable` | 没有其他 Node、对齐完成或本人确认 | 发现分叉 |
| `forked` | 本机存有自己日志的另一支 | 不离开；撤销这台设备后用新数据目录重新加入 |

拒绝写入时的错误为 `this Node is aligning with other Nodes; try again shortly`、`this Node is read-only until you confirm it was not restored from a backup` 和 `this Node's log has forked and it no longer writes`，网页翻译成中文。`confirm_writable` 只记录在本机，不进入 DAG，对 `forked` 无效。

分叉的两支经同步传给每台成员 Node，从分叉序号起两支都不计入，其他作者和其他会话照常写入。同一设备的两份副本都会存有对方那一支，都进入 `forked` 并停止写入。`write_state` 返回 `{ state, conflicts }`，每条冲突含作者、序号、本机主分支的版本（`kept`）和另一支的版本（`other`）。状态变化时向已订阅的浏览器推送 `{"type":"changed","payload":{"type":"write_state","state":...}}`。

## Node 之间的传输

### iroh Endpoint

每台 Node 运行一个 iroh 1.3 `Endpoint`，`SecretKey` 就是设备密钥。Endpoint 用 Minimal 预设、`MemoryLookup` 和 `RelayMode::Custom`：relay 来自 `network.relays`、本 Node 自己的 relay、公共入口的 relay，以及可选的 n0 公共 relay。Node 不向 n0 DNS、pkarr 或局域网发布地址，持久对端地址来自 ticket 和个人网络内部交换的签名地址记录。配置的发现服务可提供临时 IP 提示，和已有 relay 路径合并后由 iroh 自动选择连接路径。连接优先直连，失败时经普通 relay 转发 QUIC 密文。

`Router` 按 ALPN 分发连接。每个请求占一条 QUIC 双向流，帧为 4 字节大端长度前缀加 JSON，单帧最多 16 MiB。

| ALPN | 用途 | 接受的对端 |
|---|---|---|
| `vibeface/discovery/1` | 固定的地址登记、查找与空探测 | 任意通过 TLS 证明身份的设备；不授予业务权限 |
| `vibeface/sync/1` | `hello`、`membership`、`pull`、`forks`、`have` | 未撤销的成员 Node |
| `vibeface/blob/1` | 按哈希和偏移读取附件 | 未撤销的成员 Node |
| `vibeface/rpc/1` | 投递、派发、中断、App Server 操作转发、进度读取、地址簿交换、加入请求汇总 | 未撤销的成员 Node |
| `vibeface/client/1` | 网关转发的浏览器 Noise 会话，每条流一个会话 | 任意设备，会话本身由 Noise 认证 |
| `vibeface/join/1` | 只开放 `join` 和 `join_status` | 任意设备 |

`sync`、`blob` 和 `rpc` 的每个请求带 `network_id`，与本机不一致时关闭连接。连接接受时按 `remote_id()` 核对成员资格，非成员 Node 被拒绝；成员目录变化时已接受的连接重新核对，对端不再是成员就关闭。撤销还会关闭到被撤销 Node 的出站连接并删除它的地址。公共入口不接受任何入站 iroh 连接。

### 同步

1. 双方交换 `{"type":"hello", network_id, vector: {author: [max_seq, head_hash]}, boot, membership_epoch, membership, forks}`。成员目录摘要不同时交换 `membership`，双方都合并为并集。
2. 对每个对方领先的作者发 `{"type":"pull", author, from_seq, limit}`，对方按 `seq` 升序返回主分支事件，每批最多 1024 条、合计 8 MiB。
3. 对方的头本机没有时，从两边较小的序号起每次加倍退后、每次拉一条，直到拉到本机已有的事件，再向后拉回对方那一支。
4. `forks` 是本机所有分叉作者从分叉序号起两支事件 hash 的摘要，没有分叉时为空字符串。摘要不同时发 `{"type":"forks", after}`，`after` 为上一页最后一条的 `[author, seq, hash]` 或 `null`，对方返回其后的分叉证据，每页限额同 `pull`，每轮最多 64 页。
5. 本机追加事件后向已连接的成员发 `{"type":"have", author, seq, hash}`，对方落后时立即同步。收到的 Hello 带本机没有的头、不同的成员目录摘要或新的一对不同分叉摘要时，也立即同步。空闲连接每 60 秒重新 Hello。
6. `boot` 是对端本次启动的编号。编号变化说明对端重启，本机立即关闭到它的旧连接并重新同步，不等 QUIC 空闲超时。

同步只写入历史和投影，不重新执行历史中的操作。同步失败按退避重试，最长间隔 30 秒。

### 地址簿

每台 Node 为自己签一条地址记录 `{ version, network_id, device_id, seq, endpoint, gateway_url }`，以 `address` 域签名，`seq` 严格递增。`gateway_url` 在配置了 `network.entry` 时取入口 URL，否则在启用 `gateway` 时取 `public_url`。同步循环每 5 秒检查本机地址，变化后重签并唤醒同步；成员之间经 `rpc` 的 `addresses` 交换记录，最多保留 256 条。浏览器经 `entrances` 读取，用 WASM 验签，丢弃非成员、已撤销、其他网络和带路径或凭证的 `gateway_url`。浏览器为每台 Node 保存已接受记录的 `seq` 高水位，撤回地址的记录也保存，刷新后不接受更旧的记录。HTTPS 且主机不是回环、私网或单段名称的入口算公网，二维码只用公网入口。

### 附件

接收方在 `vibeface/blob/1` 上发送 `{ network_id, hash, size, offset }`。来源核对请求方是成员 Node、该哈希被本机某个事件引用且大小一致，然后从 `offset` 起按 64 KiB 发送。接收方写入 `blobs/partial/<hash>.part`，每 1 MiB `fsync` 并在 SQLite 中提交偏移，断线或重启后从已提交位置继续。收齐后核对大小和 SHA256 再原子改名。一次只下载一个附件，暂存最多 256 MiB、64 个对象。

### 节点间 RPC

`vibeface/rpc/1` 的请求为 `{ type, network_id, ... }`：

| type | 用途 |
|---|---|
| `capabilities` | 项目目录、是否运行 relay、管理 Agent 类型与能力，以及 Codex 状态 |
| `addresses` | 交换签名地址记录 |
| `deliver` | 入口 outbox 向负责人投递消息或 job |
| `scheduler_jobs` | 读取负责人上该会话最近的调度回合，`replied` 表示回复已写入 |
| `progress_read` | 读取管理轮和原生工作的临时进度 |
| `dispatch_work` | 派发方请求目标接纳工作 |
| `interrupt_work` | 停止目标上的工作 |
| `codex_state` | 转发本人读状态，要求 `human: true` |
| `codex_respond` | 另一台设备的 Bot 应答本设备挂起的 App Server 请求，同一请求只应答一次 |
| `hand_over_session` | 另一台设备上的 Bot 转交或不再关注本设备运行的会话，带发起 Bot 的会话 `from` 和它看到的 `version`；本设备核对归属与版本后写入，状态已成立时不写 |
| `cancel_manager`、`resolve` | 转发本人的管理控制，要求 `human: true` |
| `pending_joins`、`approve_join` | 汇总其他入口上的加入请求，把批准转发到保存请求的 Node |

### Relay 与公共入口

启用 `relay_listen` 的 Node 运行 `iroh-relay` 服务。个人网络里的 relay 只接纳当前成员 Node，以及带有效邀请令牌 `SHA256("vibeface:relay:v1" ‖ invite_hash)` 的加入者，所以只能经 relay 连通的新 Node 要用运行 relay 的 Node 发出的邀请。relay 看不到内容，但能看到哪些设备连接它、彼此何时通信和流量大小。

公共入口（`init --public-entry`）不加入个人网络。它的 relay 只接纳持有 `entry-token` 的设备，它不运行调度、执行和同步，不保存业务数据，本机设置路径回 404。成员 Node 用 `--entry URL --relay-token-file PATH` 指向它：把入口的 relay 加为自己的 relay 并带上令牌，地址记录的 `gateway_url` 写入口 URL，于是邀请链接和二维码打开的是入口的 origin。浏览器经入口的 `/api/client` 连到成员 Node，入口只搬运 Noise 密文。部署步骤见 [配置公共入口](node.md#配置公共入口)。

### 仅发现协议

`relay_mode=discovery-only` 的公共入口只注册 `vibeface/discovery/1`，不启动 `iroh-relay`，不提供 HTTP 浏览器转发，也不接受 `SYNC`、`BLOB`、`RPC`、`CLIENT`、`JOIN` 或 `DECISION` 业务 ALPN。网页开启时只提供静态资源和服务信息。

发现协议使用长度前缀 JSON，单帧最多 512 字节，未知字段被拒绝。请求只有 `register`、`lookup {device_id}` 和空的 `probe`。服务端用连接的 TLS 身份和已选中的实际 IP 路径构造 `registered {address}`、`peer {device_id,address}` 和 `notice {device_id,address}`；不接受客户端提交地址或任意转发载荷。对端据通知发起固定空探测，再通过原有业务 ALPN 通信。发现服务的身份由客户端配置钉住。对端查询和通知提供的地址只作临时提示；本机观察地址会随自身签名地址记录在个人网络内传播。

总控制连接上限为 512，同来源 IP 上限 8。租约覆盖整条控制会话和读写，最长 60 秒，每连接最多 60 个请求。每个登记最多暂存 8 条连接通知，同一目标的通知间隔至少 250 毫秒；客户端最多同时进行 8 个探测。一个设备可有并发登记，结束时只清理该连接的登记；临时加入连接退出后，已有主连接仍可被找到。

已知设备编号可以查询在线状态和观察地址，不提供枚举接口。运营者可见设备、IP 和查询关系。仅发现不能补偿被封锁或不可打洞的 UDP 路径；普通 relay 仍由用户另行配置并由 iroh 自动回退。部署步骤见[运行公共发现服务](node.md#运行不转发业务数据的公共发现服务)。

## 通知和自动后续

加密通道推送 `{ "type": "changed", "payload": ... }`。payload 区分历史变更、`manager_progress`、`native_progress`、`write_state` 与失效提醒。客户端读取 `notifications`，用稳定 `client_id` 确认 `notification_id`，不同浏览器各自确认。

管理进度包含 `conversation_id`、`trigger`、`invocation_id`、`source_node`、递增 `sequence` 和 `progress`，`progress.type` 为 `text_delta`、`text_snapshot`、`status` 或 `tool`。原生进度包含 `conversation_id`、`work_id`、`target_node`、`sequence` 和 `progress`。临时进度不写入事件日志。客户端读取 `conversation` 时，入口为该会话建立有界的只读观察，经 `progress_read` 从负责人和执行节点读取进度。回合状态变化时，该设备在进度环里放一条只在环内的 `job_changed` 提示，观察方收到后让客户端重读会话；它不进事件日志，也不推给本机客户端。

`message.report_results` 默认 `true`，保存在签名的用户消息中。为 `false` 时，该消息引出的工作结束后不再入队汇报。

协调 Agent 总是完全权限，没有自主度设置和只读汇报轮。

`daily_decision_limit` 只限制每个 Node、每个个人网络的 UTC 每日结果汇报轮和其他非本人触发的轮次，预算先预留，失败调用不保证退回，不是 token 或账单上限。
