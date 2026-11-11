# Node 协议参考

协议版本为 `1`，数据格式 `data_epoch=1`，Core 和 delivery 数据库的 `user_version=1`。共享结构以 `apps/core/src/model.rs` 为准。本文描述本机配置、HTTP 与浏览器通道、业务 RPC、调度、工作、事件 DAG、写入保护和 Node 之间的 iroh 传输。同一个人网络中的所有 Node 必须使用当前格式。`data-epoch.json` 检查本机目录，未知协议字段和旧格式会被拒绝。程序不迁移或清空旧数据。

## 身份与数据范围

`network_id` 是个人网络根 Ed25519 公钥，`device_id` 是设备 Ed25519 公钥本身，均编码为 64 个小写十六进制字符。Node 的 `device_id` 同时是它的 iroh `EndpointId`。地址不参与身份计算。

个人网络使用 `NetworkState { genesis, checkpoints, events }`，保留所有根 checkpoint 和各 epoch 的管理事件，两份状态在 checkpoint 链兼容时的合并是并集；同一 epoch 出现两个不同 checkpoint 时合并失败，本机状态不变。根身份签署初始 `Genesis`，其中包含根公钥及初始 Node、客户端成员。每个 `RootCheckpoint` 带下一把恢复密钥 `recovery_key`，由上一把签名，第一把是根密钥。恢复私钥只在加密恢复文件里，Node 不持有。

`Member` 包含 `device_id`、`kind`、`name` 和 `manage`，没有角色。`kind` 为 `node` 或 `client`。任何管理设备都能授予或撤销成员，并可把 `manage` 授予新的客户端成员，或用一条新的 `grant` 改变已加入设备的 `name`（电脑、浏览器或手机）或客户端的 `manage`，`kind` 不变。客户端成员不需要管理权限也能撤销自己（`issuer` 与被撤销设备相同），不能撤销其他设备；Node 成员不能撤销自己。取消管理或撤销都不能让网络失去最后一台管理设备，权限变化后该设备的订阅会断开重连以取得新权限；Node 成员不能持有管理权限。Node 不分类型：加入个人网络的 Node 承担网页、同步、调度和执行，`network` 决定它是否另外运行 relay、是否加入全网；不加入任何个人网络、只按配置提供 relay 和浏览器转发的 Node 称为公共节点。任何 Node 都可显式启用独立的文件交付服务，服务权限不改变成员关系。

管理事件含 `version`、`network_id`、`epoch`、`parents`、`issuer`、`action` 和 `signature`。授权动作为 `grant`，撤销动作为 `revoke`，撤销带 `cutoff_seq` 和 `cutoff_hash`，即发起撤销时所连 Node 上该设备主分支的头，设备没写过时为 `0` 和 `null`。被撤销设备只有不超过这个上限的事件计入投影，超出的仍可入库但不计入。并发的授予与撤销冲突时撤销优先。管理事件签发者有没有权限，按该事件因果祖先里的授权投影判定，不看最终目录。一台设备被撤销后，只有撤销事件因果祖先里它已签的管理事件计入；不在祖先里的事件，包括 `parents` 或签名时间写在撤销之前的授权，以及由这些授权派生的成员和管理权限，一律不计入，事件本身仍保留在历史里。客户端的管理权被取消时同理，只有之后因果上重新授予才恢复。互相撤销或派生成环而无法确定的权限按无权处理；同一设备被并发授予不同权限时取较窄的一份。根恢复由当前恢复密钥签名、epoch 加一的 checkpoint 建立新授权并换恢复密钥；`directory` 为空的 checkpoint 只换恢复密钥，目录沿用上一 epoch，立即生效；checkpoint 的 `retired` 列出没有保留的设备及其最终上限，旧 epoch 的事件不能放宽当前上限。

换目录的 checkpoint 带签名时间 `issued_at_ms` 和 `heads`（签名时恢复密钥所在 epoch 的管理事件 heads，rotation 为空），默认从这时起等 48 小时（`RECOVERY_DELAY_MS`）生效，之前视图仍是原目录，`pending` 给出它的哈希、`effective_at_ms` 和待生效成员。等待期内，原目录里的管理设备可以在原 epoch 签 `approve` 立即确认或签 `veto` 拦截。立即确认的事件是 `{"type":"approve","checkpoint":<checkpoint 哈希>,"at_ms":<签名时间>}`，只能由当前管理设备针对当前 `pending` 生成，并且 `at_ms` 必须早于自然生效时间；它的因果祖先须包含 checkpoint 的全部 `heads`。确认事件到达后，Node 在 `at_ms` 到达时按同一事件 DAG 投影新目录。时钟落后的 Node 将 `pending.effective_at_ms` 提前到最早合法确认时间，让已有恢复定时器在该时刻重新安装目录。管理设备也可以签 `veto`：`{"type":"veto","checkpoint":<checkpoint 哈希>,"at_ms":<签名时间>}`。否决优先于确认，算不算数只看它自己和它的因果祖先，之后到达的事件改变不了：因果祖先须包含该 checkpoint 的全部 `heads`，签发者在这些祖先里是管理设备且未被撤销（否则整份状态无效），`at_ms` 早于生效时刻；目标不符或祖先不全的否决不起作用。所以改写 `parents` 绕不过找回前的撤销，而先否决、后被撤销的设备，否决仍然有效。`heads` 含本机没有的事件的 checkpoint 不生效，`recover` 直接拒绝（`this recovery rests on device changes this Node has not received; sync it first`），同步合并时它保持等待，补齐事件后再按签名时间判定。同一把恢复密钥在找回生效前签出的 rotation（`issued_at_ms` 早于该找回的生效时刻）也会让它作废，换恢复密码不受等待中的找回阻塞。被否决或被 rotation 取代的 checkpoint 及建在它上面的 checkpoint 作废，哈希列入视图的 `stopped`，原目录继续有效。同一把恢复密钥只能出现一次；一把密钥再签时，此前签的每个找回要么有合法否决，要么被这次 rotation 取代，否则整份状态无效。最多 64 个未被否决的 checkpoint。生效不是终局：生效目录是签名记录和墙钟的纯函数，Node 不拒绝任何合法历史。窗口内签的否决或确认（或倒签到窗口内的 rotation）晚到时，所有 Node 按签名时间收敛；否决晚到时可将已按墙钟或确认翻转的 Node 回到旧目录。回滚前，它可能短暂按新目录放行。所以翻转时不往共享日志写不可撤回的记录：找回移出的 Node 上未完成的工作不记为待确认，它超出上限的写入只是在投影里不计入，回滚后重新计入；只有管理设备签名的 `revoke` 才把被移除设备的工作记为待确认（`unverified`）。作废邀请、`identity_meta.recovery_since`（首次见到某把密钥生效的时间，只用于展示）和地址簿清理都是本机可重做的。认为对方已被移除的 Node 仍接受对方推来的成员状态，所以被找回移出、又因否决回到目录的 Node 也能收敛。未被移除的被偷管理设备可以随时拦下找回或确认找回（含补签迟到否决），这个限制早已存在，补救是先移除它再找回；移除后它签出的管理事件，包括倒签到撤销之前的授权及由此派生的身份，都不计入（见上文）。曾考虑过的方案：生效即终局会让休眠错过窗口的 Node 拒绝否决、永久分叉；成员时钟会让丢失的 Node 永远拦住找回，且两台都过期的 Node 互相交换后仍会分叉；否决按 epoch 最终投影计数会让事后撤销否决人立即复活找回。视图的 `epoch` 是生效的 epoch。`attestation` 的签名设备须在任一 epoch 做过客户端。

