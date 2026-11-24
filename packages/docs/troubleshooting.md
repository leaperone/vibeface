---
title: 排障
description: 从日志、状态和连接信息定位 Vibeface 常见问题。
---

# 排障

出问题时按顺序看三样东西：`vibeface status`、数据目录里的 `node.log`，以及网页设置里导出的诊断包。分享之前检查项目路径、任务内容及外部程序输出。不要上传完整数据目录、恢复文件、邀请链接或凭证；日志中的外部错误文本不能视为已经完整脱敏。

## 首次使用卡住时

| 现象 | 先检查 |
| --- | --- |
| 找不到 `vibeface` 命令 | 安装器使用的 `--bin-dir` 是否在当前终端 PATH 中，也可直接运行该目录下的 CLI。 |
| 本机页面打不开 | `vibeface` 进程或后台服务是否运行，地址是否是这台电脑的 `http://localhost:4790`。 |
| 手机上打不开电脑的 localhost | localhost 指手机本身；用原生 App 的局域网加入方式或已配置的 HTTPS 入口。 |
| 终端能运行 Codex，后台显示未安装 | 在设备页点**自动接入**，会检查常用安装位置与 NVM；仍找不到时在详细设置检查实际程序路径及同目录 Node.js。找到程序不等于已登录。 |
| 程序已经登录，Bot 仍提示需要登录 | 先用**自动接入**并检查认证。Codex、Claude Code 可准备官方 CLI 访问令牌镜像，不消费源刷新令牌；源快照过期时先在原 CLI 刷新再检查，或使用页面的私有登录命令。Codex Keychain-only、Claude 显式自定义目录的 Keychain 不自动导入。Cursor 复用 `agent login`，桌面 GUI 登录不等于 CLI 已登录；显式环境认证优先。OpenCode 仍需私有登录，pi 沿用本机认证。 |
| 自动接入提示读取设置失败 | 重试或在详细设置手动填写。读取登录 shell 超时、输出超限或启动异常会明确失败，不表示未安装或没有密钥；Node 启动时遇到此问题仍会继续运行。 |
| 能创建 Bot，但没有回答 | 是否启用了 **运行 Bot**，模型和额度是否可用。创建 Bot 本身不验证模型调用。 |
| 第二台电脑看不到原来的 Bot | 是否加入了原来的个人网络，是否已批准安全码并完成同步；不要另建网络。 |
| 加入链接已用过或过期 | 从已有管理设备重新生成链接，不重新初始化已有 Node。 |
| 看见“结果待确认” | 先核对原任务与执行设备，不换消息编号再次派发同一写操作。 |

完整流程见[首次配置与加入设备](onboarding.md)，常用操作见[日常使用指南](user-guide.md)。

## 看状态

```sh
vibeface status
```

Node 在运行时输出类似：

```text
Peer ID: 3f2a…
Status: running
Listen: 127.0.0.1:4790
Data: /path/to/node-data
Version: 0.0.1
Writes: writable
Peer: device-a (9c41…) reachable, direct, RTT 18 ms, synced 4s ago
Peer: device-b (51be…) unreachable, unknown path, RTT unknown, synced 1830s ago
```

`Writes` 是这台 Node 能否写入，取值见下文“写入状态”。每台其他成员 Node 一行 `Peer`：`reachable` 表示最近一次同步连上了，`unreachable` 表示最近一次失败，`not tried yet` 表示启动后还没试过；`direct` 是直连，`relay` 经中继，`unknown path` 表示当前没有本机拨出的连接；RTT 是所选路径的往返时间；`synced Ns ago` 是最近一次同步完成的时间，`never synced` 表示本次启动后没有成功过。`--json` 输出同样的内容，`write_state` 和 `peers` 是对应字段，每个对端带 `path` 和 `rtt_ms`。

## 看健康检查

```sh
vibeface health
```

Node 每分钟检查以下问题。连续两轮都发现才算问题出现，只在问题出现和消失时各记一条日志：

