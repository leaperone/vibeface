# 本地 HTTP 与命令契约

Bridge 的本机 Unix socket 和可选 HTTP API 共用命令账本、任务、聊天记录与事件。HTTP 在 `run --listen ADDRESS:PORT` 时启用，使用 `VIBEFACE_CLIENT_TOKEN` 校验 `Authorization: Bearer <token>`，不接收 URL query 中的凭证。Unix socket 依靠私有目录与文件权限。

## HTTP 路径

| 路径 | 行为 |
|---|---|
| `GET /healthz` | 无认证的进程存活探针，不表示 Codex 可用 |
| `GET /v1/status` | Codex 连接、已知会话、未决问题、任务数量和管理预算 |
| `POST /v1/commands` | 提交 `CommandRequest`，通常返回 202 和命令记录；已有 `unknown` 记录返回 409 |
| `GET /v1/commands/{id}` | 查询命令状态与结果 |
| `GET /v1/tasks` | 查询持久任务记录 |
| `GET /v1/chat?after=0&limit=100` | 按聊天序号读取持久消息 |
| `GET /v1/events?after=0&limit=100` | 按事件序号读取持久事件 |
| `GET /v1/stream?after=0` | SSE，支持 `Last-Event-ID` |

除 `/healthz` 外的 HTTP 路径都需要 bearer。启动时 token 至少为 24 字符，不能包含空白。请求体上限是 128 KiB。

聊天序号与事件序号独立，`after` 不可混用。分页 `limit` 默认 100，范围 1 至 1000。`after` 不能超过有符号 64 位整数上限。SSE 的事件 ID 是 Bridge 事件序号，重连使用最后收到的序号，`Last-Event-ID` 优先于 query 中的 `after`。断线期间尚未采集的 Codex 通知无法凭空恢复。

参数无效返回 400，缺失或错误 bearer 返回 401，未知命令 ID 返回 404。同一命令 ID 提交不同内容返回 409 错误。相同 ID 与内容重试时，若已有记录是 `unknown`，返回 409 与该命令记录，不重新执行。HTTP 202 不代表执行成功，后续执行错误写入命令记录。

## 命令形状

`id` 由调用者生成。重试必须保持 ID 与内容一致，异内容复用 ID 会被拒绝。ID 允许 1 至 128 个 ASCII 字母、数字、连字符、点或下划线。

```json
{"id":"read-1","command":{"kind":"read_thread","params":{"thread_id":"THREAD_ID"}}}
```

命令记录包含 `request`、`status`、可选 `output` 与 `error`，以及 `updated_at_ms`。状态为 `queued`、`running`、`succeeded`、`failed` 或 `unknown`。命令成功仅表示该调用已完成。

| kind | params |
|---|---|
| `snapshot` | 无，查看连接、会话、未决请求与预算 |
| `manage` | `prompt`，交给持久 Cursor 管理聊天 |
| `chat_history` | 可选 `after`，默认 0；可选 `limit`，默认 100，范围 1 至 1000 |
| `list_tasks` | 无 |
| `read_task` | `task_id` |
| `create_task` | `task`，见下文 |
| `update_task` | `task_id`、`update`，见下文 |
| `control_task` | `task_id`、`action`，见下文 |
| `resolve_command` | `command_id`、非空 `evidence`，用户确认已核对结果未知的命令，不重放 |
| `task_command` | `task_id`、`command`，将 Codex 命令绑定到任务 |
| `list_threads` | 可选 `cursor` |
| `read_thread` | `thread_id` |
| `resume_thread` | `thread_id`，加载并订阅 |
| `list_turns` | `thread_id`，可选 `cursor` |
| `start_thread` | `cwd`，须为已存在且位于 allowed-root 下的目录 |
| `start_turn` | `thread_id`、`text` |
| `steer_turn` | `thread_id`、`turn_id`、`text` |
| `interrupt_turn` | `thread_id`、`turn_id` |
| `respond_request` | `request_id`、`response`，仅当前仍有效的 Codex 反向请求 |

`run_task` 是内部调度命令，拒绝外部提交。`task_command.command` 只接受 `read_thread`、`resume_thread`、`list_turns`、`start_thread`、`start_turn`、`steer_turn` 和 `interrupt_turn`。任务工具的写操作受任务状态和授权检查。

`resolve_command` 仅接受 `unknown` 记录，将记录标为 `failed` 并保存用户证据。这个状态表示用户已处理不确定性，不证明原操作没有发生。任务仍需 `control_task` 显式恢复。

`respond_request` 的 ID 保留 Codex 原始数字或字符串类型。审批、用户输入和 MCP elicitation 的结果结构不同，调用者按原请求类型提供 `response`，不能通过新 turn 发送“同意”代替 RPC 回包。Cursor 的 MCP 工具没有该操作。

## 任务字段与控制

`create_task.params.task` 使用以下字段，不接收未知字段：

| 字段 | 类型与约束 |
|---|---|
| `id` | 唯一任务 ID，使用命令 ID 相同字符约束 |
| `goal` | 非空字符串，最多 65536 字节 |
| `cwd` | 已存在且位于 allowed-root 下的目录 |
| `acceptance_criteria` | 1 至 32 个非空字符串，每项最多 65536 字节 |
| `auto_continue` | 布尔值，默认 false |
| `decision_limit` | 整数，默认 0，最多 1000；自动继续时至少为 1 |

