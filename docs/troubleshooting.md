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
| 程序已经登录，Bot 仍提示需要登录 | 默认 Provider 使用 Node 私有目录；从设备 **AI** 设置复制登录命令，在运行 Node 的电脑上执行。 |
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
Peer: device-a (9c41…) reachable, synced 4s ago
Peer: device-b (51be…) unreachable, synced 1830s ago
```

`Writes` 是这台 Node 能否写入，取值见下文“写入状态”。每台其他成员 Node 一行 `Peer`：`reachable` 表示最近一次同步连上了，`unreachable` 表示最近一次失败，`not tried yet` 表示启动后还没试过；`synced Ns ago` 是最近一次同步完成的时间，`never synced` 表示本次启动后没有成功过。`--json` 输出同样的内容，`write_state` 和 `peers` 是对应字段。

Node 没在运行时只输出前四行并以非零码退出，`Detail` 给出原因。`status` 经数据目录里的私有维护 socket 查询，不需要网络，也不需要安装服务。

## 看日志

运行中的 Node 把日志追加到数据目录的 `node.log`（权限 600），每行一条 JSON 记录：

| 字段 | 含义 |
|---|---|
| `ts_ms` | 毫秒时间戳 |
| `level` | `trace`、`debug`、`info`、`warn` 或 `error` |
| `source` | `node`、`web`（浏览器）、`agent`（协调 Agent）、`cli`（命令行）、`codex` 或 `cursor` |
| `event` | 事件名，例如 `work.dispatched` |
| `message` | 说明 |
| `conversation_id`、`work_id`、`thread_id` | 知道时才有 |
| `fields` | 其余字段 |

默认级别是 Vibeface 的 info 和依赖库的 warn；启动 Node 和 `update` 之外的命令不显示依赖库日志。可用 `RUST_LOG` 环境变量调整，例如 `RUST_LOG=warn,vibeface=debug`。前台在终端运行时日志同时以文本打印到终端。`node.log` 写到 8 MiB 时改名为 `node.log.1`，旧文件依次后移，共保留 5 个。

同一个日志还收这些来源的记录：

- 浏览器上报未捕获的错误、未处理的 Promise 拒绝、失败的请求和设置等操作，只发给当前连接的那台 Node，要求已加入的设备或本机会话；
- 协调 Agent 和命令行用 `vibeface log <level> <message> [--conversation ID] [--work ID] [--thread ID]` 写一条；`vibeface codex call` 自动记下结果，`vibeface codex send` 与 `dispatch_work` 一样由 Node 记下工作事件；
- Codex App Server、Codex SDK 和 Cursor 桥的标准错误逐行记录，能看出 thread 时带上 `thread_id`。

每条外来记录不超过 8 KiB，一次最多 20 条，每个来源每分钟最多 120 条，超出整批拒绝。

查看日志：

- 命令行：`vibeface logs` 读本机数据目录，支持 `--since 30m`、`--level warn`、`--source web`（可重复）、`--conversation`、`--work`、`--grep`、`--limit`、`--json` 和 `-f` 持续跟踪；
- 网页：**设置**的**排障**里点**日志**，可按级别、来源和关键词筛选，展开一条后能复制它、整个会话或整个任务的全部记录。管理设备和本机会话可以查看。

作为后台服务运行时，标准错误不是终端，日志只写 `node.log`。macOS 的 `service.stderr.log` 和 Linux 用户 journal 里只剩崩溃输出。更新器的日志在 `update.stdout.log` 和 `update.stderr.log`。

日志里值得找的几行：

| 日志 | 含义 |
|---|---|
| `sync.write_state`（`write state changed`） | 写入状态变化，`state` 是新状态 |
| `sync.forked`（`another copy of this device wrote different events; writes stopped`） | 发现这台设备的数据被复制过，已停止写入 |
| `sync.write_confirmed`（`write confirmed without a backup check`） | 有人在网页上确认了“不是从备份恢复的” |
| `refused a connection from a non-member` | 非成员设备试图连接，debug 级别 |
| `work changed a file outside its directory` | 工作改动了它目录之外的文件，只记录，不拦截 |
| `relay.started`（`relay started`） | 本机 relay 已启动，`listen` 字段是它的地址 |
| `app_server.start_failed`、`app_server.exited` | Node 启动的 Codex App Server 没能启动或已退出，`error`、`status` 是原因 |
| `work.dispatched`、`work.dispatch_rejected` | 工作被接纳或派发被拒绝，带 `work_id` 和原因 |
| `work.turn_started`、`work.start_failed`、`work.start_unknown` | Codex 回合开始、被拒绝或结果未知 |
| `work.state` | 工作状态变化，结束时带 `outcome` |
| `agent.turn.started`、`agent.turn.finished` | 协调 Agent 一轮开始和结束 |
| `agent.tool`、`agent.tool_failed` | 协调 Agent 调用的工具 |
| `codex.send`、`codex.call`、`codex.call_failed` | Agent 经命令行给 Codex 发的消息和请求 |
| `codex.app_server.stderr`、`codex.sdk.stderr`、`cursor.stderr` | 子进程的标准错误 |

写入前每条记录统一脱敏：邀请链接片段替换为 `[invite]`，`Bearer` 后的令牌、入口令牌、Cursor 回调令牌和环境变量中名字以 `_KEY`、`_TOKEN`、`_SECRET`、`_PASSWORD` 结尾的值，以及设置页填写的 Provider 变量值替换为 `[redacted]`。日志不记录加入 secret、本机启动 token 或模型密钥。

## 导出诊断包

在网页**设置**的**排障**里选时间范围（最近 1 小时、1 天或 7 天），点**导出诊断包**，浏览器保存一个 `vibeface-diagnostics-<日期>.json`。管理设备和本机会话可以导出。诊断包来自当前连接的那台 Node，包含：

- 版本、操作系统、CPU 架构和设备编号；
- 本机配置摘要，包括项目目录数量、已启用的 Agent 和会话扫描状态，不导出配置文件全文；
- 写入状态、冲突数量和每台对端的连通情况；
- 分诊预算的已用额度、上限、下次尝试时间，以及待审阅、处理中和结果未知的事件数；
- 时间范围内的 `node.log` 记录（最多 20000 条，已脱敏）和 `service.stderr.log` 最后 50 行。

`service.stderr.log` 导出前同样脱敏。

## 写入状态

| `Writes` | 网页显示 | 意思 | 怎么办 |
|---|---|---|---|
| `writable` | 无提示 | 正常 | 无 |
| `aligning` | “这台设备正在和你的其他设备对齐” | 数据像是复制或从备份恢复的，正在确认自己没有落后于其他设备记下的内容 | 等它连上每一台其他 Node，通常几秒；某个 `Peer` 行一直 `unreachable` 时检查网络和 relay，那台 Node 不再使用就移除它 |
| `read_only` | 待处理里出现“这台电脑暂时只读” | 数据像是复制或从备份恢复的，还没确认过它的其他 Node 都连不上，无法确认这份数据是不是旧备份 | 见下文 |
| `forked` | “这台设备的数据有两份副本” | 另一份同样身份的数据写了不同内容 | 见下文 |

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

其他设备上出现“某某 的数据有两份副本”时，说明是那台设备分叉了，按同样步骤处理那台设备。

## 常见问题

**对端一直 `unreachable`。** 两台 Node 没有直连路径时要经 relay 转发。确认至少一台 Node 用 `--relay-listen` 运行 relay，其他 Node 的 `config.json` 里 `network.relays` 指向它，反向代理允许 WebSocket 升级。改了 `network` 字段要重启。见 [Node 之间的连接](node.md#node-之间的连接)。

**手机打开公共入口后连不上个人设备。** 公共入口只经自己的 relay 找到成员 Node。检查个人设备上的 Node 是否用 `--entry` 和 `--relay-token-file` 初始化，令牌文件是否与入口数据目录里的 `entry-token` 相同，nginx 是否把 `/relay`、`/ping` 和 `/generate_204` 转给入口的 relay 端口。见 [配置公共入口](node.md#配置公共入口)。

**任务停在“待核对”。** Node 无法确认一项工作是否执行过时不会自己重做，见 [待核对](node.md#待核对)。

**设置页提示 Codex 后台服务没启动。** Node 托管的 `codex app-server` 退出后会按 1 到 30 秒退避重启。持续失败时检查 `bins.codex` 是否可执行、Codex 是否已登录，然后重启 Node。它的 socket 在私有的 `/tmp/vibeface-<编号>/` 目录，进程组号在数据目录的 `codex-app-server.pid`。

**连接页提示网关忙或加入的设备太多。** 一台 Gateway 同时最多转发 256 个会话，超出时回 `gateway_busy`；一台 Node 同时最多接待 32 个还没加入的设备，每个最多 15 分钟。正常使用碰不到这两个上限，碰到时多半有大量陌生连接，查看 `node.log` 和反向代理的访问日志，稍后再试。

**所有管理浏览器都丢了。** 在可信浏览器打开自己设备的网页，连接页选“用恢复文件找回”，选恢复文件、输入恢复密码，勾选丢失的设备后确认。这个浏览器不需要先加入。先下载并保存新的恢复文件，再提交找回；提交不代表已取得管理权。更换成员目录的找回默认有 48 小时等待期。如果仍有已加入的管理设备，可以在它的找回提醒上点“立即生效”并再次确认。原目录中的有效管理设备也可以否决，等待期内签署的有效否决即使迟到也可能让副本返回原目录。生效后新浏览器成为管理设备，旧恢复文件才被下一把替换。只轮换恢复密钥的操作立即生效，与这里的找回不同。提示“这个恢复文件已作废”时改用最近一次有效的文件。恢复文件不备份业务正文，详见 [撤销与恢复](privacy-security.md#撤销与恢复) 与 [备份与恢复](node.md#备份与恢复)。

**端口被占用。** `init` 和首次启动会先检查 `listen` 和 `local_listen`，提示“端口 X 被占用”，用 `--listen` 或 `--local-listen` 换一个。

不要为了排障删除 `node.sqlite3`、复制运行中的数据目录，或用另一台机器启动同一个数据目录的副本，这些都会造成分叉或丢失历史。