| 检查 | 出现 | 消失 | 条件 |
|---|---|---|---|
| `work_stalled` | `work.stalled`（warn） | `work.recovered` | 运行中的工作超过 10 分钟没有任何进展事件，带 `work_id`、`conversation_id` |
| `work_looping` | `work.looping`（warn） | `work.loop_ended` | 运行中的 Codex 工作在最近 5 分钟连续完成 6 次相同调用和相同结果，或连续 6 次明确工具失败；普通进度不会清除证据。重复通知不计数，换轮或窗口过期后重置。只保存有界摘要，参数和原始结果不进入健康状态或日志；不自动中断或重试 |
| `app_server_unresponsive` | `app_server.unresponsive`（warn） | `app_server.recovered` | 已连接的 Codex App Server 在 10 秒内没有回应只读探测；不重启、不重发。App Server 未连接时这一轮不检查，保持上一轮的结论 |
| `sync_stalled` | `sync.stalled`（warn） | `sync.recovered` | 本机与对端的连接还在，或最近一次同步成功，但超过 10 分钟没有完成同步，带 `peer`。连接还在而每次同步都失败、启动以来一次都没同步成功的对端也算；连接断开且同步失败的对端按离线处理，不算。10 分钟从最近一次同步成功、本机第一次看到这次连接和最近一次附件进展中最晚的那个算起，所以重新连上或下载大附件时仍有完整的 10 分钟 |
| `job_stalled` | `job.stalled`（warn） | `job.recovered` | 一轮回复在排队、steer、领取、取消或对账中停在同一状态超过 30 分钟，从本机第一次看到它处于这个状态时算起，带 `job`（`会话 ID:trigger`）、`conversation_id`。运行中的一轮由运行超时和工作检查负责，不在此列；有轮次在运行时排队等待不算，在等待处理项的、健康提醒自己的一轮也不算 |

循环检测比较 Codex 命令、MCP 和动态工具的完整调用与结果；文件变更只统计明确失败。缺少完整结果或本机证据超过 64 KiB 时不比较相等，仍可统计明确失败。其他 Provider 保留原有停滞检查。

两轮之间隔了 3 分钟以上（例如电脑睡眠后醒来）时，这一轮不下结论：工作进展和同步的计时都从此刻重新开始，上一轮的发现不再算数。

输出当前问题（检查、对象、开始时间、详情）和最近 1 小时按事件统计的 warn、error 条数，`--json` 输出 `{checked_at_ms, problems: [{check, subject, since_ms, detail, conversation_id?}], last_hour: {warn: {事件: 条数}, error: {…}}}`。

新问题出现时，调度主 Bot 的那台 Node 给主 Bot 设一个一分钟后到期的提醒，正文写明发现问题的设备，让它先用 `read_health`、`read_logs` 排查，能自己处理就处理，需要本人决定时发拍板卡片。这一轮只提供只读工具（读会话、节点、日志和健康状态等）和 `propose_provider_config`，`tool_search` 也只列出这些，不能派发或控制工作，不能用 `remind` 设提醒或用 `message_bot` 给其他 Bot 发消息，不能改设置，也不能用 `fetch_url` 读外部网页。这些限制针对协调工具；产品自带的 Shell 和联网只有 Codex 和 Claude Code 能按轮收紧。Claude Code Bot 的这一轮在本人 Claude 设置已允许的之外只能读文件、搜索和运行 Node 工具 CLI，改文件、其他命令和联网都被拒绝。Codex Bot 的这一轮在只读沙箱里跑，关闭原生网页搜索，Shell 环境不带 `VIBEFACE_CODEX_SOCKET`。只读沙箱只防误写，不是安全边界：Shell 仍能读本机文件、访问网络（Node 工具经本机回环调用），也能连本机的 Node 维护 socket 和 Codex App Server socket 间接写入，挡不住日志内容里的提示注入；Bot 线程已被其他订阅者加载时，网页搜索和 Shell 环境可能沿用线程原有设置，只有沙箱每轮下发；opencode、pi 和 Cursor 这一轮仍有完整的宿主权限。Bot 不能改动或删除这类提醒。同一设备的同一问题每小时最多一次，全部问题每小时最多 6 次、每天最多 24 次，只有写成的提醒计入。提醒没写成时（被上限挡住、没有主 Bot、未开启 **运行 Bot**、本机不可写、不是调度主 Bot 的 Node 或写入失败），问题还在就每轮再试，直到写成或问题消失；只有第一次尝试记 `health.handoff_limited`、`health.no_primary_bot`、`health.not_owner` 或 `health.handoff_failed`，之后的重试只记 debug。

