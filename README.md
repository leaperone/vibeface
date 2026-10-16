# Vibeface

在 Mac mini 上运行 Rust Bridge，用 Cursor 管理 Agent 聊天、安排 Codex 任务，并保存任务状态。当前通过本机命令行或局域网 HTTP API 使用，不需要公网 Server。

```text
本机 CLI ────────┐
                ├→ Rust Bridge ⇄ Codex App Server
局域网 HTTP API ─┘       ⇅
                   Cursor SDK 管理 Agent
```

Bridge 负责状态、事件、预算和控制规则。Cursor 负责理解指令、安排任务和总结。SDK 程序运行在 Mac mini，模型请求发送到 Cursor 服务，使用 Cursor 自己的 API key、额度与计费。

`crates/bridge` 是本地运行入口，`crates/protocol` 定义命令与记录。`crates/server` 保留为后续公网转发与认证扩展，本地使用不依赖它。手机和网页聊天界面暂不实现。

## 构建 Bridge

需要 Rust 1.88+、macOS 或 Linux，以及已运行的 Codex App Server。Bridge 通过 Unix socket 连接现有 Codex 实例，不直接修改 Codex 数据库。

```sh
cargo build -p vibeface-bridge --locked
```

输出为 `target/debug/vibeface-bridge`。

## 启动本机 Bridge

在仓库根目录运行：

```sh
./target/debug/vibeface-bridge run \
	--state-dir "$HOME/.local/state/vibeface" \
	--allowed-root /Volumes/ssd/project
```

默认 Codex socket 是 `$HOME/.codex/app-server-control/app-server-control.sock`，控制 socket 是状态目录内的 `control.sock`。需要其他路径时传 `--codex-socket` 或 `--socket`。省略 `--server` 时，Bridge 独立运行，不连接远程 Server。若环境仍配置了旧的 `VIBEFACE_BRIDGE_TOKEN`，本地运行前先取消该变量；远程 Server 地址和 bridge token 必须成对配置。

`--allowed-root` 限制新建会话和任务的工作目录，不隔离已有 Codex 历史。能够访问控制 socket 的本机用户可以管理该实例的已有会话。

在另一个终端查询状态：

```sh
./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"status-1","command":{"kind":"snapshot"}}'
```

检查结果中的 Codex 连接状态。Bridge 进程存活不代表 Codex 已连接。未配置 Cursor 时，直接查询和控制 Codex 仍可使用，`manage` 会返回管理 Agent 未启用。

命令 ID 用于去重。重试必须保留相同 ID 和内容，不能将示例中的同一 ID 用于另一条指令。结果为 `unknown` 时，先查命令、任务与 Codex 状态，不换 ID 盲目重发写操作。

## 启用 Cursor 管理聊天

