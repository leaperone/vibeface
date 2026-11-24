# 本机执行网关

外部程序可以直接在 Node 上提交 Codex 或 Claude Code 工作。调用只需要该 Node 签发的 caller key。Node 可以没有 Bot、会话或个人网络成员身份。


## 与现有 Gate 的关系

仓库里没有名为 `gate.rs` 的文件。外部入口进入 Node 的既有中台是 `apps/core/src/maintenance.rs` 里的 `maintenance::Gate`（以及同文件和 `crates/control/src/maintenance.rs` 的本机 owner maintenance socket）。`Runtime` 上的调度、工具、设置、投递、分诊和 HTTP `boundary` 已经通过 `Gate::enter()` / `WorkGuard` 准入：更新进行中时新的工作被拒绝（`Busy`），避免和执行重叠。

执行网关是这个 Gate 上的一层门面，而不是另一套并发或准入机构。HTTP 与 MCP 走同一 `boundary`：先 `enter()` 确认 Node 开放。`wait`、`queue` 和 MCP 可能阻塞到超时，因此只在准入时取租赁并立即放下，做法与现有 `/api/hooks/wait` 相同，以免挡住更新；`start` / `steer` / `stop` 在实际改状态的短临界区再次 `enter()`。`Busy` 对外映射为 `unavailable`。

本机 owner 的 maintenance socket（仅数据目录所有者、Unix 套接字，供 `vibeface` CLI 使用）以后可以挂上同一套 `gateway_call` 操作，用来签发 caller 或在本机调试；那是规范化，而不是第二套协议。远程调用方（如 Grok Bot）仍走 HTTP/MCP 和 caller key，因为 maintenance socket 不能作为公网入口。

`crates/client/src/write_gate.rs` 是客户端对目标 Node 写权限（Writable / Restricted / Forked）的观察，与这条执行门面不是同一层，不要混用。

## 接口

所有请求都使用 `Authorization: Bearer <key>`。POST 请求使用 `Content-Type: application/json`。本机 owner 通过本机监听地址调用 `POST /api/gateway/v1/callers`，可传 `can_write`（默认 false）和成对的 `webhook_url`、`webhook_secret`。成功时返回 `caller_id` 和仅这次可见的 `key`。其余接口只接受 caller key。每个调用方只能查询、控制和继续自己在网关中创建的工作。

| HTTP 操作（前缀 `/api/gateway/v1`） | 请求与结果 |
| --- | --- |
| GET `/capabilities` | 提供各执行器的启用状态、操作、沙箱、覆盖参数和限制。 |
| POST `/start` | 必填 `call_ref`、`provider`、`cwd`、`prompt`。可传 `title`、`model`、`effort`、`write`，以及 `continues`（前序 work_id）或 `thread` 之一。返回已登记工作的快照。 |
| POST `/status`、`/result` | 传 `work_id` 读取最近一次持久化的快照。 |
| POST `/wait` | 传 `work_id` 和 `timeout_ms`（1 至 120000）。结束、需要输入或到期时返回快照。HTTP 请求取消即结束等待。 |
| POST `/steer` | 传 `work_id`、`call_ref` 和 `prompt`，向已确认的活动回合发送输入。重复请求读取送达证据。 |
| POST `/queue` | 传前序 `work_id`、新的 `call_ref`、`prompt` 和 `timeout_ms`。在请求内等待该线程空闲，再沿用原执行器、cwd、模型和沙箱登记下一轮。等待超时返回 `timeout`，没有后台排队任务。 |
| POST `/stop` | 传 `work_id` 请求中断。执行器接受请求后可返回 `interrupting`；只有探测到回合结束才返回终态。 |

```json
{"call_ref":"review-42","provider":"codex","cwd":"/projects/demo","prompt":"检查最近的改动"}
```