其他设备的 Node 只能为主 Bot 写本人消息引出的设置改动，没有不冒充本人就能让主 Bot 开始一轮的路径，所以它们自己只记 `health.not_owner`。调度主 Bot 的 Node 每轮健康检查向已连接的成员 Node 读取它们已成立的问题（每台最多 32 条，每次最多等 3 秒），记 `health.member_problem`，再像本机问题一样交给主 Bot，提醒里写的是发现问题的那台设备，和本机问题共用上限；某台成员不再报告的问题即结束，这一轮没回答的成员保留上次的问题，没连上的成员不保留。`vibeface health` 在这台 Node 上也列出这些问题，带 `device`。主 Bot 用 `read_logs`、`read_health` 的 `node` 读那台设备的日志和健康状态。

`read_logs` 和 `read_health` 只提供给主 Bot，并且只在两种轮次可用：管理设备上本人发的消息开启的轮次，或本机 Node 写的健康提醒开启的轮次，且这条提醒最后一次变更仍是 Node 写的那次、仍是一次性提醒；其他轮次调用会被拒绝。它们默认读执行这一轮的设备自己的日志和健康状态；给了 `node`（成员 Node 的设备 ID）时经节点间 RPC 读那台设备的，那台设备不在线或版本较旧时报错。`read_logs` 一次最多返回 32000 字节，每条的说明截到前 2000 个字符，过大的 `fields` 换成 `{truncated: true, prefix}`；匹配的记录多于 `limit` 或更早的记录放不下时结果带 `truncated: true` 和不透明的 `cursor`，带同样的筛选条件把它作为下次调用的 `cursor` 接着往前读，同一毫秒的记录也不重不漏。时钟回拨后写入的记录多于一次读取的窗口时，回拨前那些时间更晚的记录可能读不到。`read_health` 里每台对端只给连通情况和最近同步时间，与 `nodes` 工具给 Agent 的口径一致，不含路径、往返时间和断线统计。

Node 没在运行时只输出前四行并以非零码退出，`Detail` 给出原因。`status` 经数据目录里的私有维护 socket 查询，不需要网络，也不需要安装服务。

## 看日志

运行中的 Node 把日志追加到数据目录的 `node.log`（权限 600），每行一条 JSON 记录：

| 字段 | 含义 |
|---|---|
| `ts_ms` | 毫秒时间戳 |
| `level` | `trace`、`debug`、`info`、`warn` 或 `error` |
| `source` | `node`、`web`（浏览器）、`mobile`（手机 App）、`desktop`（原生桌面 App）、`agent`（协调 Agent）、`cli`（命令行）或 `codex`。外来记录的来源由发送方自报，只能取它那条通道允许的值；`fields.device_id` 由 Node 按认证结果填写，发送方写的会被替换 |
| `event` | 事件名，例如 `work.dispatched` |
| `message` | 说明 |
| `conversation_id`、`work_id`、`thread_id` | 知道时才有 |
| `fields` | 其余字段 |