Cursor 的创建工具只创建未授权自动继续的任务。用户通过 API 或本机命令显式授予自动继续权限。

`update_task.params.update` 支持可选的 `status`、`summary`、`evidence` 和 `thread_ids`。`thread_ids` 最多包含 64 个线程 ID，不能删除已有绑定或复用其他任务的线程。已完成任务不能修改，停止的任务必须经 `control_task` 恢复。更新命令不能将任务标为 `completed`。

任务状态如下：

| 状态 | 含义 |
|---|---|
| `queued` | 等待首次处理 |
| `running` | 工作正在执行 |
| `needs_continue` | 等待下一次决策 |
| `waiting_user` | 等待用户输入或审批 |
| `blocked` | 工作受阻，停止自动继续 |
| `paused` | 用户暂停了后续调度 |
| `awaiting_review` | 等待用户核对验收条件与证据 |
| `completed` | 用户已明确验收 |
| `unknown` | 存在未确认的执行结果，停止自动继续 |

只有 `queued`、`running`、`needs_continue` 可以参与自动调度，并且必须有显式 `auto_continue` 授权与剩余决策次数。每次模型调用还受 UTC 每日调用预算限制，失败调用也计数。状态读取、事件采集与普通进度更新不调用模型。

`control_task.params.action` 是带 `kind` 的对象：

| kind | 其他字段与行为 |
|---|---|
| `pause` | 无其他字段，停止后续调度；不自动中断活动 turn |
| `resume` | 必填 `auto_continue`、`decision_limit`，用户重新授权并设置剩余决策次数 |
| `accept` | 必填非空 `evidence`，仅 `awaiting_review` 且无活动 turn、未决命令或未知命令时允许 |

任务记录保存目标、验收条件、关联 `thread_ids`、状态、`auto_continue`、`remaining_decisions`、摘要、证据、事件处理序号、Cursor 会话 ID 和更新时间。Bridge 使用相同状态目录重启后保留这些记录。`turn/completed` 不会直接将任务标为 `completed`。

## 聊天与事件

聊天记录包含 `seq`、`command_id`、`role`、`content` 和 `at_ms`。`manage` 保存用户消息与对应的 Agent 结果。Cursor 会话 ID 在发送管理指令前持久化，恢复失败不会自动创建另一个会话重复写操作。

事件包含 `seq`、`at_ms`、`kind`、可选 `thread_id` 与 `turn_id`，以及 `payload`。`chat.message` 包含保存的聊天消息。`agent.progress` 包含管理过程片段，`agent.completed` 包含最终管理结果，`agent.failed` 包含管理错误，并通过 `uncertain` 表示操作结果是否未知。管理事件的 `command_id` 和 `task_id` 用于关联指令与任务，SDK 未提供 usage 时返回 `null`，不代表零消耗。

原生 `turn/completed` 还需检查 `turn.status`，不能把 interrupted 或 failed 当作成功。连接状态、任务状态和管理模型状态分别报告。

## 规则通知

`notification` 是独立持久事件，与其他事件共用 `seq`，可分页读取或通过 SSE 恢复。规则直接生成模板消息，不调用模型。payload 的字段如下：

| category | reason | 其他字段 |
|---|---|---|
| `task` | `waiting_user`、`blocked`、`paused`、`unknown`、`awaiting_review`、`completed` | `task_id`、`message` |
| `command` | `unknown` | `command_id`、可空 `task_id`、`message` |
| `connection` | `bridge.codex_connected`、`bridge.codex_disconnected` | `source_event_seq`、`message` |

任务通知在上述状态发生改变时生成。未知命令只在首次进入 `unknown` 时提醒。连接通知按持久化的上次连接状态去重。普通内容片段、心跳和状态未变的更新不会触发通知。

```json
{"seq":42,"at_ms":1792100000000,"kind":"notification","payload":{"category":"task","task_id":"project-a","reason":"awaiting_review","message":"Task is ready for acceptance review"}}
```

`GET /v1/stream` 输出全部事件，没有服务端 `kind` 过滤参数。SSE 消费者按 `event: notification` 处理提醒。分页消费者按 JSON 的 `kind == "notification"` 筛选。使用整条事件流的序号保存游标，不能把通知数量作为 `after`。

通知只表示 Bridge 记录了该状态，不证明手机已收到系统推送。当前没有手机后台推送渠道。

## Unix socket 与 MCP

本机调用等待命令返回：

```sh
vibeface-bridge call --socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"status-local-1","command":{"kind":"snapshot"}}'
```

`vibeface-bridge mcp --socket PATH` 提供 stdio MCP，使用标准 JSON-RPC 2.0。Codex App Server 的原生协议省略 `jsonrpc` 字段，两者不同。

Cursor 的会话工具与任务工具都经同一个 Bridge 控制入口。自动任务使用绑定任务的 MCP 能力，不能借其他会话工具绕过该任务的授权。只读摘要模式仅提供读取工具，在调用入口拒绝写操作。工具集不包含用户审批回复或最终验收。

## 暂缓的远程 Server

仓库保留 `crates/server` 的设备级路由、Bridge WebSocket 与双 token 协议。它不参与当前本机和局域网运行链路，公网部署与外部客户端验收另行进行。本地 HTTP bearer 与远程 Bridge 连接 token 是不同用途的凭证。