一个数据目录只属于一个个人网络，一个 Node 进程只服务一个网络。成员关系、项目和会话不发布到公共目录。签名地址记录只在个人网络内传播；`network.mesh.enabled`（`init` 默认开启）另向[全网](#全网)公开本 Node 的设备编号、直连地址、home relay 和角色。

## 本机配置

`config.json` 由 `init` 生成。网页通过加密客户端通道修改 `manager`、`roots`、`providers` 和 `bins` 后，新操作立即读取；其他字段手动修改后重启生效。字段名固定，不接受未知字段。

| 字段 | 含义 |
|---|---|
| `listen` | HTTP 监听地址，默认 `127.0.0.1:4790`，决定可接入的网卡；iroh 默认在同一端口号上监听 UDP |
| `local_listen` | 本机会话的固定回环地址，默认 `127.0.0.1:4791`，让本机 origin 和浏览器存储跨重启不变 |
| `public_url` | 本 Node 的网页 origin，公网为 HTTPS；网页开启时作为地址记录的 `gateway_url` |
| `tls` | 默认 `false`；`true` 时 `listen` 只接受 TLS，使用数据目录里的证书链 `tls-cert.pem` 和私钥 `tls-key.pem`，`init --tls` 先为 `public_url` 的主机名或 IP 写一对自签占位，部署方可换成 CA 签发的一对，文件变化后不重启即重新加载；要求 HTTPS 的 `public_url` |
| `allowed_origins` | 额外允许的浏览器 origin 数组，默认空；`init --allowed-origin` 可重复指定 |
| `allow_insecure_lan` | 明确允许可信 LAN 上的明文 HTTP |
| `local_auto_access` | 默认 `true`，通过独立回环入口和本机凭据建立受限本机会话 |
| `web_enabled` | 是否提供网页和浏览器访问，默认 `true`。启动时 `--no-web` 只关闭本次进程的浏览器访问 |
| `roots` | 本机 Agent 和工作可选择的绝对项目目录 |
| `network` | iroh 传输、relay 和全网设置，见下表 |
| `forwarding` | 浏览器转发：`any` 默认 `false`，只转发给本网络的 Node，`true` 时转发给任何设备（`--forward-any`）；`sessions` 是同时转发的会话数，默认 256（`--forward-sessions`）；`queue` 是排队数，默认 0（`--forward-queue`），两者之和不超过 256；`rate` 是每个会话双向合计每秒的字节数，省略时不限（`--forward-rate`）；`per_ip` 是同一浏览器地址（IPv6 按 /56 计）在 `listen` 上同时保持的 `/api/client` 连接数，含目标为本 Node 的会话，省略时不限（`--forward-per-ip`）；`trusted_proxies` 是自建反代的地址，只有来自它们的连接才按单个 `X-Real-IP` 认定浏览器地址（`--trusted-proxy`），浏览器直连的 Node（如官方公共节点）不设 |
| `manager` | Bot 配置，`provider` 是运行 Bot 的产品（`codex`、`cursor`、`claude`、`pi` 或 `opencode`），关闭时为 `null` |
| `providers.execution` | 执行派发工作的产品列表，默认为空，只能列 `supports` 含 `execution` 的产品（`codex`、`cursor`、`claude`、`pi` 或 `opencode`）；为空时不提供派发执行能力。`providers.execution_models` 按 Provider 保存执行任务默认模型，空值跟随 Provider 默认；它与 Bot 的 `manager.model` 分开 |
| `providers.codex_app_server` | `{mode, socket}`：整个对象省略时默认 `spawn`，提供对象时 `mode` 必填；`system` 只有显式绝对 `socket` 才连接外部 App Server；`spawn` 时 Node 使用 `<data-dir>/providers/codex` 作为 Node-owned `CODEX_HOME`，不扫描或复用用户 App Server |
| `bins` | 各产品的程序，键为产品名 `codex`、`cursor`、`claude`、`pi` 或 `opencode`；没写的用同名程序，Cursor 为官方 `agent` CLI。后台运行时用绝对路径 |
| `timeout_secs` | 单次 SDK 调用超时，默认 600 秒 |
| `daily_decision_limit` | 本 Node 每个网络的 UTC 每日非本人触发的管理调用次数，默认 20；本人主动聊天不受此额度限制；协调 Agent 的分诊唤醒按同一上限另行计数 |

`network` 的字段：

| 字段 | `init` 参数 | 含义 |
|---|---|---|
| `bind` | 无 | iroh 的 UDP 地址；省略时在 `listen` 端口上绑定 IPv4 与 IPv6 通配地址，IPv6 失败不阻止启动 |
| `relays` | `--relay URL`、`--relay-token URL=FILE` | 额外使用的 relay `[{url, token?}]`，最多 16 个；`token` 是该 relay 向个人网络以外的设备要求的令牌 |
| `relay_listen` | `--relay-listen ADDR` | 本 Node 运行 TCP relay 的地址 |
| `relay_url` | `--relay-url URL` | 其他设备访问本 relay 的 URL，默认 `http://<relay_listen>` |
| `relay_operator_token` | `--relay-operator-token FILE` | 个人网络以外的设备出示后可用本 relay 的令牌，64 个十六进制字符 |
| `relay_open` | `--relay-open` | 本 relay 接纳任何设备，默认 `false` |
| `relay_rate` | `--relay-rate BYTES` | relay 每秒从每个连接读取的字节数，省略时不限 |
| `relay_max_clients` | `--relay-max-clients N` | relay 同时保持的个人网络以外的连接数，本 Node 和成员 Node 不计，省略时不限 |
| `relay_max_clients_per_ip` | `--relay-max-clients-per-ip N` | 同一 IPv4 地址或 IPv6 /56 同时保持的 TCP 连接数，含成员，省略时不限 |
| `relay_blocked` | 无 | relay 拒绝的设备编号 |
| `mesh` | `--no-mesh`、`--mesh-network NAME`、`--bootstrap URL` | `{enabled, network, bootstrap, allow_private}`，见[全网](#全网)。省略时关闭；`init` 和首次直接启动写 `enabled: true`、`network: "vibeface"` 和官方 bootstrap 节点 |

iroh 默认开启路由器端口映射；配置了 `network.bind` 时关闭，不发布未配置的路由器地址。`relay_url`、`relay_operator_token`、`relay_open`、`relay_rate`、`relay_max_clients`、`relay_max_clients_per_ip` 和 `relay_blocked` 要求 `relay_listen`；`relay_open` 要求 `relay_rate` 和 `relay_max_clients`，且不能有 `relay_operator_token`；relay 监听通配地址时必须给 `relay_url`。`mesh.bootstrap` 最多 16 个，同一设备只列一次；给了 `--bootstrap` 或换了 `--mesh-network` 时不再带官方 bootstrap 节点。

启用管理 Agent 时，`manager` 对象包含以下全部字段。Cursor 模式要求非空 `model`，Codex 模式可用空字符串选择其默认模型。

```json
{
	"provider": "cursor",
	"workspace": "/absolute/manager-workspace",
	"model": "auto",
	"reasoning_effort": null,
	"backup": { "automatic": false, "candidates": [] }
}
```

`reasoning_effort` 为可选推理档位。`backup` 保存本机备用策略，默认关闭。候选项为 `{provider, model, reasoning_effort}`，最多 5 个不同的 Provider 与模型组合，自动策略只允许原回合已确认未执行时尝试一次。

`provider` 可为 `codex`、`cursor`、`claude`、`pi` 或 `opencode`。Codex Bot 回合在 Codex App Server 上运行（`turn/start`，`clientUserMessageId` 为 attempt ID）。`manager_capabilities` 声明本机适配器能力，包含流式进度、取消请求、本机原生会话延续和 steer；Codex、Claude Code、pi 与 opencode 支持活动补充，分别使用原生 steer 或持续输入；当前 Cursor ACP 适配器未提供原生 steer，活动补充明确拒绝。接纳与回合归属分别记录，不代表模型已读。`cancel_confirmation=true` 表示取消可被确认，Codex 以 App Server 回合结束为证据，Cursor 必须收到 Provider 终态；本地进程组退出不等于 Provider 确认取消，结果仍须按待核对路径结算。配置存在、进程可达和真实模型可用是不同状态。

Cursor 每轮启动官方 CLI ACP 进程，按个人网络、Bot 会话、任务或设备协调会话隔离 owner 和私有存储。同一个 owner 串行运行，不同 Bot 可同时运行。`CURSOR_CONFIG_DIR` 保存私有 ACP 会话，`CURSOR_DATA_DIR` 保存私有项目数据，真实 HOME 留给 CLI 已有登录。Node 仅新建或加载自己记录归属的会话，忽略加载阶段回放，不扫描或导入个人历史。CLI 仍可能加载用户或项目 MCP、规则与 hooks，这两个私有目录不表示全部个人配置隔离。退出时停止全部受管进程组。

Cursor 执行器先保存 Node 自己的 session 归属和 prompt receipt，再发送提示；终态先写本机 journal，再报告完成。receipt 是 Node 的读回定位，不是 Cursor 原生 Run ID。ACP 没有持久运行查询或幂等 prompt 契约，断流、重启或缺少终态时保持 Unknown，只读回已有 journal，不重发 prompt，也不以历史内容猜测完成。取消只有原 prompt 返回 cancelled 才确认，进程退出不作为取消证据。Bot 的 `run_reconcile=false` 能力声明不变。

pi 的实际会话保存在 `<data-dir>/providers/pi/sessions`，真实启动同时固定 `--session-dir` 与 `PI_CODING_AGENT_SESSION_DIR`。原有 pi 认证配置不变。OpenCode 的数据、配置、状态和缓存分别使用 `<data-dir>/providers/opencode/{data,config,state,cache}`，`OPENCODE_DB` 固定为该 data 根中的 `opencode/opencode.db`。Node 强制私有 `OPENCODE_CONFIG_DIR`，忽略外部 `OPENCODE_CONFIG`、额外 CLI 配置和项目配置覆盖，Shell 工具继承相同私有根。

Opencode Bot 和执行器使用 2.2.0 的 `/api` 接口。每轮发送提示前，Node 用 `PUT /api/session/<id>/environment` 整体替换该 session 的工具进程环境，只含三部分：平台基础变量（`PATH`、`HOME`、临时目录、locale、时区和 Windows 系统变量）、Node 私有的 opencode 目录变量（XDG 目录、`OPENCODE_DB`、`OPENCODE_CONFIG_DIR` 等）和本轮的 Node 工具凭证；Node 自己的其余环境和 Provider 密钥不进入会话。Node 不通过插件修改 Shell 环境；设置失败则不发送提示。读取消息和待处理 inbox 后才判断发送结果，不重发结果未知的请求。取消会删除本轮仍在 inbox 中的输入；只有没有待处理输入时才结清待核对，新的回合也不能唤醒残留队列。外部 CLI 会话的 hook 插件不在此适配范围内。

## 数据目录

| 文件 | 内容 |
|---|---|
| `identity.json` | 设备私钥，0600 |
| `config.json` | 本机配置，0600 |
| `data-epoch.json` | `{"data_epoch":1}`，程序拒绝其他 epoch |
| `node.sqlite3` | 成员目录、签名日志、投影和本机表，`user_version=1` |
| `bootstrap.json` | 建网前的一次性本机 capability |
| `blobs/sha256/<前两位>/<hash>` | 附件正文，`blobs/partial/` 为续传暂存 |
| `local-sessions.json` | 本机会话的 cookie 摘要 |
| `local-access.json` | 本机 HTTP 所有者凭据 `{origin, node_id, capability}`，0600，Node 每次启动重新生成；`vibeface` 命令和桌面应用读取它证明所属 OS 账户，用于 `/api/local/setup`、`/api/local/launch`、`/api/hooks`、`/api/triage` 和 `/api/log` |
| `tool-port` | 上次协调工具回环端口号，明文，不是秘密；端口空闲时复用，换端口意味着 Codex 线程要重开 |
| `decision-tokens.json` | decision token 的摘要、`token_id`、标签和创建时间，0600，最多 16 个；明文 token 只在创建时显示一次，撤销即从文件删除 |
| `triage-webhook.key` | webhook 的 HMAC 密钥，0600；清空 webhook 密钥时删除 |
| `hook-spool/` | Node 不在线时 `vibeface hook` 暂存的事件 JSON，目录和文件私有；Node 每次扫描读入后删除，解析失败的也删除 |
| `update.json` | 更新策略（渠道、是否自动、安装根目录），0600，不进入共享记录 |
| `update-pending.json` | 一次更新激活的 journal（前后版本、快照路径、阶段）；确认成功后删除，失败时保留并要求再次 `update apply`，它存在时不清理任何快照 |
| `delivery-settings.json` | 可选文件交付设置，与 `config.json` 分开 |
| `delivery-wallet.json` | 独立加密 EVM 钱包，0600，不包含接收密钥 |
| `delivery/service.sqlite3` | 订单、接收密钥、证明任务、续传偏移、nonce 和签名交易账本；`user_version=1`，只初始化空库，不进入共享 DAG |
| `delivery/objects/`、`delivery/private/` | 交付对象与本机私密材料，目录 0700、文件 0600；完整恢复需保留整个 `delivery/` |
| `join.json` | `vibeface join` 或本机网页进行中的加入请求，重试时沿用同一请求 |
| `codex-app-server.pid` | 托管 App Server 的进程组号 |
| `cursor-agent.pid` | 受管 Cursor Agent CLI 的进程组号和程序路径，Node 启动时结束上次遗留且命令相符的进程组 |
| `cursor/instances/<owner 摘要>/` | 每个 Cursor Bot、coordination Agent 或任务 owner 的私有目录；`config/`、`data/` 是官方 CLI 根，`sessions/` 记录归属，`prompts/` 保存发送前及终态 journal，目录 0700，不进入共享 DAG |
| `providers/codex/local-login.json` | 首次导入的私有 auth.json 摘要标记，不含认证值；用户独立修改该文件后不再借用本机登录 |
| `providers/claude/local-login.json` | 当前 Node access 快照的摘要标记，不含认证值；每次启动前先检查私有登录，只有确认未登录时才重新建立借用标记 |
| `providers/codex/local-login-access.json`、`providers/claude/local-login-access.json` | Node 专属访问令牌快照，0600，不含源刷新令牌；后续更新不写入 CLI 的 auth.json 或 .credentials.json |
| `providers/pi/sessions/` | pi 原生会话文件，Node 的 `pi/` 仍保存恢复指针 |
| `providers/opencode/` | OpenCode 私有 XDG 数据、配置、状态、缓存及数据库，不复制用户认证 |
| `cursor/work-owners.json` | 本机 Cursor 任务到 ACP 会话、owner 和工作目录的映射，0600；不含模型密钥，不进入共享 DAG |
| `node.log`、`node.log.1`…`node.log.4` | Node 日志，JSON lines，0600，8 MiB 轮转，共保留 5 个 |
| `service.*.log`、`update.*.log` | macOS 服务和更新器的标准输出与错误 |

这张表说明各文件的用途和私密程度，不是备份白名单：`node.sqlite3` 以 WAL 模式运行，去重、待补依赖和投影状态都在数据库文件里，备份须在停止 Node 后复制整个目录。

运行中的 Node 用目录锁独占数据目录，并在私有维护 socket 上回答 `vibeface status`。`vibeface join` 遇到运行中的 Node 时，经同一 socket 发 `join { link, name }`，由 Node 自己完成加入，回复 `join` 为 `waiting`、`confirming`（带 `code`）、`joined`、`failed`（带 `reason`）或 `cancelled`。同一链接再发只读取进度，不新建请求；命令每秒发一次，直到结束。失败时 Node 保留 `join.json`，用同一链接和名称重跑会沿用原随机数，入口按原请求回答；本机网页的 `node_join` 同样如此。换一条链接时，旧承诺作废。网页正在加入时，命令被拒绝。

## HTTP 与客户端通道

允许的浏览器入口由 `public_url`、`allowed_origins`、本机 HTTP 别名，以及地址簿中未撤销成员 Node 的 `gateway_url` 组成。本机别名为 `127.0.0.1`、`localhost`、`0.0.0.0`，端口使用 `listen` 的端口。允许入口不会改变监听网卡或开放防火墙。

显式入口是不含路径、查询参数或凭证的完整 HTTP 或 HTTPS origin。非本机 HTTP 入口需要 `allow_insecure_lan`。同一主机和端口不能同时配置两个 scheme。HTTP 检查 Host，带 Origin 时必须匹配该 Host 对应的同一入口；只有 `/api/client` 和 `GET /api/info` 接受任一允许入口的跨源请求，`/api/info` 对带 Origin 的请求回 `Access-Control-Allow-Origin`，所以官方页面能读取公共节点的设备编号。普通 HTTP 正文上限为 2 MiB，自定义工具回调上限为 1 MiB。

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/`、`/assets/*` | GET | App 静态网页客户端；`/assets/` 下为带哈希的不可变资源 |
| `/sw.js`、`/manifest.webmanifest`、`/icon.svg` | GET | 离线应用外壳和 PWA 清单，`no-cache` |
| `/vibeface_secure.js`、`/vibeface_secure_bg.wasm` | GET | 同一 Rust 密码学实现的浏览器构建 |
| `/api/info` | GET | `device_id` 和 `version`；允许入口可跨源读取 |
| `/api/local/setup` | POST | 本机状态与所有者材料：所有者进程凭 `local-access.json` 的 bearer 读取一次性初始化邀请、当前入口和本机启动 token，匿名本机网页只得到状态；`forwarding.any` 的公共节点回 404 |
| `/api/client` | GET，WebSocket upgrade | 浏览器网关 |
| `/api/decision` | POST | 决定令牌回答提问 `{device_id, question_id, choice \| text}`，`Authorization: Bearer <令牌>`；目标是本机时核验令牌并回答，`forwarding.any` 的公共节点不核对成员、经 `vibeface/decision/1` 转给目标，成员 Node 只转给当前成员 Node；每台 Node 同时最多 16 个、每分钟 256 个来源、每来源 12 次（IPv6 按 /56 计），超出回 429，来源按 `forwarding.trusted_proxies` 认定 |
| `/sdk.v1.SdkCustomToolCallbackService/CallCustomTool` | POST | 只在独立的回环工具端口上提供；`vibeface tool` 发 `{toolName, args, agentId}`，回 `{result}`；按当前 attempt bearer 授权，`agentId` 不授予权限 |
| `/mcp` | POST | 只在同一私有回环工具端口提供的无状态 Streamable HTTP MCP；initialize、tools/list、tools/call 均验证当前 attempt bearer，调用同一 Runtime.tool 与 call_ref 去重路径 |

网站不提供用户名密码注册或明文业务 RPC。`/api/local/setup` 要求正文为 `{}`，实际 TCP 来源为 loopback，Host 为字面 localhost 或 loopback 地址，Origin 与该入口完整一致，且不带 `Forwarded`、`X-Forwarded-*` 或 `X-Real-IP`。所有者身份另由 `Authorization: Bearer <capability>` 证明：capability 连同 `origin`、`node_id` 记在数据目录私有的 `local-access.json`，Node 每次启动重新生成，只有能读该目录的 OS 账户进程拿得到，回环连接本身不证明账户；`vibeface` 命令和桌面应用经 Core 的 `local_setup` 控制请求读取它并代为调用。未初始化时所有者得到 `{ kind: "uninitialized", invitation: { url, device_id, capability } }`，匿名请求得到 `{ kind: "owner_required", initialized: false }`，本机网页只能用 `vibeface access` 打印的链接随 fragment 带来的邀请；已有网络时两者都得到 `{ kind: "initialized", entry, removed, local_launch? }`，`removed` 表示这台电脑已被移出，`local_launch { url, device_id, token }` 只在所有者请求且本机访问开启时给出。只有 `local_listen` 的本机 router 接受所有者凭据；公开 listener 上的同名路径不签发邀请或启动 token，隐藏真实来源的回环反代也不行。该响应不授予成员资格。

独立回环 listener（`local_listen`）提供 `/api/local/info`、`/api/local/launch`、`/api/local/exchange` 和 `/api/local/ws`。`launch` 只接受本机私有启动凭据，`exchange` 消费 URL fragment 中的单次 token 后签发绑定浏览器公钥的 HttpOnly Cookie，`ws` 仍执行 Noise 握手。公开 router 不挂这些入口。关闭 `local_auto_access` 会撤销本机会话、待兑换 token 和连接。

### 浏览器通道

浏览器连接 `/api/client` 后先发送文本帧 `{"type":"connect","target":"DEVICE_ID"}`：

- `target` 是本 Node 时，Noise 在这里终止。
- `target` 是其他设备时，网关经 iroh `vibeface/client/1` 打开一条流，双向原样搬运二进制帧，单帧最多 1 MiB。`forwarding.any` 为 `false` 时只转发给本网络的 Node，目标不是成员时不排队，直接回 `{"type":"error","code":"target_not_member"}`；连不上时回 `target_unreachable`，随后关闭。
- 网关按设备编号拨号，候选地址来自地址簿、局域网广播和全网表；表里没有时向全网邻居查询。拨号最多 7 秒，在浏览器 10 秒连接超时之前回 `target_unreachable`；没有共同 relay 且无法直连时也是这个结果。
- 网关同时最多转发 `forwarding.sessions` 个会话。名额用完时，新会话最多 `forwarding.queue` 个排队：网关立即回 `{"type":"queued","position":n}`，之后每 2 秒更新一次，轮到时回 `position` 为 0 再拨号；排队期间浏览器发来的最多 4 帧暂存，之后转给目标。队列也满时回 `gateway_busy` 并关闭。设了 `forwarding.per_ip` 时，同一浏览器地址（IPv6 按 /56 计）在 `listen` 上同时保持的 `/api/client` 连接不超过它，含目标为本 Node 的会话；超出的连接在 WebSocket 升级前收到 HTTP 429，浏览器只看到连接失败。浏览器地址是 TCP 对端，对端列在 `forwarding.trusted_proxies` 里时取它发来的唯一一个 `X-Real-IP`。`forwarding.rate` 限制每个会话双向合计的字节速率。`connect` 首帧须在 10 秒内到达。浏览器把 `queued` 当作等待而不是拒绝，每收到一帧就重新开始连接超时计时。

随后浏览器与目标执行 `Noise_XX_25519_ChaChaPoly_SHA256`，prologue 为 `vibeface:client:v1`。双方每次会话生成临时 Noise 静态密钥，并在握手 payload 中携带 `Proof { device_id, signature }`，签名覆盖 `"vibeface:noise-static:v1" ‖ prologue ‖ 本次静态公钥`。浏览器的签名来自不可导出的 WebCrypto Ed25519 `CryptoKey`。浏览器钉住目标 `device_id`，编号不符时关闭连接。目标验证浏览器的 `Proof` 后按成员目录授权。转发会话的网关、它前面的 TLS 终止方和 relay 只能看到 Noise 密文。提供网页的 origin 不在此列：它分发浏览器运行的代码，必须是本人信任的网页：自己的 Gateway、自己服务器上的公共节点或官方 App。

握手后的应用消息均为二进制加密记录，大消息分片重组。解密后的请求为 `{ id, method, params }`，响应为 `{ id, result, error?, code? }`。`code` 只标记需要本人处理的状态，目前只有 `codex_missing`（这台电脑上没找到 Codex），网页据此提示而不是报错。请求 ID 只用于匹配响应，写入去重依靠业务 ID。Noise 握手须在 10 秒内完成。连接每 20 秒心跳，90 秒无数据关闭，单次调用 120 秒超时，超时的写操作结果未知，需要按业务 ID 对账。空闲计时只看相邻两次收发的间隔，不计 Node 处理一次调用的时间，调用回复后重新计时。每台 Node 同时最多保留 32 个未加入设备的会话，超出时回 `too many devices are joining this Node; try again later`；未加入设备的会话最长 15 分钟，加入后不再计入。

| method | params | 用途 |
| --- | --- | --- |
| `info` | `{}` | 访客仅得到通用名称 `Node`、`device_id`、`version`、`initialized` 和 `network_id: null`；有效成员或本机会话还可读取真实名称、`network_id` 和 `connection_entry` |
| `authenticate` | 可选 `network_id` | 验证成员授权或本机会话；不是成员但在找回目录里做管理设备的，回 `recovery: {status: "waiting", checkpoint, effective_at_ms}` 或 `{status: "stopped", checkpoint}`，`checkpoint` 是该次找回的精确哈希，用于未知结果对账，不透露成员目录 |
| `directory` | `{}` | 当前签名目录及投影 |
| `entrances` | `{}` | 地址簿中的签名地址记录，浏览器自行验签 |
| `subscribe` | 可选 `vector` | 订阅 `changed` 推送；浏览器带已见的版本向量，入口落后时回 `behind: true` 并立即同步 |
| `rpc` | 下节业务对象 | 业务读写 |
| `delivery` | `{op, ...}` | 独立文件交付通道，见[可选文件交付](#可选文件交付)；不经业务 RPC 或 Agent 工具 |
| `write_state` | `{}` | 写入状态和本机记下的冲突，见 [写入保护](#写入保护) |
| `confirm_writable` | `{}` | 本人确认数据目录不是从备份恢复的，只读或对齐中的 Node 进入可写 |
| `diagnostics` | 可选 `since_ms`、`until_ms`，默认最近一天 | 脱敏诊断包，见 [排障](troubleshooting.md) |
| `log` | `records`：1 到 20 条 `{level, event, message, conversation_id?, work_id?, fields?}` | 浏览器写日志，来源记为 `web`，每条 JSON 不超过 8 KiB，每台设备每分钟 120 条 |
| `logs` | 可选 `since_ms`、`until_ms`、`level`、`sources`、`conversation_id`、`work_id`、`text`、`limit` | 读本机日志，返回 `records`，最多 20000 条 |
| `node_settings` | `{}` | 读取本机 `settings {roots, manager}`、`defaults`、`readiness {manager}`和 `providers`。`providers` 只列 `supports` 非空的产品，每项为 `{provider, roles, supports, installed, state, observed, issues, bin, app_server, execution_model, execution_readiness, env, probe, readiness, setup}`，`bin` 是它实际运行的程序，`installed` 说明这台电脑上找得到这个程序（同启动时的查找）：`roles` 是它现在的用途（运行 Bot 为 `agent`，执行派发的工作为 `execution`），`state` 为 `ready`、`unverified`、`needs_setup`、`starting` 或 `off`（没有用途）。`observed` 分别记录 installed、launchable、authenticated、model_known、ready，每项包含 confirmed/rejected/unknown 状态和固定 evidence 代码；未观察登录或模型不等于不可运行，也不等于 ready。`env` 只说明变量是否已设置。`display_paths` 把响应里位于主目录下的每个绝对路径映射为 `~/…` 写法，只用于显示；`configure_provider` 和 `configure_node` 收到的路径以 `~` 开头时按主目录展开 |
| `configure_node` | `manager`、`roots` | 保存可用文件夹和当前 Bot 的模型和工作目录，路径开头的 `~` 按本机主目录展开；不能开关 Bot 或更换它的服务 |
| `configure_provider` | `provider`，可选 `bot`、`execution`、`execution_model`、`bin`、`app_server`、`env`、`connect_local` | 唯一选择用途的方法。`bot: true` 让这个服务运行 Bot，保留已有的 Bot 设置，没有时用 `defaults`；Cursor 模型为空时取 `auto`，换到 Codex 时 `auto` 改为默认。`bot: false` 只在 Bot 运行于这个服务时关闭它。任务只经目标设备的 Bot 到达，所以这台设备没有 Bot 时，`execution: true` 且未给 `bot` 会让这个服务同时运行 Bot。`bot` 和 `execution` 只用于 `supports` 含对应用途的产品，`app_server` 只用于 Codex；`bin` 为空时恢复默认程序。`execution_model` 只写执行角色的设备默认模型，空字符串清除覆盖并回落 Provider 默认；`env` 中字符串设置变量，`null` 删除，值只写入本机私有文件。`bin` 和 `app_server.socket` 开头的 `~` 按本机主目录展开；`CODEX_HOME` 由 Node 固定为 Node-owned 根。`connect_local:true` 只对 Codex、Claude Code、Cursor 补齐本机程序、私有配置和所需环境变量，不改变用途，返回完整 `node_settings` 形状；尚未启用的服务也可准备。开启用途时自动走同一补缺。Cursor 只使用官方 CLI 订阅登录，拒绝 CURSOR_API_KEY、CURSOR_AUTH_TOKEN 与 AGENT_CLI_CREDENTIAL_STORE 设置；自动接入会把不支持的 SDK Bridge 选择改为发现的官方 CLI，保留有效自定义 CLI。本次显式编辑的变量名含 `null` 不补回。已有私有值优先，其次 Node 环境，再按需查询受限交互登录 shell；来源异常返回固定安全错误。system 模式跳过私有接入，不发现个人会话或 socket。 |
| `provider_models` | `provider` | 读取共享模型目录 `ModelCatalog`，Bot 和执行任务复用。`available` 带模型和覆盖范围，`unavailable` 带原因；两种状态都保留默认和手填。Codex 经 App Server `model/list`，pi 经无会话 RPC 列出 |
| `provider_probe` | `provider` | 主动刷新该 Provider 的无 prompt 检查，返回完整 `ProviderStatus`；不调用模型或执行登录 |
| `local_access` | 可选 `enabled` | 查询或开关本机自动访问 |
| `invite` | `record` | 管理设备登记签名邀请，返回 `join_id` 和本 Node 的节点链接 `entry` |
| `share_link` | 无 | 返回 `{link}`，本 Node 当前的节点链接 `vibeface://node/<设备编号>?addr=…&relay=…` |
| `join_check` | `secret` | 打开链接时询问入口还收不收它，回 `invite` 为 `open`、`used` 或 `expired`，不占用链接。必须持有 secret 才能问。没登记过的 secret 与过期的回答相同，回答也不超出 `join` 拒绝时给的信息。本设备已提交且仍在等待的请求回 `open` |
| `join` | `secret`、`public_key`、`kind`、`name`、`commit` | 新设备提交加入请求，`public_key` 必须是本通道设备 |
| `join_status` | 可选 `join_id`、`reveal` | 新设备查询或揭示随机数，请求已有 `admin_id` 时另带该管理设备的名字 `admin_name`；不带 `join_id` 时管理设备列出所有入口上的待处理请求 |
| `join_withdraw` | `join_id` | 新设备撤回自己仍在等待的加入请求，只认提交该请求的设备；请求已结束时原样返回当前状态。网页和手机点“取消”时经当前通道发一次，不重试，回复丢失时请求按期过期 |
| `approve` | `join_id` 加 `admin_nonce`、`event` 或 `reject: true` | 管理设备推进或拒绝加入，请求在其他入口时转发过去 |
| `grant` | `event` | 管理设备签名改已加入设备的名称或客户端的 `manage`，`kind` 须与现有成员一致，`epoch` 须是生效 epoch，之后须仍有管理设备；新设备只经 `approve` 加入。同一事件重发不新增记录 |
| `revoke` | `event` | 管理设备签名撤销成员，或客户端成员签名撤销自己，`epoch` 须是生效 epoch，撤销后须仍有管理设备 |
| `veto` | `event` | 管理设备签名否决正在等待的找回：签发者是本通道设备，`epoch` 是生效 epoch，`at_ms` 与 Node 时间相差不超过 10 分钟，目标须是当前 `pending`，且离生效至少还有 10 分钟；同一事件重复提交返回原结果 |
| `approve_recovery` | `event` | 管理设备签名确认当前等待的找回并立即生效：`epoch` 是生效 epoch，`at_ms` 不晚于 Node 当前时间且相差不超过 10 分钟，并早于生效时间，目标须是当前 `pending`。确认确实改变生效 epoch 后才安装，返回 `state`、`view` 和 `now_ms`，即使新目录移除了签发者也返回这份签名结果。同一事件重复提交不新增记录 |
| `recovery_state` | `expires_at_ms`、`signature`、`checkpoints`、`events` | 恢复密钥以绑定本通道设备的两分钟签名读取当前目录 `state` 和本机各作者主分支的头 `heads`；先合并恢复文件带来的 `checkpoints` 和 `events`；新 checkpoint 据此为没有保留的 Node 写 `retired`，并从 `state` 取签名 epoch 的管理事件 heads 写入 `heads` |
| `recover` | `genesis`、`checkpoints`、`events`、`capability` | `checkpoints` 是完整链，最后一个是新 checkpoint，`events` 是恢复文件里的管理事件。空 Node 凭 `capability` 安装整条链和全部事件；已建网 Node 只取指向所带 checkpoint 的 `veto`，`approve` 和其余事件丢弃，最后一个 checkpoint 本机已有时什么也不改，否则合并后安装它。新的换目录 checkpoint 的 `issued_at_ms` 须在 Node 时间 10 分钟内（太早回 `this recovery was signed too long ago; start it again`，太晚回时钟偏差）；本机没有的旧 checkpoint 已作废的直接放过，仍会生效的同样检查 |

`ProviderStatus.setup` 为 `{directory, command}` 或 `null`，只含非敏感私有目录和可复制登录命令。Codex spawn、Claude Code 和 OpenCode 提供入口，Web 和 Mobile 保留复制与检查可用性。Codex system 共享外部服务的会话且 `setup=null`。pi 只隔离会话目录并保留本机认证，`setup=null`。

未加入的设备只能调用 `info`、`authenticate`、`join_check`、`join`、`join_status`、`join_withdraw`、`recovery_state`、`recover`，以及本机页面使用的 `node_join_check`、`node_join`、`node_join_cancel` 和 `rejoin`。`node_join_check` 带一次性本机授权 `capability` 和 `link`，由这台 Node 经 `vibeface/join/1` 向邀请所在 Node 发 `join_check`，转回其回答；其他方法要求已验证的成员或本机会话。`recovery_state` 和 `recover` 是根恢复的专用准入：前者要求当前恢复密钥对 `{device_id, expires_at_ms, network_id}` 的签名，`device_id` 为本通道设备，有效期最长两分钟；后者在安装前验证新 `RootCheckpoint` 由当前恢复密钥签名且 epoch 加一，验证前没有副作用。签名来自已被替换的恢复密钥时两者都回 `this recovery key was replaced by a newer recovery`，有找回在等时回 `another recovery is waiting to take effect`，来自被否决的找回时回 `one of your devices stopped this recovery`。所有管理浏览器都丢失时，新浏览器凭恢复文件经这两个方法找回，48 小时后得到管理权限，等待期内它只是未加入的设备。`authenticate`、`directory` 和 `recovery_state` 的回复带 `now_ms`，回复里的视图按这个时间算出，浏览器和原生客户端用它复算核对（WASM 与 FFI 的 `verify_membership` 都接受 `now`），签否决或确认时也用 Node 时间。到点或迟到的否决改变生效目录时，Node 重新安装一次签名状态，重算投影并作废邀请；定时任务每次最多睡 15 分钟，醒来按墙钟重算。每个请求都重新查成员目录；目录变化时，已订阅或正在观察进度的会话重新核对，设备被移除后会话关闭。`confirm_writable`、`diagnostics`、`logs`、`node_settings`、`provider_models`、`provider_probe`、`configure_node`、`configure_provider`、`test_codex_app_server`、`triage_settings`、`configure_triage`、`test_webhook`、`create_decision_token`、`revoke_decision_token`、`relay_settings`、`configure_relay` 和 `share_link` 另要求管理设备或本机会话（`test_codex_app_server` 到 `revoke_decision_token` 的参数见 [Agent 直连 App Server](design/agent-direct-app-server.md)）。本机维护接口上的所有者在 Node 尚未加入个人网络时也能调用不属于某个网络的方法，例如 relay 设置和 `share_link`，公共节点和 bootstrap 节点因此不必建网；`confirm_writable`、`diagnostics` 这类方法仍回 `personal network is not installed`，`invite`、`approve`、`approve_recovery` 和 `veto` 要求管理设备，`revoke` 要求管理设备或撤销自己的客户端成员。

加入使用承诺加短码。管理设备生成 ticket（入口 Node 在 `invite` 应答里给出的节点链接和一次性 secret，节点链接含最多 4 个直连地址和 home relay，见[两种链接](design/mesh.md#两种链接)），以 `vibeface://join/vibeface1…` 链接或同一链接的二维码交给新设备；Node 加入时先把票据里的地址和 relay 放进地址簿，入口的 relay 拿到邀请凭证，再查全网和配置的 relay，所以关闭全网或 bootstrap 节点连不上时也能连到入口；Core、网页、桌面端和手机 App 只接受这种链接和单独的 `vibeface1…` 票据。桌面端和手机 App 注册 `vibeface` scheme 打开链接，浏览器由用户粘贴。浏览器经本页的 `/api/client`、手机 App 经官方页面的 `/api/client` 连接，由该 Gateway 按设备编号转发，设备身份仍以 ticket 中的设备公钥核验。新设备提交 `commit = SHA256(public_key ‖ joiner_nonce)`，管理设备随后给出 `admin_nonce`，新设备再揭示 `joiner_nonce`，揭示值与承诺不符时请求作废。两端用双方公钥和随机数各自算出 6 位短码，本人核对一致后管理设备签名 `grant`。加入请求的视图带 `commit`。管理设备在发出 `admin_nonce` 前记下所见请求的设备公钥、`invite_id` 和 `commit`，揭示后自己核对承诺；新设备在揭示前记下 `network_id`、`admin_id` 和 `admin_nonce`。短码只用这些自己固定的值计算，入口之后改动任何一项，管理端不显示短码，新设备停止加入。ticket 默认 10 分钟过期，只能被一个加入请求消耗。

浏览器代本人写入时，`InputPosted`、`BotCreated`、网页删除的 `BotDeleted`、`ConversationOwner`、本人结清的 `WorkFinished`、`NotificationAcked`、`PrimaryBotSet`、`BotProjectSet`、`BotAvatarSet` 和 `ScheduleControlled` 的 body 带 `attestation { device_id, signature }`；回答提问的 `MessagePosted(user, answers)` 签名可选，Grok 经 decision token 的回答由 Node 无签名写入。签名输入为 `vibeface:message:v1:attestation:` 加 `SHA256(canonical_json({network_id, scope_id, body}))` 的十六进制，body 去掉 `attestation`，所以签名绑定网络、会话、事件类型和业务 ID。签名设备必须是未撤销的客户端成员。除回答外，Node 和协调 Agent 不能代写这些事件。

恢复包外层为 `{ version, network_id, envelope }`，加密正文为 `{ version, network_id, root_secret_key, genesis, checkpoints }`，`root_secret_key` 是当前恢复私钥，使用 Argon2id 与 XChaCha20-Poly1305。口令和明文恢复私钥不发给 Node。根恢复默认保留现有管理设备。根恢复和换恢复密码都先下载新恢复文件再提交，完成后旧文件失效。业务正文和项目文件不在恢复包中。

协调工具入口只接受本机 SDK 调用期间签发的短期 bearer token，权限固定到一个个人网络和业务会话，不能添加或撤销设备或修改后续授权。

## RPC 方法

`rpc` 的 `params` 是 JSON 对象，`method` 选择业务方法，其余字段与 `method` 同级。已验证的客户端确定 `network_id`。

| method | 主要字段 | 结果或行为 |
|---|---|---|
| `nodes` | 无 | 成员 Node、角色、在线状态、`max_seq`、`head_hash` 和能力，已撤销的 Node 给出撤销上限；同步正在失败的 Node 直接记为离线，不等待探测。离线 Node 的 `capabilities` 是它最后一次回答的能力，只存在本 Node 自己的存储里，不进共享记录，重启后仍在；从没回答过的为 `null` |
| `bots` | 无 | 当前网络的机器人定义，名称和说明已合并聊天中的修改；`avatar` 为 `bot_profiles` 里按 rank 合并聊天修改和 `BotAvatarSet` 的结果，`null` 表示没挑过，客户端按 bot ID 派生；`avatar_version` 是最后一次写长相的事件 hash 前 16 位，从没改过时为 `null`；`primary` 为主 Bot 的会话 ID；`successor` 为删掉主 Bot 后接替它的会话 ID，没有其他 Bot 时为 `null` |
| `set_primary_bot` | `conversation_id`、`attestation` | 本人选定主 Bot，写入 `bots` scope |
| `set_bot_project` | `conversation_id`、`project`（`{ target_node, cwd }` 或 `null`）、`attestation` | 本人换掉或清空机器人的项目文件夹，在它的会话写 `BotProjectSet`（签名事件体 `{"type":"bot_project_set", project}`），投影进 `bot_profiles` 的 `project` 字段，不建表、不改数据纪元；与当前相同时不写 |
| `set_bot_avatar` | `conversation_id`、`version`（字符串或 `null`）、`avatar`（`{ outline, eyes, hue }` 或 `null`），两项必填；`attestation` | 本人给机器人挑长相或恢复默认，`version` 是本人看到的 `avatar_version`（`bots` 里的值）。当前已是 `avatar` 时不写并返回当前 bot；否则当前版本不等于 `version` 时拒绝（`the bot's face changed since you saw it; nothing was written`），客户端删掉这条待写、刷新、不重发；相等时在它的会话写 `BotAvatarSet`（签名事件体 `{"type":"bot_avatar_set", version, avatar}`），投影进 `bot_profiles` 的 `avatar` 字段，与聊天修改按 rank 后写者胜。任何修改都换新版本，改回原长相也一样，所以重放不会盖掉之后的修改。比较只在接收写入的 Node 上做，读取与追加在同一把存储锁内。返回 `{ bot }`，含新的 `avatar_version` |
| `create_bot` | `bot_id`、`conversation_id`、`name`、`instructions`、可选 `project`、`attestation` | 创建机器人和它的会话，完整请求精确去重；与现有 Bot 重名（忽略大小写和首尾空格）时拒绝 |
| `delete_bot` | `conversation_id`、`attestation` | 本人删除机器人，在它的会话写 `BotDeleted`；已删除时不写。本机停下它正在跑和排队的回合，并中断它派到本机的工作，其他设备收到删除后各自停下自己的。返回 `{ written, primary }`，`primary` 为删除后的主 Bot 会话 ID |
| `control_schedule` | `conversation_id`、`schedule_id`、`action`（`pause`、`resume` 或 `delete`）、`attestation` | 本人暂停、恢复或删除一个 schedule，写 `ScheduleControlled`；已是该状态时不写，返回 `{ written }` |
| `control_memory` | `conversation_id`、`memory_id`、`base`、`after`、`attestation`、`operation_id` | 本人新增、接管编辑或删除记忆。签名体为 `{type:"memory_controlled", memory_id, base, after}`。返回 `{state, operation_id, current}`，state 为 applied、already_recorded、superseded 或 conflict。先核验签名和操作身份，再查 receipt，未见才在同一事务比较本机 base 并追加；conflict 不写入 |
| `memory_state` | `conversation_id`、`memory_id`、可选 `operation_id` | 返回 `{current, receipt}`。current 为含墓碑的 `{memory_id, text, metadata}` 或 null。未指定操作时 receipt=null，否则为 recorded、superseded 或 not_seen_here。not_seen_here 只表示入口未见，不是全网未执行 |
| `conversations` | 无 | 本地已知会话 |
| `create_conversation` | `conversation_id`、`title`、首个负责人 `scheduler_node` 及其签名 `owner_attestation` | 以稳定 ID 创建会话 |
| `conversation` | `conversation_id` | 会话（含负责人与 `owner_epoch`）、机器人、消息、`works`（DAG 投影，含 `state`）、`deliveries`（本机还在转交的派发和停止请求）、`changes`、`bot_settings`、`sent`（本 bot 发给其他 bot 的消息 `{message_id, to, text, created_at_ms}`，`to` 是对方会话）、`checks`、`questions`（本会话的提问及回答）、`sessions`（归给本 Bot 的外来会话，含 `muted`）、`local_node`、调度队列和 `observation_targets`（本次观察进度的设备）。负责人是其他设备时，入口限时向它读取 `scheduler_jobs`，带 `source_node` 合并进调度队列，只用于显示、不写本地；`scheduler.remote` 为 `{owner, reachable}`，联系不上时 `reachable: false` |
| `open_works` | 无 | 全网仍要本人看的工作 `{works}`：没有结果或结果未知的工作；每个会话最新结束的工作失败或被拒、且还没跟进的那一项；以及本机发出、被拒且还没跟进的派发。跟进指之后又有工作结束、本人在它结束后发的消息有了完成的回复、它的汇报回合回复完成，或本人在待处理关掉它。只在 SQL 里选出这些工作，不读整段历史。派发项的 `state` 为 `dispatch_rejected`，没有 `outcome`。只有本人客户端能调用 |
| `workspace_overview` | 可选 `conversation_id`、`limit`（1 至 256，默认 256）、`include_finished`（默认 `false`）、`include_sessions`（默认 `true`）；不支持 `cursor` | 本机所知事实的有界只读汇总，只给本人客户端：`{schema: 1, generated_at_ms, network_epoch, heads, consistency, truncated, conversations, bots, tasks, questions, checks, jobs, sessions, deliveries, relations, coverage}`。只含未删除 Bot 的会话。不带 `include_finished` 时 `tasks` 只取 `open_works` 那部分工作，已结束的只按 SQL 计数进 `coverage.omitted_finished`；本机还没被接收、被拒（仍未跟进）或结果未知的派发，以及已接收但工作还没同步来的派发（`accepted_waiting_sync`）也列进 `tasks` 和 `deliveries`。各列表超过 `limit` 时截断，`truncated` 为 `true`，`coverage.omitted_*` 给出截掉的条数。负责人是其他设备的会话不去问它，列进 `coverage.remote_owners`，此时 `consistency` 为 `partial` |
| `message` | `conversation_id`、`message_id`、`target`、`intent`、`sequence`、`text`、`attestation`，Task 目标还须 `expected_revision`，可选 `attachments`、`report_results` | 保存绑定目标的本人输入并返回调度回执；带附件时 `text` 可以为空。字段和回执约束见下文可靠调度回执 |
| `set_owner` | `conversation_id`、`scheduler_node`、`epoch`、`attestation` | 换会话负责人，`epoch` 必须等于当前 `owner_epoch + 1` |
| `cancel_manager` | `conversation_id`、`trigger` | 请求停止指定管理轮，不停止已派发的工作；排队中的回合记为已停止并写一条 `stopped` 回复。本机不持有该回合时转给会话负责人，超时返回 `{cancel_requested: false, unconfirmed: true, owner}`，不重发，客户端重读会话对账 |
| `reply_unanswered` | `conversation_id`、`message_ids` | 本机作为负责人，为其中仍未回复的消息排队回复，返回 `{receipts}`；重试不会再排 |
| `dismiss_unanswered` | `conversation_id`、`message_ids` | 本机作为负责人，把其中仍未回复的消息记为开始前已取消，不再列出也不执行，返回 `{receipts}`；重试不会再记 |
| `interrupt_work` | `conversation_id`、`work_id` | 中断工作及它委派出的工作；目标在其他 Node 时由入口转发和重试 |
| `codex_state` | 可选 `node` | App Server 状态摘要 `{connection, threads: [{thread_id, name, status, cwd, plan, reasoning, token_usage}], rate_limits}`；网页不调用 App Server 方法 |
| `questions` | 无 | 全网未回答的提问：`{questions: [{question_id, severity, text, choices: [{label}], about, conversation_id, bot, author, created_at_ms, answer}]}` |
| `answer` | `question_id`、`text`、`attestation` | 写该提问唯一的回答消息 `a-<question_id>`，签名作用域是提问所在会话。会话里的回答按 `answer` 触发准入，或转交调度 Node。返回 `{status, question_id, message, receipt}`；已有不同回答时 `status = "already_answered"`，返回已有回答 |
| `resolve` | `conversation_id`、`check_id`、工作项另带 `outcome` 与可选备注 | 本人一键结清：管理进程确认已停止；工作项写一条带本人签名的 `WorkFinished`。不重做原触发 |
| `upload_attachment` | `conversation_id`、`hash`、`size`、`offset`、`data` | 按 1 MiB 分块上传附件，返回已落盘的偏移 |
| `read_artifact` | `conversation_id`、`blob_hash`、可选 `offset`、`limit`、`binary` | 分页读取该会话实际引用的附件 |
| `notifications` | `client_id` | 该客户端尚未确认的通知 |
| `ack` | `notification_id`、`client_id`、`attestation` | 写入 `NotificationAcked` |
| `dismiss_work` | `conversation_id`、`work_id`、`attestation` | 本人从待处理关掉一个失败、被拒或未接收的工作：在该会话写 `NotificationAcked { notification_id: work_id, client_id: "inbox" }`（签名事件体 `{"type":"notification_acked", notification_id, client_id}`），不建表；已关掉时不写，重试只读回。工作没有失败时拒绝。返回 `{ dismissed: true }` |

`bot_id`、`message_id`、`conversation_id` 和 `call_ref` 标识业务写入。重试保留原身份和内容，同 ID 换内容会被拒绝。协调 Agent 的 token 只能调用 `nodes`。

`read_artifact` 默认按 UTF-8 文本分页，返回 `{ blob_hash, text, offset, next_offset, size }`，`limit` 为 4 至 65536 字节，偏移必须位于字符边界。`binary: true` 时每页 512 KiB，只读取请求的区间，返回 `{ blob_hash, offset, size, next_offset, data }`，`data` 为 base64。`next_offset` 为 `null` 时结束。浏览器拼好全部字节后用 SHA-256 与 `blob_hash` 比对，一致才保存文件。附件随引用它的消息由同步按哈希拉到其他成员 Node，还没拉到的入口读取失败，稍后再试。

`changes` 按 rank 列出本会话的 bot 修改，每项为 `{ change_id, conversation_id, cause, event_hash, created_at_ms, change }`，`conversation_id` 是被修改的 bot 的会话，`cause` 为 `{ conversation_id, message_id }` 或 `null`（bot 自己定的提醒），`change` 即 `BotChanged.change`。`overwritten` 列出本会话里被另一台设备没看到就覆盖的修改，每项为 `{ change_id, field, provider?, by, value }`：`field` 为 `name`、`instructions`、`model`、`reasoning_effort`、`skills` 或 `avatar`。`model` 和 `reasoning_effort` 均带 `provider`，后者的 `value` 为 `null` 时使用设备默认。`by` 是当前值的写入设备，`value` 是当前值。它由已生效事件按 rank 现算，规则见 design/bot-config.md 第 3 节。`bot_settings` 为 `{ skills: [{ skill_id, name, description }], schedules: [{ schedule_id, name, prompt, first_run_at_ms, every_minutes?, enabled, depth?, next_run_at_ms }], memory: [{ memory_id, text, metadata }], memory_staged_count, memory_conflicts, memory_sync_conflicts }`，skill 是全网共用的库，schedules 只列本会话还会运行或已暂停的 schedule，暂停时 `next_run_at_ms` 为 `null`，没有 `every_minutes` 的是一次性提醒，`depth` 省略即 0。`messages` 中 `role=bot` 的是另一个 bot 发来的消息，带 `sender { conversation_id, name }`。

`works` 的每项包含 `work_id`、`conversation_id`、`target_node`、`trigger`、`dispatcher`、`kind`、`model`、`resolved`、`cwd`、`title`、`prompt`、`call_ref`、`continues`、`parent`、`thread`、`phase`、`summary`、`thread_id`、`turn_id`、`outcome`、`result_text`、`failure_reason`、`failure`，以及每台设备都从 DAG 推出的 `state`（`accepted`、`running`、`interrupting`、`finished` 或 `unverified`）。网页以 `title` 为任务名，`prompt` 作为“交代的事”显示在任务详情主区。

工作接纳先读取已有 `work_id` 并校验完整请求，只有新工作读取可变设置。停用或切换 Bot 后，相同请求仍返回原状态且不重新发送。任务调度在 claim 和会话身份计算之前选择冻结的 Provider 与模型。`resolved.model=null` 固定 Provider 默认语义，不再读取 Node 后来修改的默认值，也不保证 Provider 内部实际型号不变。

Memory 的 metadata 为 `{epoch, revision, control_revision, source_event_hash, origin, user_controlled, essential}`。`base` 为 `{epoch, version}`，version 为 null 或 `{revision, control_revision, source_event_hash}`，删除必须有 version。epoch 必须为当前 1。用户 operation_id 使用 `SHA256(attestation 签名输入 ‖ 签名设备)`，入口重算后核对，签名字节和 Node 外层事件不参与。本人操作计数为 base 计数加一，空 base 为 1。自动计数固定 0，revision 为完整事件 hash；用户 revision 为 operation_id。CAS 比较 epoch/revision/control_revision，source 另验真实等价来源。相同操作的多个封装以最小 hash 作为来源代表，不改变逻辑版本。

`memory_sync_conflicts` 为 `{items, has_more}`，最多 50 项。每项为 `{memory_id, losing_revision, winning_revision, losing_event_hash, winning_event_hash, reason}`，reason 为 user_protected、concurrent_automatic 或 concurrent_user。该页从入口 effective 历史的逻辑 base 链计算，同一本人操作先去重，赢家正常祖先排除，不包含候选正文。它与 owner 的本轮 `memory_conflicts` 独立，owner 不可达不影响入口共享冲突。

## 可靠调度回执

本人普通输入使用 `InputPosted`。`message` 请求必需携带 `target`、`intent`、目标内 `sequence`，Task 修改还核验 `expected_revision`。实际提交目标为 Bot Main、稳定业务 Task、已登记 Work、Routine 或频道 Task。界面的频道成员目标先由客户端创建并绑定稳定 Task，再以 channel_task 提交；message 不直接接受 channel_member。签名覆盖目标、意图、顺序、revision、正文、完整附件和汇报选项。审批回答仍使用绑定原 `question_id` 的签名 `MessagePosted`，不能由普通输入冒充。

连接身份绑定或会话权限检查失败仍返回 RPC 错误。进入 `message` 处理后的未保存拒绝返回 `{accepted:false, code:"input_not_saved", stored:false, message_id, reason}`。同一消息 ID 已有内容冲突时返回 `code:"input_conflict"` 和 `stored:true`，保留原记录。`accepted:true` 表示输入已保存，执行阶段仍以 `receipt` 为准。

`workspace_command` 持久化频道、任务、Assignment 和队列控制命令。相同 command ID 回读原结果，改变内容须换 ID。 ControlInput 在追加前明确因 CAS 或资格校验被拒绝时，返回 `{accepted:false, code:"workspace_command_not_saved", stored:false, command_id, reason}`。该证据只表示这次控制命令未保存，不授予重发输入的权限。客户端绑定确切控制 ID 后重新查询原输入；IO、提交或转发结果不明时保留原控制身份继续对账。`workspace_state` 与 `workspace_history` 是有界共享投影，保留截断说明和已核验发言身份。`scope_context` 返回确切目标的本机编译来源与省略数量，`input_receipt` 返回该消息的逐阶段证据。Main 快照只包含 Main 消息和私有 job 投影；`scope_jobs` 另供明确目标选择，不携带原始提示、错误正文或私密会话内容。 `input_receipt.control_event` 是确切输入控制事件的哈希，不可达时为 null。`retry_safe=true` 必须带有效控制引用，未知结果始终为 false。

调度 job 以 `(conversation_id, trigger)` 为键，`trigger` 为 `message:<message_id>`、`work:<work_id>`、`task:<work_id>`、`assignment:<assignment_id>`、`schedule:<schedule_id>:<slot_ms>` 或 `bot_message:<message_id>`，重试和同步都不会产生第二个 job。`message` 返回 `accepted`、`state`、`trigger`、`message_id` 和 `receipt`：

```json
{
	"network_id": "NETWORK_ID",
	"conversation_id": "conversation-id",
	"trigger": "message:message-id",
	"state": "queued"
}
```

会话负责人由 `ConversationOwner` 决定。入口不是负责人时，消息写入入口自己的日志并留在本机 outbox，回执 `state=pending_delivery`。入口通过持久 PeerEnvelope 短准入投递，远端先保存命令并返回 `saved`，缺失的事件由后台同步补齐，只有 `owner_accepted` 才结清入口 outbox。同一逻辑目标按 sequence 顺序投递，不同目标独立推进，同一远端设备的其他 scope 不等待慢请求。未知且没有保存证据的命令阻止同目标后项越过。首次发送前在 Store 内绑定目标 Node，可能已发送后只查询原身份，不自动转投另一 Node。客户端保存入口回执后继续回读逐条输入状态，历史确认接纳后才清理普通本地 outbox 项，拒绝或未知项继续保留。

远端只将明确的权限、原始意图、owner epoch 或取消等业务拒绝记为 `rejected`。存储、IO、队列及其他结果未明的错误保留原命令的 `saved` 或 `waiting_dependencies`，按同一 command ID 重试；已存在的 owner receipt、Work 或停止事实优先用于对账，回执保存失败也不会重新执行工作。

回合以 `owner_epoch` 为 fence。领取 job 时记下当时的 `owner_epoch`，之后工具调用的权限检查要求本机仍是负责人且 epoch 未变，换走再换回也算失效。旧负责人收到更高 epoch 的 `conversation_owner` 后立即拒绝该回合的后续工具写入，并走停止回复的取消路径结束它：取消得到确认时回复状态为已停止，未确认时记为中断并留待核对，不重做也不交给新负责人重放。Agent 任务在工作所在设备运行，不受负责人 fence。

新负责人从共享记录推导旧负责人留下的未回复消息，列在 `conversation` 调度队列的 `unanswered` 里，按会话顺序给出。未回复指本会话的本人消息没有 `status = completed` 的回复，没有 `message_steered` 记录，本机也没有它的 job 或 outbox 条目。只有本机是负责人且 bot 未删除时才列出。网页显示“N 条消息没等到回复”，本人选择回复后经 `reply_unanswered` 在本机排队，不会自动重放；选择“不回复”则经 `dismiss_unanswered` 在本机把它们记为开始前已取消，不执行任何操作。这条记录和队列一样只在本机：会话再换到第三台设备时，那里会重新列出，仍需本人决定。排队按 trigger 去重，重试不会再排，结果未知时重读会话即可对账。回复 ID 由 `(会话, trigger, owner_epoch)` 派生，所以新负责人的回复不会和旧负责人已写的停止或中断回复冲突。

每个会话的本机管理队列最多 256 个待处理触发，本人消息优先于工作结果、schedule 和 bot 消息。每轮按领取时固定的版本向量 `basis` 准备上下文。排在后面的消息因此能读到前一条的回复。提示总长不超过 50000 字节，bot 指令和 schedule prompt 各以 8 KB 封顶并计入总长。Codex 线程在 `thread/start` 和 `thread/resume` 的 `developerInstructions` 里收到 CLI 说明、初始工具目录和固定规则，回合消息只带本轮角色和数据：续接的线程只收到新消息、变化过的工作（`works_unchanged` 计其余条数）和变化过的 bot（未变时为 `{"unchanged":true}`），`trigger.unavailable_tools` 列出本轮不提供的工具。线程看过的 bot 和工作摘要随 `covers` 存在本机会话里，会话还记录 stable Prompt fingerprint 和 dynamic Context fingerprint。Bot、workspace、skill、工具目录或权限版本变化会使下一轮不能续接旧 native session；会话键含线程指令和 stable fingerprint 的摘要，指令变化就开新线程。模型仍按回合经 `turn/start` 传，但模型变化会改变 stable fingerprint 并开启新 generation。Cursor 每轮提示带 stable profile 和动态上下文，模型变化开新会话。超出时依次收缩工作列表、检查点和较早的消息，不让这一轮失败。工作列表和消息从新到旧取到各自预算为止，再按时间顺序给出；Memory 按 Store 的 changed_at_ms、memory_id 升序选入独立预算，各段均附省略条数。提示只读最新检查点之后（续接的会话则是它看过的 `covers` 之后）的事件；触发事件和它回答的问题按事件的 `kind` 与 `subject` 索引直接取，不随历史变长而多读。每条消息带 `author` 和 `seq`，触发事件只出现在 `trigger.event`，不在 `messages` 里重复；续接的会话不再收到本节点在该会话里正常完成的回复，停止、中断或失败的回复照常给出。回合开始前，提示要展示的附件不在本机时，向写这条消息的设备拉取，合计最多等 15 秒；仍没拉到的附件带 `pending_download: true`，触发事件另列 `pending_download` 哈希。之后每轮同步都向对端补拉本机仍缺的附件，缺失清单由已存事件引用的附件减去本机文件得出，不另存记录；对端没有某个附件时跳过它，继续拉其余的，下载按已落盘的偏移续传。

负责人每 30 秒检查本机负责的会话中已启用的 schedule，只取不晚于当前时间的最近一个时刻 `slot_ms`，错过的不补跑。运行前负责人写一条 `schedule_run_claimed` 领取这个时刻；时刻已过去超过 10 分钟（设备关机或休眠）时同样写领取，但不运行，只记一条 `schedule.skipped_late` 日志。本机只按已保存或已同步到的有效领取记录去重，重启、同步和换负责人后也一样，一次性提醒同样如此。schedule 定义、启停和负责人变更使用事件首次在本机落库的 `seen_at_ms`，不再使用写入方设备的墙钟，因此写入方时钟偏快或偏慢不会改变本机的首次运行和接管宽限。旧负责人在尚未收到换负责人记录时不会暂停定时；断网超过宽限期且领取记录尚未同步时，新旧负责人都可能执行同一时刻。同一 schedule 仍在排队时不领取下一个时刻。早于最近一次修改 5 分钟以上的时刻不运行，所以暂停后恢复不会补跑。新负责人不领取、不运行早于或等于换到它的 `conversation_owner` 记录首次在本机落库的 `seen_at_ms` 的时刻：旧负责人可能已在断开期间运行过它，领取还没传过来。换到它之后的 5 分钟宽限期内（`TAKEOVER_GRACE_MS`），新负责人也不领取任何时刻，留给还没看到换负责人的旧负责人把领取传过来；宽限期满时，最近一个没有领取的时刻照常领取并运行，只是晚一些。schedule 轮可以派发工作和设提醒，不提供修改 bot 的工具，回复的 `in_reply_to` 为该 trigger，由它派发的工作结果也回复这个 trigger。领取前 schedule 已暂停或删除，或这个时刻的领取胜者不是本机时，job 取消。换负责人时未开始的 schedule job 直接丢弃，由新负责人按自己的检查运行。

`bot_message:<message_id>` 由写入 `BotMessaged` 的发送方负责人接纳（对方会话也归它调度时），否则进 outbox，经 `deliver` 交给对方负责人。同步只存消息，不排 job。每个轮次有深度：本人消息和回答为 0，schedule 轮取其 `depth`，bot 消息轮取 `hops`，工作结果和 Agent 任务沿派发链继承；提醒和 bot 消息的深度为当前加一，超过 4 时拒绝。

job 状态为 `queued`、`steering`、`claimed`、`running`、`cancelling`、`reconciling`、`completed`、`failed`、`cancelled`、`interrupted` 或 `steered`，后五种是终态。普通继续输入解析到确切的活动 attempt，附件全部验证后先登记 `steering` 再调用 Provider。显式 Defer 不参与活动补充。同目标的旧输入和未准备附件阻止后续输入越过，不同目标的投递与回读独立运行。Provider 明确拒绝才回到 `queued`，超时或写入结果未知保留核对，不能直接重发。

逐条 receipt 分开保存 Provider 接受与实际原生回合归属。Codex 以稳定 `clientUserMessageId` 回读，Claude 以已登记 UUID 和持续输入组恢复，pi 与 OpenCode 读取各自原生记录。输入可能属于实际后续回合，必须记录其真实 turn。工具调用中的中间 assistant 消息不证明整个回答完成。只有原生证据证明输入及其终态才能结算为 `answered` 或 `unfinished`；仅有提交或接收不能显示模型已读。不可读与宿主结束后持续 Pending 分别计时，不能解释成一个固定的总等待期限。确认未到达才允许后续调度，持续无法核定则保留 `unknown`。

steer 状态只在本机调度日志保存，同步和恢复不重放输入。结算为 `answered` 时同事务写入 `message_steered`，避免新 owner 把它当未回复。当前 Cursor ACP 适配器没有提供原生文本 steer，不能声称活动补充已支持。

跨重启的 `running` 与 `cancelling` 进入 `reconciling` 并登记待核对，只查询原回合。Bot 回答与 job 终态在同一事务提交，`status` 为 `completed`、`failed`、`stopped` 或 `interrupted`，停止或中断保留已输出部分。`failed` 与 `interrupted` 带失败类别，原始错误留在本机 job 与 `node.log`，客户端按类别说明。

失败类别 `failure` 取 `missing_key`、`auth_failed`、`quota`、`missing_binary`、`execution_disabled`、`no_model`、`no_bot`、`writer_busy`、`rejected`、`transport`、`timed_out`、`restarted`、`lost` 或 `unknown`。本仓库自己的错误在产生处直接给出类别；Cursor ACP 与 Codex App Server 的上游错误只在 `crates/bridge/src/failure.rs` 一处识别，Codex 回合错误先看 `codexErrorInfo`。回复、工作结果（`WorkFinished.failure`）、本机 job（`failure {kind, detail}`）和被拒的派发（含 `deliveries` 与 `workspace_overview.tasks`）都带同一类别。目标设备没有 Bot 接任务时拒绝派发并给出 `no_bot`；客户端按类别判断能否到那台设备的设置里修复，不匹配原因文字。

```text
queued → claimed → running ─→ completed | failed
             │        └─ cancelling ─→ cancelled（Provider 确认终态）
             │                     └─→ reconciling（退出未确认，登记待核对）
             └ 重启时 running / cancelling ─→ reconciling（登记待核对）
reconciling ─ 查询原回合或本人确认后 → 终态
```

Node 按记下的 Provider、会话和回合标识读取原结果。只有 Provider 返回明确终态时才自动结清并保存对应结果；读不到或仍在进行时继续待核对，不能仅凭进程退出就认定已停止。`conversation.checks` 列出本会话尚未结清的管理轮，不按进程组是否存活过滤，也可由本人确认结清。结果未知的工作由 `works[].state == "unverified"` 表示，卡片 ID 为 `work:<work_id>`。同一工作只显示一条：`kind=task` 的工作结果未知时，目标上那一轮管理进程的待核对不再单独显示，结清工作项时一并结清。模型不能结清待核对。

Cursor 已发送的回合遇到 bridge 崩溃、断流或超时后记为 `reconciling` 并登记待核对，只退休该 Bot 的进程组。下轮等待旧进程组结束后才能启动新进程，其他 Bot 的回合继续。Node 不重发未知结果的提示，也不因本地进程结束把它改成成功或 Provider 已确认取消。

负责人每轮结束后，最新检查点之后的消息超过 20000 字节或事件超过 400 条时写一条 `ContextCheckpoint`，事件以归属和进度为主的会话也会写。检查点不覆盖的部分既不超过最新约 10000 字节的消息，也不超过最新 200 条事件。写检查点时只读取最新检查点之后、它的 `covers` 没有覆盖的事件，结果与读取整个会话相同。检查点是确定性摘录，不是模型写的总结。`decisions` 是不截断区，记本人对提问的回答和本人确认的工作结果，后续检查点原样继承，只在超过 200 条时为限制事件大小丢最早的；`summary` 是滚动区，只保留最新的 6000 字节。`recent_user_requests` 是被覆盖的最近 20 条本人消息摘录，只是历史，不代表需要长期遵守的指令。提示放不下时检查点依次让出滚动摘要、较早的本人消息摘录和未完成工作，`decisions` 最后才省略，并给出 `decisions_omitted`。

本机调度 job、outbox、工作状态日志和原生 SDK 会话不进入事件日志。

## 动态工具目录与上下文观测

Provider 初始只收到 `tool_search`、`tool_describe`、`tool_call`、`context`、`read_skill` 和 `call_lookup`。`tool_search { query?, offset?, limit? }` 返回本轮有权发现的领域工具及 `more/next`；`tool_describe { tool }` 返回原始 schema 和 effect；`tool_call { tool, arguments, call_ref? }` 进入同一领域 router，先解析实际目标再执行权限检查。目录不授予权限，也不能包装私有内部工具或再次嵌套发现工具。稳定指纹覆盖完整目录，未初始展示的 schema 变化同样会失效会话。

`conversation.scheduler.context` 是本机或可达 owner 报告的限长观测，只包含 stable/dynamic 指纹、session generation、各分段 coverage、字节数、Provider 和真实阶段。`compiled` 仅表示已编译，不表示已发送；实际 Provider 回合回调和终态才推进 `started/completed/failed/cancelled/interrupted/unknown`。owner 不可达时不以入口的 Provider 状态代替。`records.context` 更新本机观测，客户端保留远端 owner 的状态。`bot_settings.memory_conflicts` 和 `records.memory_conflicts` 仅含冲突 ID、epoch/revision 和原因，不含候选文本。

队列的 `lane` 按触发来源区分 `main/task/schedule_routine/bot_message`；main 和 bot_message 共用主会话，task 按 work_id、routine 按 trigger 隔离 session 和 worker。`hold_reason` 来自实际 claim gate，可为 `active_turn`、`steering_settlement`、`provider_serial`、`task_limit`、`machine_slot`、`pending_check` 或 `backend_config_pending`。pending check 只阻止与该未知结果共享执行边界的回合；不会自动重放写操作。`context.session_status` 为可选的 `{resumable,pending_generation}`，无可验证配置观察时省略，配置从下一回合生效。

## 工作

工作登记时会把发起方的模型覆盖和目标 Node 解析出的 Provider 与模型分别冻结到工作事件和投影。重试不会重新读取变化中的设置。远端任务必须省略显式模型，由目标 Bot 选择并冻结自己的模型；发送方带非空模型时在建立待投递记录前拒绝。

每台设备只有一个协调角色：协调 Agent 加 Node 运行时。协调 Agent 的 Shell 完全访问本机，用 `vibeface codex call|send|watch` 直接经 `VIBEFACE_CODEX_SOCKET` 调用本机 App Server。需要 Node 记录和观察的工作用协调工具 `dispatch_work { title, prompt, cwd, call_ref, agent?, target_node?, parent?, continues?, thread?, model?, effort?, images? }` 派发。`title` 是给本人看的几个字，最长 120 字符。`agent` 是本机 `providers.execution` 里的产品，可省略：给了 `thread` 时沿用本机工作在该会话里用过的产品，没有工作在其中跑过的会话按 Codex 线程处理；给了 `continues` 时沿用原工作的产品；都没有时用本机 Bot 自己的产品（它在其中时），否则用其中第一个；不在其中时以 `execution_disabled` 拒绝；`agent` 与会话或原工作的产品不符时，错误点名 `agent` 和原产品。`effort` 只有 Codex、Claude Code 和 pi 接收，`images` 只有 Codex、pi 和 opencode 接收；产品不接收时在登记前拒绝，什么都不留。登记后启动被拒时，`dispatch_work` 直接报错并带上原因，工作记为 `rejected`，要换新的 `call_ref` 重派。派发到其他设备时必须省略 `model`，由目标设备的 Bot 在接纳时冻结自己的模型；远端显式模型会在发送前拒绝。`work_id` 由 `(会话, trigger, call_ref)` 派生，同一轮重试同一 `call_ref` 先回读原工作，省略 `agent` 的重试沿用原工作的产品，不随执行开关变化；同一 `call_ref` 换了内容时拒绝，错误列出不同的字段；结果不明时用 `call_lookup` 查询。`hand_over_session { node, session_id, version, to? | stop_following? }` 把本 Bot 名下的外来会话转给 `to`（另一个 Bot 的会话 ID 或名称，给自己即重新关注），或 `stop_following: true` 不再关注；`version` 取自 `bot.sessions` 或 `list_sessions`，是最后改动该会话归属的事件 hash 前 16 位。只有会话当前所属的 Bot 能操作，运行该会话的设备写入；状态已成立时不写、重试只读回；版本已变时不写，错误里给出当前版本。返回 `{node, session_id, conversation_id, muted, version, changed}`。`propose_provider_config { execution?, app_server_socket?, codex_sessions?, bins? }` 提议本设备的路径，`execution` 列出要开启执行工作的产品：给出的必须是存在的绝对路径，其余由 Node 补齐，只留与现有设置不同的项；都相同时返回 `{status: "unchanged"}`。否则在本聊天写一张「允许 / 不允许」提问卡片，卡片里的路径缩写成 `~/…`，返回 `{status: "asked", question_id, proposed}`，此时什么都不写。本人回答后的那一轮，Bot 用 `apply_provider_config { question_id }`：回答为「允许」时经 `configure_provider` 的同一写入路径写入，返回 `{status: "applied", applied, also_filled}`；其他回答返回 `{status: "declined"}`，不写。同一提问重复调用只读回第一次的结果。提议只存在内存，Node 重启后要重新提议。密钥和其他变量不经这两个工具。`context { at? | before? | after?: {author, seq}, types?, author?, keyword?, limit?, blob_hash?, offset? }` 按本轮领取时的 `basis` 读历史，位置不随新事件移动：不给位置读最新的，`before` 或 `after` 读某条事件之前或之后的，`types` 按事件类型过滤，`author` 按作者筛选，`keyword` 按可见事件文本做字面子串搜索；筛选先于分页并受本轮 basis 限制。`more` 表示该方向还有匹配事件，`next` 给出下一页使用的 `before` 或 `after` 位置；一页最多 32000 字节，单个事件最多 8000 字节；`at` 只读该事件，把事件体的 JSON 文本从 `offset` 字节起放进 `body_json`，整个结果不超过 32000 字节，`next_offset` 指向下一页，各页拼接即完整事件体；位置超出本轮 `basis` 时报错。带 `blob_hash` 时读附件，`limit` 以 KiB 计、最多 32，UTF-8 文本以 `text` 返回并在字符边界分页，其他内容为 `data_base64`，附件还没到本机时返回 `pending_download: true`。`work_status { work_id? | before?, limit? }` 给出单个工作的完整记录（结果超过 32000 字节时截断并标 `result_truncated`），或从新到旧分页列出本会话的工作，`before` 接上一页最后一个 `work_id`。其他协调工具为 `nodes`、`context`、`work_status`、`interrupt_work`、`read_skill { skill_id }`、`recall { query? }`、`fetch_url { url }`、`list_bots {}`、`read_bot { bot }`、`list_sessions { before?, limit? }`（从新到旧分页列出本 Bot 名下的会话，带 `version`；还有更早的时返回 `next`，作为下一页的 `before`），以及 `dispatch_work` 内部使用的 `track_work`、`bind_work` 和 `wait_work`。`list_bots`、`read_bot`、`create_bot`、`call_lookup` 查到的新建 bot 和轮次里的 `bot.configured_bot` 给出 bot 实际显示的长相 `avatar { outline, eyes, hue, picked }`：没挑过时由 bot ID 按 `packages/client/src/avatar/face.ts` 的 `faceOf` 派生，`picked` 为 `false`；工具结果不带长相版本 `avatar_version`；两端的派生由 `packages/client/src/avatar/derived-faces.json` 的共用向量核对。`fetch_url` 只读 http 和 https，解析出的每个地址都必须是公网地址，连接固定到核对过的地址，不走代理，最多 5 次跳转并逐跳核对，15 秒超时，正文最多 2 MiB，返回 `{ status, final_url, content_type, title, text, truncated }`，文字最多 20000 字符。

唤醒工具 `remind { call_ref, note, in_minutes? | at_ms? }` 和 `message_bot { call_ref, bot, text }` 提供给除 Agent 任务和路由外的所有轮次。`remind` 写一个一次性 schedule（深度为当前加一，1 分钟至 366 天后）；`message_bot` 在对方会话写 `BotMessaged`，`message_id` 由 `(会话, trigger, call_ref)` 派生，每个 bot 每小时最多 10 条，正文最多 4000 字节。

修改 bot 的工具为 `update_profile { call_ref, name?, instructions?, avatar?, default_avatar? }`、`save_skill { call_ref, skill_id?, name, description, body }`、`remove_skill { call_ref, skill_id }`、`save_schedule { call_ref, schedule_id?, name, prompt, first_run_at_ms, every_minutes?, enabled? }`、`remove_schedule { call_ref, schedule_id }`、`remember { call_ref, memory_id?, text }`、`forget { call_ref, memory_id }`、`create_bot { call_ref, name, instructions?, project_node?, project_cwd? }`、`configure_bot { call_ref, bot, name?, instructions?, model?, skills?, all_skills?, avatar?, default_avatar? }` 和 `delete_bot { bot }`，只提供给本人消息触发的轮次。Node 再核对 trigger 是 `message:<id>` 且该消息是本会话的本人消息，`cause` 取自 trigger，`change_id` 由 `(会话, trigger, call_ref)` 派生，同一 `call_ref` 同内容返回原记录，换内容拒绝，`call_lookup` 也能查到修改。改前值取自投影，没有变化的修改被拒绝。参数不接受其他字段，AI 服务和文件夹只能在设置里改。`model` 为空表示跟随设备默认；`skills` 是启用的 skill ID 集合，必须都在库里，删除的 skill 从各 bot 的集合里去掉，`all_skills: true` 恢复全部，两者不能同时给。`avatar` 为 `{ outline?, eyes?, hue? }`，至少给一项，没给的项取这个 bot 当前显示的长相补全；`hue` 只能取 20、45、100、130、155、180、205、230、255、285、315、345；补全后与当前显示的长相相同时被拒绝。`default_avatar: true` 恢复由 bot ID 派生的长相（写 `null`），与 `avatar` 不能同时给。同一 `call_ref` 重试时，给出的长相各项与原记录的 `after` 相同即视为同内容。修改名称和说明需要会话有机器人；`create_bot` 和改名都不能与现有 Bot 重名（忽略大小写和首尾空格）；schedule 只能由创建它的会话修改。每个 bot 最多 20 个活动 schedule；本机新增或恢复记忆时以 50 条有效记忆为接纳上限，回合完成事务逐条重检容量。不同离线 Node 的合法新增合并后可能超过该数量，不为容量删除已签名事实。笔记每条最多 500 字符，像密钥的内容被拒绝。一条本人消息最多建 3 个 bot。`create_bot` 的 `bot_id` 和会话 ID 由 `(会话, trigger, call_ref)` 派生，Agent 只给名字。各协调工具的 `call_ref` 只作本次调用的重试键，须为 1-128 个 ASCII 字母、数字、`-`、`.` 或 `_`，不合规时错误点名 `call_ref`。`delete_bot` 在被删 bot 的会话写 `BotDeleted { cause }`，不能删除发起轮次的 bot 自己，重复删除不写新事件；网页由本人签名写 `BotDeleted { attestation }`，RPC 为 `delete_bot { conversation_id, attestation }`，返回 `{ written, primary }`。

Node 接纳工作时核对派发方是成员 Node 且处于进行中的调度轮次，并核对 `cwd` 位于自己的 `roots` 内。本机工作的 `kind` 是执行它的产品，由该产品的执行器开始、复查和中断。Codex 工作（`kind=codex`）：`track_work` 写 `WorkDispatched` 并返回 `{work_id, call_ref, state, may_send, send_by_ms, cwd, thread_id?}`，只有第一次登记且由 CLI 发送时 `may_send=true`；CLI 用 `thread/start`（`approvalPolicy=never`、`danger-full-access`）开线程，`bind_work {work_id, report: {status: "thread", thread_id}}` 绑定后才发 `turn/start`，`clientUserMessageId` 为 `work_id`，再 `bind_work {work_id, report}` 回报 `{status: "turn", turn_id}`、`{status: "rejected", kind, detail}` 或 `{status: "unknown", reason}`；`unknown` 时 Node 先对账，不重发。`send_by_ms` 为登记后 60 秒，之后不再发送；再过 30 秒仍没有回合时，还没绑定线程的记为 `rejected`；已绑定线程的由 Node 按 `clientId` 复查，找不到就记为 `unverified`，之后回合出现仍照常跟进。`target_node` 是其他设备时，任务（`kind=task`）作为那台设备协调 Agent 的一轮执行，由它在本机直连派发。`continues` 在原工作的 thread 上开新 turn，原目标不可达时报错，不改派。`thread` 是本机已有的 App Server 线程（`vibeface codex send`）：CLI 先 `thread/read` 取 `cwd`；线程是 Codex 子 Agent（`source.subAgent`）时 Codex 不接受直接输入，CLI 在登记前拒绝并给出 `parentThreadId`，什么都不写；Node 登记时即为 `starting`，与 `continues` 互斥。发送前 Node 读取并规范化线程的真实目录，核对它位于本机 `roots` 内，并与显式 `cwd` 一致；回合进行中则 `turn/steer`，否则 `turn/start`。几项工作可共用一个回合，各自结束并各自回报。

Codex 默认 bypass。Node 订阅的线程上的服务端请求和不属于线程的请求由 Node 挂起并交给协调 Agent：Agent 用 `vibeface requests` 查看、`vibeface respond <request_id> '<result-json>'` 回答，需要本人时用 `vibeface triage raise --request-id … --question … --choice <标签>` 在负责该会话的 Bot 聊天里提问，提问挂住请求直到有回答。本人答案先持久化，再经私有 `HeldAnswerControl` 绑定原问题、请求、session、turn、transport generation 和响应 schema，回到原执行设备递交。基础审批按原请求动作递交，文字题由捕获配置的新协调会话整理。普通 Bot 在提问后不能代答。不属于任何 Bot 的会话先归给主 Bot 再按归属处理；没有 Bot、不属于线程或后台线程（`codex exec`、子代理）上的请求立即兜底。没有协调 Agent 或 120 秒内无人处理时按方法拒绝并记录（见 `packages/docs/codex-app-server.md`）。其他客户端的请求留给原客户端。hook 也能挂起请求：事件带 `hold`（`permission`、`question` 或 `continue`）时 Node 同样挂起它并唤醒协调 Agent，`/api/hooks` 的响应给出 `request_id`，hook 随后经本机 `POST /api/hooks/wait {request_id}` 等待回答；hook 的请求不因提问延长，30 秒内没有回答时返回 `{"decision":null}`，交回会话自己处理，不当作拒绝。Node 不自动重试写请求。请求 ID 由方法、线程、回合、`itemId` 或 `callId` 及 `approvalId` 的哈希确定，缺少这些字段时随机生成。重连后同一请求再次到达时沿用原有提问或 Agent 事件，只改用新连接的 JSON-RPC ID，不会再提问一次。连接后 App Server 读完 Node 订阅的线程，本设备提过问、仍未回答但请求未再到达的提问由本设备写 `question_closed` 关闭；之后同一请求再到达时按方法兜底。

工作状态为 `accepted`、`starting`、`running`、`interrupting`、`finished` 和 `unverified`。每次转换在同一事务更新本机日志并追加对应事件。`WorkFinished.outcome` 为 `completed`、`failed`、`interrupted`、`rejected`（从未启动）或 `unverified`；只有 `unverified` 可以被本人签名的 `WorkFinished` 取代。`unverified` 由目标自己写，或由会话当时的负责人为已被移除的目标写入；入库时按因果上的负责人校验非目标作者，目标自己的结果和本人签名的结清都优先于它。工作结束时若原消息的 `report_results` 不为 `false`，负责人把 `work:<work_id>` 入队，下一轮向本人汇报。

`providers.codex_app_server.mode` 为 `spawn` 时 Node 托管 App Server：执行 `codex app-server --listen unix://<socket>`，socket 位于 0700 的 `/tmp/vibeface-<identity>/` 目录，App Server 放入独立进程组，进程组号写入 `codex-app-server.pid`。启动时先结束上次遗留且命令相符的进程组，退出时 SIGTERM，3 秒后 SIGKILL，异常退出按 1 至 30 秒退避重启。App Server 每次重连 epoch 加一。重连后对未结束的工作调用 `thread/read` 只读复查，读不到原 turn 时进入 `unverified`。运行中和可复查的 `unverified` 工作每分钟也这样复查一次，补上漏掉的结束事件；复查只读，不重发。同一用户的本机进程可以连接这个 socket，协调 Agent 经 `VIBEFACE_CODEX_SOCKET` 得到它。

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

每台 Node 在网络中只有一条日志：`seq` 从 1 连续递增，`prev` 为同一作者 `seq - 1` 的 hash。`deps` 引用写入时依据的其他作者事件。`created_at_ms` 用于展示，排序时只在 `lamport` 相同时作为第二键。SQLite 的 `events` 行另存本机首次落库时间 `seen_at_ms`；它不进入签名 envelope、不随事件同步，投影重放时复用原值。收到的事件 `created_at_ms` 与 `seen_at_ms` 相差超过 5 分钟时，Node 记一条 `event.clock_skew` 警告日志，只用于排查写入方时钟，不影响入库。浏览器不写日志，它发起的写入由所连 Node 作为作者写入并附 `attestation`。

| 类型 | 主要字段 | 作者 |
|---|---|---|
| `conversation_created` | `title` | 入口 Node |
| `conversation_owner` | `scheduler_node`、`epoch`、`attestation?`、`cause?` | 入口 Node；bot 建 bot 时由负责人写，带 `cause` 不带签名 |
| `bot_created` | `bot_id`、`name`、`instructions`、`project?`、`attestation?`、`cause?` | 同上 |
| `bot_messaged` | `message_id`、`from { conversation_id, name }`、`text`、`hops` | 发送方会话的负责人，写在对方会话 |
| `schedule_controlled` | `schedule_id`、`action`、`attestation` | 入口 Node |
| `bot_project_set` | `project`、`attestation` | 入口 Node，写在该 bot 的会话 |
| `bot_avatar_set` | `version`、`avatar`、`attestation` | 入口 Node，写在该 bot 的会话；`version` 为 `null` 或 16 位十六进制，签名覆盖 `version`，投影只用 `avatar` |
| `message_steered` | `message_id`、`reply` | steer 这条本人消息的 Node，在宿主回复 `reply` 完成且消息并入其中时写 |
| `input_posted` | `message_id`、`target`、`intent`、`sequence`、`expected_revision?`、`text`、`attachments`（空时省略）、`report_results?`、`attestation?` | 入口 Node 封装本人签名的普通输入 |
| `workspace_commanded` | `command_id`、`command`、`acting_assignment?`、`attestation?`、`cause?` | 命令写入 Node；按具体命令核验本人签名、Bot 因果来源或执行 Node 身份 |
| `message_posted` | `message_id`、`role`、`text`、`attachments`、`report_results?`、`in_reply_to?`、`status?`、`failure?`、`answers?`、`attestation?` | 本人对原问题的答案（含普通问题与审批）由接收答案的 Node 写，`answers` 绑定原问题，`message_id` 为 `a-<question_id>`；assistant 回复由负责人写 |
| `work_dispatched` | `work_id`、`trigger`、`dispatcher`、`call_ref`、`kind`（执行它的产品名，或 `task`）、`model?`、`resolved {provider, model}`、`cwd`、`title`、`prompt`、`continues?`、`parent?`、`thread?` | 目标 Node；`resolved` 必填，其 `model` 为 `null` 时跟随 Provider 默认 |
| `work_progress` | `work_id`、`phase`、`summary`、`thread_id?`、`turn_id?` | 目标 Node |
| `work_finished` | `work_id`、`outcome`、`result_text`、`failure_reason?`、`failure?`、`attestation?` | 目标 Node，本人结清时由入口写 |
| `notification_acked` | `notification_id`、`client_id`、`attestation` | 入口 Node |
| `question_asked` | `question_id`、`target?`、`severity`、`text`、`choices: [{label, action?}]`、`about {node, agent, session_id, cwd, summary, request_id?}`、`attention {kind, device_id}`（必填） | 提问的 Node；写在拥有该会话的 Bot 会话；原生审批动作仅为 `allow_once` 或 `deny` |
| `question_closed` | `question_id`、`reason`（`request_lost`） | 提问且挂着该请求的 Node，写在提问所在会话；其他作者的关闭不计入。已有回答时回答为准，与到达顺序无关；关闭后回答被拒绝 |
| `session_attributed` | `node`、`agent`、`session_id`、`cwd`、`summary` | 运行该会话的 Node，写在接手 Bot 的会话；rank 最大者为准 |
| `session_muted` | `node`、`session_id` | 运行该会话的 Node，写在当前接手 Bot 的会话：不再关注，新的 `session_attributed` 撤销 |
| `bot_changed` | `change_id`、`cause`、`change` | 负责人 |
| `memory_controlled` | `memory_id`、`base`、`after`、`attestation` | 本人客户端签名，入口 Node 封装；不运行模型 |
| `context_checkpoint` | `covers`、`summary`、`recent_user_requests`、`open_work`、`decisions` | 负责人 |
| `schedule_run_claimed` | `schedule_id`、`slot_ms`、`owner_epoch` | 负责人，写在 schedule 所在会话 |

`question_asked.attention` 必填。`kind` 为 `question`、`approval` 或 `browser_takeover`。`device_id` 可为 `null`，非空时必须等于 `about.node`；`browser_takeover` 必须提供该运行设备。卡片类型不授予额外权限。

`question_asked.target` 记录问题的输入目标。外部 Agent 会话没有执行绑定时，只有运行设备签名的 `session_attributed` 能证明该会话归属当前 Bot，问题及其答案才属于该 Bot 的主会话；无法证明归属时保留未知目标，不作为主会话输入。

`bot_changed.change` 按 `kind` 区分：`profile { name?, instructions?, model?, reasoning_effort?, skills?, avatar? }` 的每项为 `{ before, after }`，`avatar` 的两端为 `{ outline, eyes, hue }` 或 `null`（由 bot ID 派生）；`skill { skill_id, before, after }`、`schedule { schedule_id, before, after }` 与 `memory { memory_id, before, after, base, origin, essential }` 的 `after` 为 `null` 表示删除。skill 为 `{ name, description, body }`，名称最多 64 字符，说明最多 512 字节，正文最多 16 KiB；schedule 为 `{ name, prompt, first_run_at_ms, every_minutes?, enabled, depth? }`，名称最多 120 字符，说明最多 8 KiB，`every_minutes` 为 5 至 40320，省略时只运行一次。说明（instructions）最多 8192 字节。这个事件不带 `attestation`。`cause` 为 `{ conversation_id, message_id }`，入库时要求 `prev` 或 `deps` 中有该会话、该 `message_id` 且带 `attestation` 的本人消息，否则拒绝；带 `cause` 的 `conversation_owner` 与 `bot_created` 按同一规则核对，`conversation_owner` 还必须是 epoch 1 且 `prev` 是同一会话的 `conversation_created`。没有 `cause` 的只能是新建的一次性 schedule，且 `depth` 至少为 1。其他配置投影比较 rank：名称、说明、模型、推理强度、skill 和长相逐项后写者胜并合并进机器人定义，长相与 `bot_avatar_set` 共用一项，skill 全网按 `skill_id`、schedule 按 `schedule_id`；Memory 按 network/conversation/memory_id 隔离，用户永久优先，用户间比较 control_revision 再比较 operation_id，自动间按 rank 后写者胜，删除保留墓碑，`schedule_controlled` 按 rank 改 `enabled` 或删除，修改记录按 `change_id` 取 rank 最小者。

自动 Memory origin 只接受 agent/review，不接受 user、import 或 user_controlled。所有非空 Memory base 必须指向 prev/deps 中已存、已验证的真实父事件，目标网络、会话、ID、epoch、revision 和 counter 一致，自动 before 还要等于该来源的值。入口把 source hash 加入直接依赖。导入不比较当前赢家，空 base 表示签名者未见版本，不检查封装事件全部祖先是否无此 ID。用户接管后永久保护，包含删除墓碑；未保护 Agent 并发仍 LWW，并发用户删除没有额外优先级。未知写先按 operation_id 读回，客户端保留原请求且不自动换 base 重签。

`context_checkpoint`、没有 `cause` 的 `bot_changed`（bot 自设提醒）、`schedule_run_claimed` 和 `bot_messaged` 只能由对应会话的负责人写：前三者看写入的会话，`bot_messaged` 看发送方会话 `from.conversation_id`，写 `bot_messaged` 时 `deps` 也包含发送方会话里各作者的最新事件。入库和本机写入都按作者写入时的认知核对：只看作者自己此前的事件和每个 `deps` 所在日志到该依赖为止的 `conversation_owner`，其中 epoch 最大的若有一条指名作者即通过，否则拒绝并记 `sync.event_rejected` 日志。`schedule_run_claimed` 的 `owner_epoch` 还必须等于这个最大 epoch。作者还没看到的换负责人不算违规；乱序到达的事件先等依赖入库再核对，所以各副本结论一致。换负责人后旧负责人结束回合时只写回复，不再写检查点。同一 `(会话, schedule_id, slot_ms)` 的领取投影为 `schedule_claims`，`owner_epoch` 大者胜，相同时作者较小者胜，与到达顺序无关。

通知由五类事件派生，通知 ID 即该事件 hash：Bot 回答 `message_posted(assistant)`（`status` 为 `completed` 时类型为 `manager_response`，其余为 `manager_attention`）、其他 Bot 发来的 `bot_messaged`（类型 `bot_message`，记在收件会话）、`work_finished`（类型 `work_finished`）、会话里的 `question_asked`（类型 `question`）和新的会话归属 `session_attributed`（类型 `session_attributed`）。会话归属投影 `session_owners` 来自 `work_progress.thread_id`（作者即设备）和 `session_attributed`：工作记录 rank 最小者胜，`session_attributed` rank 最大者胜并覆盖工作记录，`session_muted` 置 `muted`；所属 Bot 已删除的行读取时视为无主。Codex 回合内部的命令、diff、推理和工具输出不进入 DAG。

入库逐个检查，顺序为 hash、网络、事件形状与 `attestation`，重复，作者是当前成员或带上限的已撤销设备，作者签名，然后才看 `seq` 与 `prev` 的连续性和 `deps` 是否已入库。签名只在入库时验证一次，未通过验证的事件不会入库，也不会触发 `forked`。重复事件忽略；序号跳过的事件丢弃并从缺口重新拉取；`prev` 或 `deps` 还没入库的事件进入 `pending_events`，等它们入库后在同一事务内自动入库。`pending_events` 最多 10000 条，满了之后到达的事件这次丢弃，之后的同步轮次再拉。入库后才向本机客户端推送该事件的 `changed` 并执行它的后续动作，例如 `bot_deleted` 停止该 bot 的回合；还在等待的事件不推送。所有事件存在同一张 `events` 表：`prev` 是本机主分支头的为 `main=1`，同作者同序号的另一支为 `main=0`，作为分叉证据，不锁住会话。每行另存事件类型 `kind` 和它所指的消息、工作或提问 `subject`，按类型的读取（检查点、负责人记录、触发事件、`context` 的类型过滤）走索引。`log_heads` 保存每个作者主分支的 `max_seq` 和 `head_hash`，即本机版本向量。

`effective` 标记事件是否计入投影，只由签名证据决定：作者从分叉序号起两支都不计入；当前成员 Node 计入此前的主分支事件；被撤销设备计入不超过上限的事件，分叉后沿 `cutoff_hash` 往回的那一支也计入。依赖只决定顺序，不传递是否计入。成员目录变化或分叉证据到达时，同一事务内重新计算 `effective`，有变化时清空并按顺序重放 reducer 写的投影表，本机账本不变，不重放副作用，然后推送一条不带会话的 `changed`。

入库时计算 `lamport = 1 + max(prev, deps 的 lamport)`，读取顺序为 `(lamport, created_at_ms, hash)`。投影由 reducer 对每个事件增量执行一次：同一 `message_id` 取因果最早者；`ConversationOwner` 取 `epoch` 最大者；工作状态只前进。负责人同级再相同时取请求身份较小者，请求身份为 `SHA256(attestation 签名输入 ‖ 签名设备)`。同一份已签名请求经不同入口重试时请求身份相同，外层事件 hash 不参与仲裁，重试不会改变结果。

事件上限为 1 MiB。事件的 `blobs` 使用 `BlobRef { hash, size }`；本人消息的 `attachments` 为 `{ hash, size, name }`，按哈希升序、每个文件一次，与事件 `blobs` 一一对应，`name` 为 1 至 255 字节、不含路径分隔符和控制字符的文件名。附件单件上限 64 MiB，正文是 `blobs/sha256/<前两位>/<hash>` 文件，按网络和会话引用授权读取。

## 写入保护

Node 的写入状态为 `aligning`、`read_only`、`writable` 或 `forked`。只有 `writable` 时 Node 追加自己的事件，其他状态下所有本地写入在追加时被拒绝，调度和执行在进入 `writable` 前不启动，进入 `forked` 后不再开始新的回合和定时时刻。读取、同步和入库其他设备的事件不受影响。

每次进入 `writable`，Node 在本机 `identity_meta.write-file` 记下数据库文件的 inode 和创建时间。启动时文件仍是这一个，说明它就是上次写入用的数据，直接可写，不等其他 Node。复制数据目录或从备份恢复都会产生新文件，对不上，才走下表的对齐。

| 状态 | 进入条件 | 离开条件 |
|---|---|---|
| `aligning` | 启动时数据库文件不是上次写入用的那个，且成员目录中还有其他 Node | 本次启动以来每台其他当前成员 Node 都确认过：一次同步结束时它记录的本机 `max_seq` 不大于本机，本机在该序号上的事件就是它的头，本机日志也没有分叉；对端更多时先拉回本机自己的日志。成员目录变化时重新判断，被撤销的 Node 不再需要确认 |
| `read_only` | 对齐中且还没确认的 Node 都同步失败 | 这些 Node 恢复后补齐确认，或本人调用 `confirm_writable` |
| `writable` | 启动时文件对得上、没有其他 Node、对齐完成或本人确认 | 发现分叉 |
| `forked` | 本机存有自己日志的另一支 | 不离开；撤销这台设备后用新数据目录重新加入 |

拒绝写入时的错误为 `this Node is aligning with other Nodes; try again shortly`、`this Node is read-only until you confirm it was not restored from a backup` 和 `this Node's log has forked and it no longer writes`，网页翻译成中文。`confirm_writable` 只记录在本机，不进入 DAG，对 `forked` 无效。

分叉的两支经同步传给每台成员 Node，从分叉序号起两支都不计入，其他作者和其他会话照常写入。同一设备的两份副本都会存有对方那一支，都进入 `forked` 并停止写入。`write_state` 返回 `{ state, conflicts }`，每条冲突含作者、序号、本机主分支的版本（`kept`）和另一支的版本（`other`）。状态变化时向已订阅的浏览器推送 `{"type":"changed","payload":{"type":"write_state","state":...}}`。

## Node 之间的传输

### iroh Endpoint

每台 Node 运行一个 iroh 1.3 `Endpoint`，`SecretKey` 就是设备密钥。Endpoint 用 Minimal 预设、`MemoryLookup` 和 `RelayMode::Custom`：relay 来自 `network.relays`、本 Node 自己的 relay、成员签名地址记录里成员运行的 relay，以及从全网表选出的开放 relay，不使用 n0 公共 relay。同一个 relay URL 出现多次时合并为一条配置，带凭证的配置不会被无凭证的副本覆盖，地址簿和全网表里学到的裸 URL 也不会替换已配置的同一 relay 及其令牌。不向 n0 DNS 或 pkarr 发布地址；ticket 带入口 Node 的节点链接（设备编号、最多 4 个直连地址和 home relay）。成员地址来自个人网络内部交换的签名地址记录，其他设备的地址来自全网表，和已有 relay 路径合并后由 iroh 自动选择连接路径。连接优先直连，失败时经 relay 转发 QUIC 密文。

成员 Node 另在局域网用 mDNS 广播 `_vibeface._udp.local.`。实例名和 TXT 记录 `t` 都是标签 `hex(HMAC-SHA256(network_id, "vibeface:lan:v1\0" ‖ device_id ‖ hour)[..16])`，`hour` 是 Unix 毫秒除以 3600000 的 u64 大端字节；主机名为 `<标签>.local.`，SRV 端口是 iroh 实际监听的 UDP 端口，A 和 AAAA 是本机非回环网卡地址，网卡变化时自动更新；iroh 在该端口没有监听的地址族不广播。每到整点，同步循环在几秒内改用新标签重新登记，旧实例发出 mDNS 告别。收到的广播在拨号时才比对，用前一、当前和后一小时的标签匹配要拨的成员 Node，过了时间窗的标签自然失效。比对上的地址在 `connect` 中与地址簿和全网表提示合并；新出现或变化的广播只要属于当前成员 Node（不含自己），就立即唤醒与它的同步，同一 Node 每 10 秒最多唤醒一次；是否属于成员在收到时判定并记下，网络或成员变化时重新判定。只收私网 IPv4 和 ULA IPv6 地址，IPv4 在前，每条最多 4 个；拨号只用比对上的最新一条广播。最多保留 64 条广播，满时先删不属于成员 Node 的，再删最早收到的；收到告别或 SRV 记录过期后删除。Node 关闭网络时撤回自己的广播，最多等 1 秒让告别发出。`bind` 为具体地址的 Node 不启用 mDNS，尚未加入或已被移出网络的 Node（包括公共节点）不广播；mDNS 失败只记录日志，不影响 Node 运行。知道 `network_id` 和 `device_id` 的任何人（包括前成员和持有过邀请或恢复材料的人）都能算出并关联标签；标签只提示拨号地址，不是授权，小时轮换也不是撤销手段，成员资格仍由 QUIC TLS 证明的设备身份另行检查。

`Router` 按 ALPN 分发连接。私人 Node 请求每次占一条 QUIC 双向流，帧为 4 字节大端长度前缀加 JSON，单帧最多 16 MiB。`service` 的独立 Noise 帧和容量见[服务传输](#服务传输)。

| ALPN | 用途 | 接受的对端 |
|---|---|---|
| `vibeface/mesh-gossip/<network>/1` | 全网 iroh-gossip 主题 | 任意设备；不授予业务权限 |
| `vibeface/mesh/<network>/1` | 全网记录交换和按设备编号查询 | 任意设备；不授予业务权限 |
| `vibeface/sync/1` | `hello`、`membership`、`pull`、`forks`、`have` | 未撤销的成员 Node |
| `vibeface/blob/1` | 按哈希和偏移读取附件 | 未撤销的成员 Node |
| `vibeface/rpc/1` | 投递、派发、中断、App Server 操作转发、进度读取、地址簿交换、加入请求汇总 | 未撤销的成员 Node |
| `vibeface/client/1` | 网关转发的浏览器 Noise 会话，或手机 App 在局域网用临时密钥直接拨入的会话；每条流一个会话，帧为 `tag ‖ u32 BE 长度 ‖ 内容` | 任意设备，会话本身由 Noise 认证 |
| `vibeface/join/1` | 只开放 `join_check`、`join`、`join_status` 和 `join_withdraw` | 任意设备 |
| `vibeface/decision/1` | 经另一台设备送达会话的决定，目标核对决定令牌 | 任意设备，由决定令牌授权 |
| `vibeface/service/1` | 可选的公益和付费文件对象交付 | 已认证 PeerID，另验服务开关、条款、证明及对象授权，不要求成为私人成员 |

`sync`、`blob` 和 `rpc` 的每个请求带 `network_id`，与本机不一致时关闭连接。这些私人通道接受连接时按 `remote_id()` 核对成员资格，非成员 Node 被拒绝；成员目录变化时已接受的连接重新核对，对端不再是成员就关闭。撤销还会关闭到被撤销 Node 的出站连接并删除它的地址。公共节点不在任何个人网络里，私人 `sync`、`blob` 和 `rpc` 一律拒绝；显式启用文件交付时，只在独立 `vibeface/service/1` 上按服务规则接收。

历史上确为 Node 的被移除设备只可在 `sync` 上交换 `hello` 和 `membership`，每个 Node 跨连接只处理 1 个请求，本机共 4 个，每个请求含读取、处理和响应最多 10 秒，请求与撤销证明各限 256 KiB。它推来的签名治理事件仍可带来迟到否决；checkpoint 不从该通道安装，因此持恢复文件也不能推入倒签的找回或 rotation。每次先合入治理事件再检查当前成员资格，只有真正恢复为成员 Node 后才返回完整历史。仍被移除时只回复可独立验签的本次撤销证明，包含并发权限屏障所需的证据，不披露后续无关成员或名称。证明以最多 64 个 checkpoint、合计 1024 条治理事件为计算边界；超过边界、证明必须带上对方未以本次完整签名状态证明已知的后续授予或更名，或无法证明撤销时，明确拒绝而不回传全量。Hello 失败后，本机仍认定为成员的已认证对端可再接收一次纯 `membership` 交换，让合法迟到否决仍能收敛；本轮不算历史同步成功，下一连接重新同步。

### 同步

1. 双方交换 `{"type":"hello", network_id, vector: {author: [max_seq, head_hash]}, boot, membership_epoch, membership, forks}`。成员目录摘要不同时交换 `membership`，双方都合并为并集。
2. 对每个对方领先的作者发 `{"type":"pull", author, from_seq, limit}`，对方按 `seq` 升序返回主分支事件，每批最多 1024 条、合计 8 MiB。
3. 对方的头本机没有时，从两边较小的序号起每次加倍退后、每次拉一条，直到拉到本机已有的事件，再向后拉回对方那一支。
4. `forks` 是本机所有分叉作者从分叉序号起两支事件 hash 的摘要，没有分叉时为空字符串。摘要不同时发 `{"type":"forks", after}`，`after` 为上一页最后一条的 `[author, seq, hash]` 或 `null`，对方返回其后的分叉证据，每页限额同 `pull`，每轮最多 4 页。本机按对端记住读到的位置，下一轮接着读，读完后清空。
5. 本机日志头一变就向已连接的成员发 `{"type":"have", author, seq, hash}`，不论事件经哪条路径写入，对方落后时立即同步。`have` 只播报本机自己的日志头，其他作者的事件靠 Hello 转达。收到的 Hello 带本机没有的头、不同的成员目录摘要或新的一对不同分叉摘要时，也立即同步。空闲连接每 60 秒重新 Hello。一轮有进展但仍有作者落后（例如待补依赖的事件超过暂存上限），或分叉证据还没读完时，立即再同步一轮。对方主动关闭它连来的连接时立即重新同步。与某个成员的同步由成功变为失败或由失败恢复时，向已订阅的客户端推送一条不带会话的 `changed`，客户端据此重读 `nodes`。
6. `boot` 是对端本次启动的编号。编号变化说明对端重启，本机立即关闭到它的旧连接并重新同步，不等 QUIC 空闲超时。

同步只写入历史和投影，不重新执行历史中的操作。同步失败按退避重试，最长间隔 30 秒。

### 地址簿

每台 Node 为自己签一条地址记录 `{ version, network_id, device_id, seq, endpoint, gateway_url, relay? }`，以 `address` 域签名，`seq` 严格递增。`gateway_url` 在启用网页时取 `public_url`。`relay` 只在本 Node 运行 relay 时出现，取 `relay_url`。成员把记录里的 `relay` 加入自己的 relay 候选；`endpoint` 里的 relay 只用于拨号，不加入候选，因为对方的 home relay 未必接纳本 Node。同步循环每 5 秒检查本机地址，变化后重签并唤醒同步；成员之间经 `rpc` 的 `addresses` 交换记录，最多保留 256 条。浏览器经 `entrances` 读取，用 WASM 验签，丢弃非成员、已撤销、其他网络和带路径或凭证的 `gateway_url`。浏览器为每台 Node 保存已接受记录的 `seq` 高水位，撤回地址的记录也保存，刷新后不接受更旧的记录。HTTPS 且主机不是回环、私网或单段名称的入口算公网，二维码只用公网入口。

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
| `respond` | 另一台设备的 Bot 应答本设备挂起的请求（App Server 请求或等待中的 hook），同一请求只应答一次 |
| `hand_over_session` | 另一台设备上的 Bot 转交或不再关注本设备运行的会话，带发起 Bot 的会话 `from` 和它看到的 `version`；本设备核对归属与版本后写入，状态已成立时不写 |
| `cancel_manager`、`resolve` | 转发本人的管理控制，要求 `human: true` |
| `pending_joins`、`approve_join` | 汇总其他入口上的加入请求，把批准转发到保存请求的 Node |
| `join_changed` | 保存请求的 Node 告知本机持有的加入请求有变化，不带请求内容；收到的 Node 向本机客户端推送不带会话的 `changed`，不回传、不转发。同步正常但 3 秒内没有送达的 Node 在 3 秒后补发一次 |

### Relay 与公共节点

启用 `relay_listen` 的 Node 运行 `iroh-relay` 服务。relay 接纳本 Node 自己、当前成员 Node，以及带有效邀请令牌 `SHA256("vibeface:relay:v1" ‖ invite_hash)` 的加入者，所以只能经这台 relay 连通的新 Node 要用运行 relay 的 Node 发出的邀请；配置了 `relay_operator_token` 时另外接纳持该令牌的设备，`relay_open` 时接纳任何设备。`relay_blocked` 里的设备一律拒绝，本 Node 与成员 Node 以外的连接超出 `relay_max_clients` 时拒绝，`relay_rate` 限制从每个连接读取的速率。relay 始终只监听回环，由本 Node 在 `relay_listen` 上接收 TCP 连接再转给它；设了 `relay_max_clients_per_ip` 时，同一 IPv4 地址或 IPv6 /56 超出上限的连接直接关闭。前置代理为每条连接生成随机的一次性私有绑定，替换首个请求自带的同名头，准入时必须消费有效绑定，成员 Node 也不例外。缺失、伪造、已消费或重复的绑定均拒绝，所以健康检查之后经 keepalive 或管线化发来的未绑定升级，以及绕过前置代理的未绑定后端连接，都不能接入 relay。转发的连接任一方向结束即关闭两端，客户端连接开启 TCP keepalive。relay 看不到内容，但能看到哪些设备连接它、彼此何时通信和流量大小。

公共节点是不加入个人网络的 Node，通常同时提供开放 relay（`relay_open`）和面向任何设备的浏览器转发（`forwarding.any`），并在全网记录里宣告 `relay` 和 `gateway` 角色。它不运行调度、执行和同步，不保存个人网络的事件或附件。显式启用文件交付后，独立 `delivery/` 可以暂存交付密文及服务账本，不授予私人网络权限。浏览器经它的 `/api/client` 按设备编号连到成员 Node，它转发 Noise 密文，看不到内容，但能看到浏览器地址、目标设备编号、时间和流量大小。部署步骤见[从外网访问](node.md#从外网访问)。

### 全网

`network.mesh` 开启时，Node 加入 iroh-gossip 主题 `SHA-256("vibeface:mesh:v1:" ‖ network)`，向全网宣告一条以设备密钥签名（域 `mesh-v1`）的记录 `{network, device_id, seq, ttl_s, addrs, relay_url, reachable, roles}`，并持有、转发、持久化所有 Node 的记录。Endpoint 绑定后在后台入网，本机 API、维护接口和局域网发现不等待它。记录格式、交换、入网顺序和 relay 选择见[全网层](design/mesh.md)。

接收端验签，核对网络名、有效期和大小后才入表；同一设备只留一条，新记录的 `seq` 必须不小于已持有记录的 `seq` 加 30 秒。表最多 65536 条，保存在数据目录的 `mesh.sqlite3`，保留 30 天内见过的记录，删除后从 bootstrap 节点重新入网。

`mesh.bootstrap` 的每一项是节点链接 `vibeface://node/<device-id>?addr=<host>:<UDP-port>&relay=<URL>`，最多 4 个 `addr`、1 个 `relay`，至少有其一；relay 只是拨号提示，不加入 relay 列表。`vibeface mesh share` 或 `share_link` 给出本机的节点链接。官方 bootstrap 节点由仓库文件 `config/public-bootstrap.json` 的 `issuer`、`udp_host`、`udp_port` 和 `relay_url` 生成，Core 用 `include_str!` 编入同一文件，不另抄一份。QUIC 握手证明对端就是这台设备，编号不符时连接失败。`vibeface mesh bootstrap add|remove` 改写这个列表，Node 运行时立即生效。

角色只说明一台设备愿意提供什么：`relay {url, access, rate, max_clients, offer?}`、`gateway {url}`、`service {service, offer?}`。采用方先验证：relay 用 `/generate_204` 和真实握手，网关请求 `<url>/api/info` 并核对设备编号；私网和回环地址只在 `mesh.allow_private` 时采用。运行 relay 的 Node 经 `configure_relay`（网页**公共转发**、`vibeface relay volunteer on|off`）开放 relay，请邻居从外部探测通过后才宣告 `relay` 角色。

`vibeface join` 和本机网页的 `node_join` 先用票据里的直连地址和 home relay，同时在全网表里查发出邀请的 Node，表里没有时问邻居，再加上配置的 relay；入口的 home relay 不开放时带邀请令牌，全网表里查不到入口时，没有配置令牌的 relay 也带邀请令牌。`vibeface status` 的 `Mesh` 和 `vibeface mesh status` 显示全网是否开启、网络名、记录数和邻居数。入网失败按退避重试，不影响个人网络内的功能。

## 可选文件交付

VDP-1 服务默认关闭，与私人 DAG、附件同步和 Agent 工作分开。公益没有钱包和链调用。付费只接受字面回环 HTTP RPC 的 `chain_id=31337`，部署和 proof profile 必须与程序固定值一致。它不接入主网或真实稳定币。配置字段、资金状态和结算规则见[文件交付实验参考](paid-delivery.md)。

### 客户端授权

加密客户端方法 `delivery` 使用 `op` 区分请求。`status`、`list`、`get`、`find_intent` 和 `read` 要求已验证的个人网络主体。写操作另要求来自当前成员目录、且 `manage=true` 的客户端，受限本机会话和 Agent token 不能代替它。除 `configure` 外，写操作还要求 Node 处于 `writable`。

| op | 主要参数和行为 |
| --- | --- |
| `status` | 服务开关、PeerID、钱包和接收 key 的公开状态、文件容量上限。 |
| `configure` | `enabled`、`charity_enabled`、`paid_enabled` 三个布尔值，不接收钱包口令、链或 prover 路径。 |
| `list`、`get` | 当前个人网络的订单列表，或精确 `order_id` 的详情。 |
| `find_intent` | 当前网络下精确 `intent_id` 的订单，返回 `{order: ...}` 或 `{order: null}`。 |
| `prepare` | UUID `intent_id`、双方及服务 PeerID、模式、验收条件、文件名、base64 内容、服务费和链费上限。固定订单与报价，尚未付款。 |
| `confirm` | 原 `intent_id`，先持久化授权并返回，后台继续证明、付款和传输。 |
| `accept` | 原 `order_id`，仅接收端接受，后台按已授权条款继续。 |
| `reconcile` | 原 `order_id`，核对原意图和原交易，不创建替代付款。 |
| `read` | 已接受订单的完整文件，返回 `name`、`content_base64`、`sha256` 和 `bytes`，不是新的接收授权。 |

订单按当前网络隔离。接收端和发送端的写授权在后台副作用前再次核对。撤销管理成员或配置变化会停止当前执行步骤；锁定钱包只能观察原交易。公共节点上的 P 没有私人网络，后台仅按已接受的服务订单和本机预算发布 E、提款或对账。

### 服务传输

`vibeface/service/1` 在 QUIC 上使用独立 Noise 上下文 `vibeface:service:v1`，把会话 PeerID 绑定到对端。每条流处理一个请求，4 字节大端长度前缀，控制 JSON 最多 16384 字节。每个连接最多 64 条请求，服务同时最多 16 个连接，每个 PeerID 最多 2 个。默认关闭时拒绝服务连接。

服务操作包含公告、接收 key、报价、订单投递、对象授权与偏移、分块上传及读取、hold、open 和订单查询。付费对象 grant 绑定 P、调用 PeerID、订单、窗口、对象哈希、操作、最大字节数和截止。它不能复用为另一个对象或私人 RPC。分块大小为 8192 字节，持久偏移与对象账本共同提交；完整长度和 SHA-256 通过后才算完成。公益使用独立签名条款和对象授权，不构造虚假付费订单。

### 证明和恢复

规范 `DeliveryStatement` 为 47 个 ABI word，共 1504 字节，唯一声明为 `crates/delivery/abi/Statement.sol`。四种关系为 key binding、source、hold 和 open。profile 固定 guest image、schema、密码套件及窗口和资源上限。Native 测试、guest 执行和已验 Groth16 收据各自有不同的证据强度。

每窗口使用 ChaCha20-Poly1305 和 RFC 9180 HPKE。C 长度为窗口明文加 68 字节，E 长度为 80 字节。Source 校验全部签名和加密对应关系，hold 证明完整 C、接收私钥及同意，open 实际执行解封和解密。合约独立重建公开 statement，检查当前 registry、阶段、期限和固定结算地址。proof 不能替代链的当前状态或付款事实。

证明收据以 statement digest 固定本机任务路径，私密输入不进入 argv、日志、公共 fixtures 或 DAG。收据完成验证后原子发布，重启先验真并恢复原 seal。签名前在事务中固定 nonce、calldata 和 gas 预算，raw transaction 持久化后广播。结果未知只对账或在授权仍有效时重播同一字节。

接收 key 的注册、轮换和撤销只通过同用户维护 socket。注册、轮换和撤销均使用显式稳定 UUID，重试不能更换 action、gas、部署、profile 或 serial。状态读取不自动续期或重新启用 key，registry 确认前不宣称生效。旧 KeyId 私密材料保留，已 Held 窗口继续使用原 key，未 Held 的旧 serial 窗口按合约终止或退款。个人恢复文件和独立钱包备份都不包含这些订单解密材料，恢复必须保留完整 Node 数据目录。

## 通知和自动后续

加密通道推送 `{ "type": "changed", "payload": ... }`。payload 区分历史变更、`manager_progress`、`native_progress`、`write_state` 与失效提醒。本机签名成员目录有变化时（加入、撤销、自行离开、权限变化或同步合并来的新事件），Node 推送一条不带会话的 `changed`，客户端据此重读 `directory`。本机持有的加入请求被提交、揭示随机数、批准、拒绝或撤回时，Node 推送同样不带内容的 `changed`，不含 `join_id` 或状态；管理设备重读 `join_status`，其他成员只重读 `directory`。保存请求的 Node 同时向同步未失败的其他成员 Node 发 `join_changed`，它们各自向本机客户端推送同样的 `changed`，所以连在任何一台 Node 上的管理设备都能立即重读到另一台 Node 持有的请求（`join_status` 不带 `join_id` 时经 `pending_joins` 汇总）。通知只发一次，不重试；当时连不上的 Node 在同步恢复时会推送 `changed`，客户端届时重读。加入请求按 Node 时钟过期，不推送；列表里每条请求都带保存它的 Node 给出的 `expires_at_ms`，客户端在最早的到期时间之后重读，仍未过期就每 5 秒再读，直到它消失。客户端读取 `notifications`，用稳定 `client_id` 确认 `notification_id`，不同浏览器各自确认。会话的历史变更带 `open_works`：Node 现算的该会话仍要本人看的工作，客户端用它整体替换这个会话在任务和待处理里的部分，不自己按到达顺序推断；跨设备同步晚到的旧工作因此不会让后来的失败消失。

管理进度包含 `conversation_id`、`trigger`、`invocation_id`、`source_node`、递增 `sequence` 和 `progress`，`progress.type` 为 `text_delta`、`text_snapshot`、`status` 或 `tool`。原生进度包含 `conversation_id`、`work_id`、`target_node`、`sequence` 和 `progress`。临时进度不写入事件日志。客户端读取 `conversation` 时，入口为该会话建立有界的只读观察，经 `progress_read` 从负责人和执行节点读取进度。回合状态变化时，该设备在进度环里放一条只在环内的 `job_changed` 提示，观察方收到后让客户端重读会话；它不进事件日志，也不推给本机客户端。

`message.report_results` 默认 `true`，保存在签名的用户消息中。为 `false` 时，该消息引出的工作结束后不再入队汇报。

协调 Agent 总是完全权限，没有自主度设置和只读汇报轮。

`daily_decision_limit` 只限制每个 Node、每个个人网络的 UTC 每日结果汇报轮和其他非本人触发的轮次；协调 Agent 的分诊唤醒按同一上限另行计数。本人显式回答原请求的私有回答控制不受预算暂停影响，仍计入分诊次数。额度用完时保留未审阅事件，尚未得到本人答案的挂起请求仍按原期限兜底；额度恢复后只重新审阅事件，不重放原工具或已执行的工作。分诊调用先预留额度，只有当前进程确认模型未启动时才退还原日额度；启动后失败、结果未知或 Node 重启时不退款，也不自动重跑该批事件。预算不是 token 或账单上限。