默认级别是 Vibeface 的 info 和依赖库的 warn；启动 Node 和 `update` 之外的命令不显示依赖库日志。iroh 的连接处理（`iroh::protocol`）和 gossip 网络层（`iroh_gossip::net`，含拨号、发送队列满和解码失败，`iroh_gossip::net::util` 除外）只记 error：陌生设备握手失败、对端不在线都属常态，对端和邻居的上下线看 `peer.*` 与 `mesh.*`，Vibeface 自己处理连入连接时出的错记为 `net.accept_failed`。可用 `RUST_LOG` 环境变量调整，例如 `RUST_LOG=warn,vibeface=debug`；要看回这两类握手告警，用 `RUST_LOG=warn,vibeface=info,vibeface_bridge=info`。前台在终端运行时日志同时以文本打印到终端。`node.log` 写到 8 MiB 时改名为 `node.log.1`，旧文件依次后移，共保留 5 个。

重复的记录会合并。来源、级别、事件、说明、三个 ID 和 `fields` 都相同的记录，60 秒内再次出现时不再逐条写入；只在状态变化时记的 `peer.online`、`peer.offline`、`mesh.neighbor_up`、`mesh.neighbor_down`、`sync.write_state`、`work.state` 和健康检查的 `*.stalled`、`*.unresponsive`、`*.recovered` 不合并，每次变化都立即写入。窗口结束时补一条同样的记录，`fields` 多出三项：

| 字段 | 含义 |
|---|---|
| `repeated` | 这个窗口里合并掉的条数 |
| `first_ms`、`last_ms` | 其中第一条和最后一条的时间 |

持续重复的记录每 60 秒一条，出现总数是首条加上各条 `repeated` 之和。Node 开始停止时（`node.stopping` 之后）和停止结束时都把尚未写出的计数写入；被强制结束时最多丢最后一个窗口的计数。浏览器、Agent 和命令行提交的记录不能自带这三项，Node 会去掉它们。同时合并的不同记录最多 256 种，超出时最早的一种先结算；超过 8 KiB 的记录不合并。终端上的文本输出不合并。

同一个日志还收这些来源的记录：