从 [官方 SDK Bridge v1.0.37 发布](https://github.com/cursor/sdk-bridge/releases/tag/v1.0.37) 下载对应平台的独立包，用同页 `SHA256SUMS.txt` 校验后解压。程序位于 `bin/cursor-sdk-bridge`。应用代码是 Rust，通过本地 HTTP/1.1 Connect JSON 调用官方程序，不包装 Cursor CLI，也不使用 Cloud Agents API。

在 Bridge 的环境中配置 `CURSOR_API_KEY`。不要将密钥放进命令行、URL、Git 或日志。然后启动：

```sh
./target/debug/vibeface-bridge run \
	--state-dir "$HOME/.local/state/vibeface" \
	--allowed-root /Volumes/ssd/project \
	--agent-bin /path/to/cursor-sdk-bridge \
	--agent-model composer-2.5 \
	--agent-daily-limit 20
```

SDK 必须显式指定模型，也可设置 `VIBEFACE_AGENT_MODEL`。`composer-2.5` 是示例，请选择账户可用模型。API key 不自动复用 Cursor App 登录。SDK 调用有上下文成本，不保证比 Grok 更省。

提交聊天消息，再读取保存的聊天记录：

```sh
./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"chat-1","command":{"kind":"manage","params":{"prompt":"查看相关任务的最新状态，汇总需要我决定的问题，不要创建或续跑任务。"}}}'

./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"history-1","command":{"kind":"chat_history","params":{"after":0,"limit":100}}}'
```

管理聊天保存用户消息、Agent 回复和 Cursor 会话 ID。相同状态目录下的后续 `manage` 继续该会话。单次回复成功不代表 Codex 任务完成。

Cursor 通过 Bridge 的 stdio MCP 管理 Codex。SDK 仅启用 MCP 工具，不提供 shell、文件编辑或子 Agent 工具，也不加载项目和用户的其他 MCP 配置。Codex 审批由用户回答，Cursor 工具没有审批能力。

若 SDK 的 `CreateAgent` 或模型列表超时，检查继承的代理环境。本机真实调用曾在清除 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 及对应小写变量后恢复，系统代理无需更改。

## 创建任务并授权自动继续

先创建有目标和验收条件的任务。以下任务不会自动调用模型或续跑：

```sh
./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"create-task-1","command":{"kind":"create_task","params":{"task":{"id":"project-a","goal":"检查项目 A 的当前状态并整理后续工作","cwd":"/Volumes/ssd/project/project-a","acceptance_criteria":["说明当前状态，并列出需要用户决定的问题"],"auto_continue":false,"decision_limit":0}}}}'
```

将示例目录替换为 `--allowed-root` 内的实际目录。需要自动继续时，由用户显式授权决策次数：

```sh
./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"resume-task-1","command":{"kind":"control_task","params":{"task_id":"project-a","action":{"kind":"resume","auto_continue":true,"decision_limit":3}}}}'

./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"read-task-1","command":{"kind":"read_task","params":{"task_id":"project-a"}}}'
```

Bridge 在任务需要处理时唤醒 Cursor，Cursor 通过绑定该任务的工具操作 Codex。普通进度采集不调用模型。任务的剩余决策次数和 UTC 每日调用上限同时生效，失败调用也消耗已预留次数。次数上限不是 token 或费用硬上限。

审批、未知结果、任务暂停和等待验收会停止自动继续。`pause` 停止后续调度，不会中断已经运行的 Codex turn。要停止正在执行的工作，用户需明确发送 `interrupt_turn`。

Cursor 最多将任务标为 `awaiting_review`。用户核对证据后，才能将任务验收为 `completed`：

```sh
./target/debug/vibeface-bridge call \
	--socket "$HOME/.local/state/vibeface/control.sock" \
	--json '{"id":"accept-task-1","command":{"kind":"control_task","params":{"task_id":"project-a","action":{"kind":"accept","evidence":"已检查输出与验收条件，确认结果符合要求。"}}}}'
```

验收要求任务处于 `awaiting_review`，且没有活动 turn、未决命令或未知命令。Codex 的 `turn/completed` 只表示一轮结束，不能自动触发业务验收。

Grok 原生直连通道可继续保留。直连会绕过 Bridge 的任务控制，不能保证与 Cursor 互斥。交接时先停止 Grok 对同一任务的写操作，再让 Cursor 读取原会话状态继续。

## 开放局域网 API

在本机启动参数中增加 `--listen`，并配置独立的 HTTP bearer token：

```sh
export VIBEFACE_CLIENT_TOKEN="$(openssl rand -hex 32)"
./target/debug/vibeface-bridge run \
	--state-dir "$HOME/.local/state/vibeface" \
	--allowed-root /Volumes/ssd/project \
	--listen 0.0.0.0:8788 \
	--agent-bin /path/to/cursor-sdk-bridge \
	--agent-model composer-2.5
```

将同一 token 安全配置到局域网调用端。以下请求把凭证经标准输入传给 curl，避免把 token 放进 URL 或进程参数。将 `MAC_LAN_IP` 替换为 Mac mini 的局域网 IPv4 地址：

```sh
curl --silent --show-error --config - <<EOF
url = "http://MAC_LAN_IP:8788/v1/status"
header = "Authorization: Bearer ${VIBEFACE_CLIENT_TOKEN}"
EOF
```

提交管理消息返回 HTTP 202 和命令记录，之后按 ID 查询结果：

```sh
curl --silent --show-error --config - \
	--request POST \
	--header 'Content-Type: application/json' \
	--data '{"id":"lan-chat-1","command":{"kind":"manage","params":{"prompt":"汇总需要我决定的问题，不要续跑任务。"}}}' \
	http://MAC_LAN_IP:8788/v1/commands <<EOF
header = "Authorization: Bearer ${VIBEFACE_CLIENT_TOKEN}"
EOF

curl --silent --show-error --config - <<EOF
url = "http://MAC_LAN_IP:8788/v1/commands/lan-chat-1"
header = "Authorization: Bearer ${VIBEFACE_CLIENT_TOKEN}"
EOF
```

HTTP API 与本机 socket 共用账本、任务和管理会话。订阅事件：

```sh
curl --silent --show-error --no-buffer --config - <<EOF
url = "http://MAC_LAN_IP:8788/v1/stream?after=0"
header = "Authorization: Bearer ${VIBEFACE_CLIENT_TOKEN}"
EOF
```

SSE 持续输出事件。客户端只将 `event: notification` 作为提醒，其余事件用于更新聊天与进度。Bridge 在任务需要人工处理、等待验收、用户验收完成、命令结果未知或 Codex 连接改变时生成持久通知。同一状态不重复提醒，普通内容片段与心跳不提醒，也不调用模型。通知是规则模板，字段与过滤方式见 [本地 HTTP 与命令契约](docs/api.md)。

HTTP bearer 只校验持有者，不加密传输；仅在可信局域网使用该明文入口。

## 恢复状态

退出 Bridge 后，用相同 `--state-dir` 和 Codex socket 重新启动。Bridge 保留命令、任务、聊天记录、事件、Cursor 会话 ID 和每日调用预算。任务需要恢复 Codex 连接并完成状态核对后才能继续。

结果未知的写操作不会盲目重放。用户先检查实际结果，再用 `resolve_command` 附证据确认该不确定命令，之后显式恢复任务。确认会将账本记录标为 failed，不重放原操作，也不表示原操作实际没有发生。

Codex 断线期间未收到的通知无法完整恢复，重连会核对已跟踪会话的当前状态。`--auto-manage` 是独立的只读摘要选项，不授予任务自动继续权限。

状态目录和 SQLite 包含聊天及任务内容，使用私有文件权限。尚无自动历史清理，需监控磁盘占用。不要直接编辑 Bridge 或 Codex 数据库。

## 验证

本地验收区分 Rust 行为检查、模拟 SDK 与 Codex 的集成检查，以及真实 Cursor 模型调用。模拟通过不能证明真实账号、模型或网络可用。

真实链路已验证 Cursor 通过 Bridge 驱动隔离 Codex 任务、独立核对产物、等待用户验收后完成，以及重启不额外调用模型。HTTP 认证与状态接口已通过本机访问局域网地址验证，第二台设备与手机尚未测试。

```sh
cargo fmt --all -- --check
cargo test -p vibeface-protocol -p vibeface-bridge --locked
cargo clippy -p vibeface-bridge --all-targets --locked -- -D warnings
```

当前范围不包含公网 Server、聊天 UI、手机推送和额外的 decision 模型。具体测试结果以本次交付报告为准。