一次工作以 caller 和 `call_ref` 定位。相同请求可安全重试，复用同一标识修改执行参数会得到 `idempotency_conflict`。REST 错误体包含 `code` 和 `message`。快照包含 `work_id`、`state`、`outcome`、`thread`、`sandbox`、`final_reply`、`changed_files` 和 `failed_commands`。Codex 从终态回合取最多 128 个已更改文件和最后 5 条失败命令，每条输出最多 1024 字符。Claude 的回合探测只提供文本和执行结果，两个明细数组为空。传输中断后，`unverified` 表示证据不足，可继续使用原 work_id 查询。交互审批暂不支持，Codex 的输入请求会记录 `needs_input` 并使用拒绝或取消应答。

MCP 使用同一监听器的 `POST /api/gateway/mcp` 和相同的 caller key。客户端先发送 JSON-RPC `initialize`，再用 `tools/list` 查看上述八项工具的输入 schema。通过 `tools/call` 调用 `capabilities` 可获取当前节点的限制。工具调用与 HTTP 共用权限校验、幂等登记及状态。

## 执行与持久化

`runtime::executor_gateway` 负责鉴权后的准入、发起、跟进和恢复。记录保存在 `<data-dir>/gateway/gateway.sqlite3`，仅属于本机，包含调用方密钥摘要、权限、签名回调配置、工作、控制请求和待投递事件。工作标识采用 `WorkSpec::id("gateway:{caller_id}", "gateway", call_ref)`。侧库调用已有的 `work::accept` 和 `transition`，保留原生线程绑定，独立于 DAG、Bot 会话和 `work_runs`。恢复时只查询原生回合。

- `cwd` 必须是已存在的目录，解析符号链接后位于 Node 配置的执行根目录内。两个提供方均须在本机启用。写入需要调用方具有 `can_write`，并在本次 start 显式指定 `write: true`。在原线程续作时沿用该工作的目录和沙箱。
- Codex 通过现有 Executor 保留 App Server。Node 发送 `thread/start` 或 `thread/resume`，在侧库记录线程，再发送包含 `clientUserMessageId=work_id` 的 `turn/start`。线程及回合均指定 `approvalPolicy=never`。默认使用 `read-only`，授权写入后使用 `workspace-write`，回合禁用网络并排除系统临时目录的写权限。
- Claude 由现有 Executor 启动 CLI 并跟随其输出。网关要求 CLI 支持 `--restricted --safe-mode --strict-mcp-config`。只读模式使用 `--permission-mode dontAsk --tools Read,Glob,Grep`，写模式使用 `acceptEdits` 并增加 `Edit,Write`。文件工具受 CLI 工作目录限制，两种模式均不开放 Shell。缺少这些参数的 CLI 会拒绝启动。

## 回调

终态和需要输入分别产生 `terminal` 和 `needs_input` 事件。状态与 outbox 在同一事务提交。正文为 `{"type":"terminal","event_id":"…","work":{…}}`，其中 `work` 与查询接口使用相同快照。目标 URL、密钥和正文在入队时固定。接收方使用原始请求体按以下头部验签，再按事件 ID 去重。

| 请求头 | 含义 |
| --- | --- |
| `x-vibeface-event-id` | 同一工作和结果的稳定标识。 |
| `x-vibeface-timestamp` | 本次投递的 Unix 秒数。 |
| `x-vibeface-signature` | `v1=` 后接 `HMAC-SHA256(secret, timestamp + "." + raw_body)` 的十六进制摘要。 |

HTTP 2xx 表示送达。传输失败、408、429 和 5xx 从 10 秒起指数退避，最多尝试 8 次；其他 HTTP 错误直接进入死信。投递是至少一次，回调重试只重发同一事件。

跨 Node 调度返回 `cross_node_deferred`。Bot 迁移、把同一 `gateway_call` 挂到 owner maintenance socket、Pi、OpenCode、Cursor、图片输入以及进程级 `--chat-agent` 联调留待后续版本。