- 浏览器上报未捕获的错误、未处理的 Promise 拒绝、失败的请求和设置等操作，只发给当前连接的那台 Node，要求已加入的设备或本机会话。记录只在页面内存里排队，最多 100 条；发送失败时这一批留在队首，5 秒后（`log_rate_limited` 时一分钟后）随下一条记录再发，重新连上时立即补发，只有 `log_rejected` 才丢弃；结果未知后重发的记录在 Node 上可能出现两次。浏览器和手机掉线后重连的前三轮（启动时的首次连接也算一轮）里，每个没连上的入口记一条 `connection.attempt_failed`，`fields` 有连接方式 `path`、目标 `node`、访问地址、耗时 `elapsed_ms`、第几轮 `attempt`、通道是否已打开 `opened` 和错误码。失败的请求（`web.rpc_failed`）的 `message` 是页面显示给人看的文案，`fields.code` 是 Node 返回的错误码，`fields.cause` 是 Node 或连接层的原始错误文本（最多 500 字符），没有或与文案相同时省略；
- 手机 App 上报同样的记录（`mobile.uncaught`、`mobile.unhandled_rejection` 等），另记连接阶段的每次变化（`mobile.connection`，`fields` 有 `from`、`to`、连接方式 `path`、目标 `node`、访问地址和断开原因，不含凭证），来源是 `mobile`，`fields.device_id` 同样取认证这条通道的设备。手机先把记录写进本机存储，最多保留最新 300 条，App 重启或崩溃后仍在；每条带 `fields.client_seq`（递增编号）和 `fields.client_ts_ms`（手机上产生的时间），`ts_ms` 仍是 Node 收到的时间。连上 Node 后自动按批补发，Node 确认收下才从手机删去；结果未知（如超时），或 Node 因维护、权限等与这批记录无关的原因拒绝时保留，稍后重发，Node 跳过编号不大于该设备已写入最大编号的记录，所以不会重复。只有 Node 说记录本身无效（`log_rejected`）时这一批才丢弃。有记录被跳过时，Node 回跳过的条数 `skipped`（总是这一批最前面的几条）和写完这一批后的最大编号 `max_seq`。手机在本机存储里记着发出过的最大编号，被跳过的记录都不大于它时就是重发，这一批算送达；有被跳过的记录大于它，或 `max_seq` 不小于手机的下一个编号时，说明手机编号倒退了（如本机存储损坏后重来），Node 这次写下的记录从手机删去，其余保留的记录从 `max_seq + 1` 起重新编号再发，不会把没写入的记录当作已送达。补发遵守下面的额度，超出额度（`log_rate_limited`）时等一分钟再续传。新记录约半秒后写入本机存储；致命崩溃的记录立即写入，过大的记录截短时仍保留 `fields.fatal` 和堆栈开头一段，所以堆栈很长时也立即写入。App 转入后台或失去焦点时，把还在等这半秒的记录立即写入。**设置**的**问题排查**里点**发送日志到节点**立即补发，显示已发送的条数；没连上时连上后自动发送。**报告问题**发出前也先补发，最多等 3 秒；补发的记录在手机当前连接的那台 Node 上，手机连的不是主 Bot 所在的 Node 时，主 Bot 用 `read_logs` 的 `node` 读那台 Node 的日志，且要那一轮由管理设备上本人发的消息开启。Node 明确回答没写入（维护、额度、权限等）时，这一批不计入发出过的最大编号；只有结果未知时才计入，所以结果未知之后手机编号又倒退时，被跳过的记录仍可能当作重发而算送达。记录里的孤立代理字符在上报前换成 U+FFFD，截短不切开表情等成对字符；Node 解析不了的单个请求只回错误（`log` 回 `log_rejected`，其他回 `invalid_request`），不断开通道。Node 只在内存里记编号，重启时正在发送的那一批可能重复一次；
- 协调 Agent 和命令行用 `vibeface log <level> <message> [--conversation ID] [--work ID] [--thread ID]` 写一条；`vibeface codex call` 自动记下结果，`vibeface codex send` 与 `dispatch_work` 一样由 Node 记下工作事件；
- `vibeface update check` 和 `update apply`（含自动更新）像 `vibeface log` 一样经运行中的 Node 记下检查和应用的结果，来源是 `cli`；连不上 Node 时这条结果只在更新器自己的输出里；
- 原生桌面 App 连着本机 Node 时，把自己显示的错误（`desktop.error`）和崩溃（`desktop.panic`）经同一个本机通道写入，来源是 `desktop`；
- Codex App Server 的标准错误逐行记录，能看出 thread 时带上 `thread_id`。Cursor ACP 不记录原始标准错误，只返回受控的失败类别。

每条外来记录不超过 8 KiB，一次最多 20 条，每个发送方每分钟最多 120 条，超出整批拒绝，Node 跳过的重发记录不计入。`log` 命令可带 `source`，只能是 `web`（默认）或 `mobile`；`vibeface log --source` 只能是 `agent` 或 `cli`，桌面 App 经同一本机通道写 `desktop`。浏览器和手机按设备身份分别计数，本机 Agent、CLI 和桌面 App 各自共享额度。

查看日志：

- 命令行：`vibeface logs` 读本机数据目录，支持 `--since 30m`、`--level warn`、`--source web` 或 `--source mobile`（可重复）、`--conversation`、`--work`、`--grep`、`--limit`、`--json` 和 `-f` 持续跟踪；
- 网页：**设置**的**问题排查**里点**日志**，可按级别、来源和关键词筛选，展开一条后能复制它、整个会话或整个任务的全部记录。管理设备和本机会话可以查看。

作为后台服务运行时，标准错误不是终端，日志只写 `node.log`。macOS 的 `service.stderr.log` 和 Linux 用户 journal 里只剩崩溃输出。更新器的完整输出在 `update.stdout.log` 和 `update.stderr.log`，Node 在运行时结果另记入 `node.log`。

日志里值得找的几行：

| 日志 | 含义 |
|---|---|
| `node.started` | Node 启动完成，带 `version`、`data_epoch`、`os`、`arch` 和是否开启网页 |
| `node.panic` | Node 进程里任何一次 panic，`message` 是原因，`location` 是源码位置；之后照常输出原有崩溃报告。被 tokio 接住的后台任务 panic 也记，这时进程不一定退出 |
| `node.start_failed` | Node 打开数据或启动服务时失败，`error` 是原因 |
| `node.stopping`、`node.stopped` | Node 开始停止和停止完成。`reason` 是 `stop_requested`（收到停止信号）、`parent_closed`（桌面 App 退出）、`rejoin`、`api` 或 `maintenance`，出错停止时为 error 级并带 `error`。有 `node.started` 却没有 `node.stopped`，说明进程被强制结束或崩溃 |
| `input.accepted`、`input.rejected` | 收下或拒收一条发来的消息，带 `conversation_id`、`message_id`；收下时 `state` 是排队结果，拒收时 `code` 和 `error` 是原因 |
| `input.admitted` | 当时没排上队的消息重试后排上了 |
| `input.delivered` | 另一台 Node（`origin`）转来的消息或任务结果已排队，`trigger` 是它的触发编号 |
| `schedule.fired` | 定时任务到点，已排队 |
| `triage.queued` | 收到一条会话动态，等协调 Agent 分诊，带 `provider`、`kind`、`origin` 和 `thread_id` |
| `triage.concluded` | 分诊结论：`conclusion` 是 `dismiss`（不用处理）或 `raise`（向你提问，带 `severity`）。不记问题和会话内容 |
| `peer.online`、`peer.offline` | 与成员 Node（`peer`）启动后首次同步成功、恢复同步，或同步失败（`error` 是原因，含本机错误，不一定是网络不通）。只在状态变化时记，启动后第一次同步就失败也记一条 `peer.offline` |
| `mesh.neighbor_up`、`mesh.neighbor_down` | 全网邻居连上或断开 |
| `net.accept_failed` | 别的设备连进来后，Vibeface 处理这个连接时出错，带 `alpn`、`peer` 和 `error` |
| `update.checked` | 更新器检查了通道，`available_version` 为空表示已是最新 |
| `update.deferred`、`update.applied`、`update.failed` | 更新因 Node 正忙推迟、已应用（`previous_version`、`version`），或检查、应用失败（`action`、`error`）。拿更新锁之前的失败只在命令输出和 `update.stderr.log` 里 |
| `sync.write_state`（`write state changed`） | 写入状态变化，`state` 是新状态 |
| `sync.forked`（`another copy of this device wrote different events; writes stopped`） | 发现这台设备的数据被复制过，已停止写入 |
| `sync.write_confirmed`（`write confirmed without a backup check`） | 有人在网页上确认了“不是从备份恢复的” |
| `refused a connection from a non-member` | 非成员设备试图连接，debug 级别 |
| `relay.started`（`relay started`） | 本机 relay 已启动，`listen` 字段是它的地址 |
| `app_server.start_failed`、`app_server.exited` | Node 启动的 Codex App Server 没能启动或已退出，`error`、`status` 是原因 |
| `work.dispatched`、`work.dispatch_rejected` | 工作被接纳或派发被拒绝，带 `work_id` 和原因 |
| `work.turn_started`、`work.start_failed`、`work.start_unknown` | Codex 回合开始、被拒绝或结果未知 |
| `work.state` | 工作状态变化，结束时带 `outcome` |
| `agent.turn.started`、`agent.turn.finished` | 调度领取一个作业后协调 Agent 一轮开始（带 `trigger`、`attempt`、`provider`），和这轮的结果（`state`，没回复时带 `failure`） |
| `agent.tool`、`agent.tool_failed` | 协调 Agent 调用的工具 |
| `codex.call`、`codex.call_failed` | Agent 经命令行向 Codex App Server 发的请求 |
| `codex.app_server.stderr` | Codex App Server 的标准错误 |
| `work.stalled`、`work.looping`、`app_server.unresponsive`、`sync.stalled` 及对应恢复事件 | 健康检查发现和解除的问题，见上文“看健康检查” |
| `health.handed`、`health.handoff_limited`、`health.no_primary_bot`、`health.not_owner`、`health.handoff_failed` | 新问题已交给主 Bot、被每小时或每天的上限挡下、没有主 Bot、主 Bot 由其他 Node 调度（带 `owner`），或提醒没写成 |

写入前每条记录统一脱敏：邀请链接片段替换为 `[invite]`，`Bearer` 后的令牌、relay 令牌、Cursor 回调令牌和环境变量中名字以 `_KEY`、`_TOKEN`、`_SECRET`、`_PASSWORD` 结尾的值，以及设置页填写的 Provider 变量值替换为 `[redacted]`。日志不记录加入 secret、本机启动 token 或模型密钥。

## 导出诊断包

在网页**设置**的**问题排查**里选时间范围（最近 1 小时、1 天或 7 天），点**导出诊断包**，浏览器保存一个 `vibeface-diagnostics-<日期>.json`。管理设备和本机会话可以导出。诊断包来自当前连接的那台 Node，包含：

- 版本、操作系统、CPU 架构和设备编号；
- 本机配置摘要，包括项目目录数量、已启用的 Agent 和会话扫描状态，不导出配置文件全文；
- 写入状态、冲突数量和每台对端的连通情况：`reachable`、`synced_ms`、`path`（`direct` 或 `relay`）、`rtt_ms`，以及本次启动以来的 `path_changes` 和 `disconnects`。这两项只统计本机拨给该对端的同步（SYNC）连接，一次断网只计一次：`path_changes` 是这些连接所选路径在直连和中继之间切换的次数；`disconnects` 是这些连接因对端关闭、空闲超时或网络中断而结束的次数，本机主动关闭或丢弃的不计，对端拨进来的连接也不计；
- `health`：与 `vibeface health --json` 相同的健康检查结果；
- 分诊预算的已用额度、上限、下次尝试时间，以及待审阅、处理中和结果未知的事件数；
- 时间范围内的 `node.log` 记录（最多 20000 条，已脱敏）和 `service.stderr.log` 最后 50 行。

`service.stderr.log` 导出前同样脱敏。

## 写入状态

| `Writes` | 网页显示 | 意思 | 怎么办 |
|---|---|---|---|
| `writable` | 无提示 | 正常 | 无 |
| `aligning` | 连接状态显示“同步中” | 数据像是复制或从备份恢复的，正在确认自己没有落后于其他设备记下的内容 | 等它连上每一台其他 Node，通常几秒；某个 `Peer` 行一直 `unreachable` 时检查网络和 relay，那台 Node 不再使用就移除它 |
| `read_only` | 待处理里出现“这个节点暂时只读” | 数据像是复制或从备份恢复的，还没确认过它的其他 Node 都连不上，无法确认这份数据是不是旧备份 | 见下文 |
| `forked` | “某某 的数据被复制过” | 另一份同样身份的数据写了不同内容 | 见下文 |

正常重启直接可写，其他设备离线也一样。只有数据库文件和上次写入时的不同（复制了数据目录或从备份恢复）才进入对齐。对齐和只读期间可以照常阅读，发消息会提示稍后再试，已排队的消息在可写后继续处理。

### 只读：是否从备份恢复

数据目录是从备份、快照或旧磁盘恢复的，点**是，等其他设备**，然后打开你的其他设备，连上后它会先拉回自己写过的内容再变成可写，不会产生分叉。

确定这份数据没有被恢复过，例如只是把数据目录挪到了另一块磁盘，点**不是，继续使用**。这个确认只记在这台设备上。确认错了的后果是分叉：这台设备之后写的内容会与它以前写过、但不在这份数据里的内容冲突。

### 分叉：数据有两份副本

同一个数据目录被复制到两处，或者从备份恢复后被确认为可写，两份都写了内容，就会分叉。所有设备都会收到两份内容，从分叉处起两份都先不算数；其他设备和其他内容照常。移除这台设备时，保留当前连接的电脑先收到的那份。

两份副本都会停止写入。处理方法：

1. 在待处理卡片的**两份内容**里看两边各写了什么，记下需要保留的。
2. 点**去设备页**，在管理设备上移除这台设备。
3. 停掉这台设备两份副本上的 Node，用 `vibeface init --data-dir <新目录>` 初始化一个新的数据目录，再用新的加入链接执行 `vibeface join --data-dir <新目录> <链接>`。旧数据目录不要再启动。
4. 把需要保留的内容重新发一次。

其他设备上出现“某某 的数据被复制过”时，说明是那台设备分叉了，按同样步骤处理那台设备。

## 常见问题

**对端一直 `unreachable`。** 两台 Node 没有直连路径时要经 relay 转发。确认至少一台 Node 用 `--relay-listen` 运行 relay，其他 Node 的 `config.json` 里 `network.relays` 指向它，反向代理允许 WebSocket 升级。改了 `network` 字段要重启。见 [Node 之间的连接](node.md#node-之间的连接)。

**手机打开官方页面或自建公共节点后连不上个人设备。** 公共节点按设备编号在全网表里找到个人设备上的 Node。检查这台 Node 正在运行，`vibeface status` 的 `Mesh` 不是 `off`，`vibeface mesh status` 有邻居，自建时两边在同一个网络名下；两边没有直连路径时还要有共同的 relay，Node 会从全网表里选用公共节点宣告的开放 relay。见 [从外网访问](node.md#从外网访问)。

**任务停在“待核对”。** Node 无法确认一项工作是否执行过时不会自己重做，见 [待核对](node.md#待核对)。

**设置页提示 Codex 后台服务没启动。** Node 托管的 `codex app-server` 退出后会按 1 到 30 秒退避重启。持续失败时检查 `bins.codex` 是否可执行、Codex 是否已登录，然后重启 Node。它的 socket 在私有的 `/tmp/vibeface-<编号>/` 目录，进程组号在数据目录的 `codex-app-server.pid`。

**连接页显示排队中、访问地址正忙或加入的设备太多。** Gateway 同时转发的会话数由 `forwarding.sessions` 决定，默认 256；名额用完时新会话按 `forwarding.queue` 排队，页面显示“排队中，第 n 位”，队列也满时回 `gateway_busy`。同一浏览器地址的连接已达 `forwarding.per_ip` 时，新连接在升级前收到 HTTP 429，页面显示“连不上这个节点”，关掉多余的页面后重试。官方公共节点的名额很小，排队是正常现象。一台 Node 同时最多接待 32 个还没加入的设备，每个最多 15 分钟。自己的 Gateway 碰到这些上限时多半有大量陌生连接，查看 `node.log` 和反向代理的访问日志，稍后再试。

**所有管理浏览器都丢了。** 在可信浏览器打开自己设备的网页，连接页选“用恢复文件找回”，选恢复文件、输入恢复密码，勾选丢失的设备后确认。这个浏览器不需要先加入。先下载并保存新的恢复文件，再提交找回；提交不代表已取得管理权。更换成员目录的找回默认有 48 小时等待期。如果仍有已加入的管理设备，可以在它的找回提醒上点“立即生效”并再次确认。原目录中的有效管理设备也可以否决，等待期内签署的有效否决即使迟到也可能让副本返回原目录。生效后新浏览器成为管理设备，旧恢复文件才被下一把替换。只轮换恢复密钥的操作立即生效，与这里的找回不同。提示“这个恢复文件已作废”时改用最近一次有效的文件。恢复文件不备份业务正文，详见 [撤销与恢复](privacy-security.md#撤销与恢复) 与 [备份与恢复](node.md#备份与恢复)。

**端口被占用。** `init` 和首次启动会先检查 `listen` 和 `local_listen`，提示“端口 X 被占用”，用 `--listen` 或 `--local-listen` 换一个。

不要为了排障删除 `node.sqlite3`、复制运行中的数据目录，或用另一台机器启动同一个数据目录的副本，这些都会造成分叉或丢失历史。
