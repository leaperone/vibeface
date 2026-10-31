# 聊天里的提问与回答

## 0. 用户交互

- 选项先进入本机“已选择”状态，点击“发送回答”才提交完整选项文字。默认不预选，也不把顺序当作推荐。
- 有选项的问题默认收起自由输入，选择“自己写回答”才展开有可见标签的多行输入。无选项直接输入。切换选项保留手写草稿，数字按原文字发送，Enter 和输入法确认都不发送。
- 问题正文首个非空行是具体决定，其余段落说明已知情况、影响和为什么需要本人回答；未知信息明确说明。选项只列真实替代方案，0 到 8 个是协议范围，不是数量目标。
- Web 任务概览保留任务状态，用待处理数量链接代替重复的问题和确认卡片。待处理、聊天与任务详情仍可回答同一问题。
- 发送会回答本题并让 Bot 按回答继续处理。已有回答先显示真实胜者，再判失效；结果未知时只重试原回答，不能换答。

## 1. 行为

- 协调 Agent 要本人拍板时，在拥有该会话的 Bot 聊天里发一条提问：问题、可选答案、所涉设备和项目。提问是会话事件，和消息一样同步。
- 本人的回答（选择或输入后发送）是这个聊天里发给 Bot 的一条消息。网页和手机不把它显示成单独的消息气泡，而是折进所答问题的卡片里。Bot 把它当作普通一轮的输入，输入里写明回答的是哪个问题，再自己执行，例如 `vibeface codex send|respond`。
- 每个问题只有一个回答 ID，由问题 ID 派生。网页、Grok 或重试写的回答都是这一个 ID，所以“已回答”是一个事实，Bot 只为它跑一轮。两端同时写下不同文字时 rank 小者胜，输方网页把自己的回答换成“已由另一端回答：<胜者文字>”，不让它悄悄消失。
- Grok 经现有 webhook 发件箱收到同一个问题，经现有 `POST /api/decision` 回答，落成聊天里的同一条回答。任一侧回答后，提问方 Node 再经发件箱通知 Grok 已回答。
- 待处理页列出全部会话里未回答的问题，可以就地回答，并能跳到对应聊天。

## 2. 会话归属

Codex 线程或 Claude Code 会话（统称 session）属于某个 Bot，只有两种已记录的事实：

1. 工作记录：`WorkProgress { thread_id }` 由目标设备写入工作所在会话。派发即归属，不新增字段。App Server 的请求可能先于这条记录到达，所以运行它的设备在记录写入前以本机工作已绑定的线程为准。
2. 归属：`SessionAttributed` 事件，写入接手 Bot 的会话。只有运行该 session 的设备能写，签名形状要求 `node` 等于作者。

`vibeface codex send` 是 Bot 在本聊天里登记的工作，`WorkDispatched.thread` 记下目标线程；它和其他工作一样经 `WorkProgress` 归属。

两者都投影到 `session_owners`，主键 `(network_id, node, session_id)`。有 `SessionAttributed` 时 rank 最大的一条为准（后来的交接胜出），否则工作记录里 rank 最小者胜。没有归属的 session 是“外来会话”。

外来会话：

- 只看用户在用的会话。Codex rollout 首行 `session_meta` 的 `source` 为 `exec`/`mcp`、`source` 是子代理对象或 `originator` 以 `_exec` 结尾时跳过；App Server 的 `thread/started` 按同样规则记下后台线程；hook 进程按 rollout 首行（Codex）或 `CLAUDE_CODE_ENTRYPOINT=sdk-*`（Claude）在发出前跳过。普通 Stop 事件照常进分诊。
- 第一个事件到来时，运行它的 Node 直接把它归给主 Bot，写一次 `SessionAttributed`，不提问。归属已存在（含不再关注）就不再写，所以重复事件、重启和同步都不会再写；所属 Bot 被删除后，它名下的会话（含不再关注的）读取时视为无主，下一个事件到来时同样归给当时的主 Bot、只写一次，不在删除时批量改写；之后按普通归属路由，App Server 请求交给该 Bot 处理。没有 Bot 时事件丢弃，请求立即按方法兜底。
- 协调 Agent 自己的线程登记在 `coordination_threads`，永不清理，不会被归属。
- 交接由 Bot 自己判断，网页和手机都不做手动路由。Bot 每轮的 `bot.sessions` 列出它名下的会话（设备、会话 ID、版本、项目、摘要、是否还在关注），按最新优先截在 3000 字节内，省略条数记在 `sessions_omitted`。它用工具 `hand_over_session` 转给另一个 Bot（写新的 `SessionAttributed`），或不再关注（写 `SessionMuted`）。只有会话当前所属的 Bot 能操作，且要带上它看到的版本；运行该会话的设备在同一事务里核对归属和版本再写，不在本机时经节点间 RPC `hand_over_session` 转过去。请求的状态已成立时不写，所以重试只读回；结果未知时用同样的参数再调一次即对账。版本已变（例如对方已把会话转回来）时不写，错误里给出当前版本，所以两个 Bot 来回交接时过期的重试不会再写一次。
- 不再关注后该会话的事件在分诊前丢弃，不进待处理。它仍在原 Bot 名下，主 Bot 不会重新认领，原 Bot 被删除时除外；原 Bot 转给某个 Bot，或转给自己（重新关注）即恢复。
- 协调规则写在 Bot 的固定提示词里：无主会话归主 Bot；会话属于另一个 Bot 的领域时，先用 `message_bot` 商量，对方同意后再转；其他 Bot 的消息是同事的请求，不是本人授权；转走后不再处理该会话。
- Bot 聊天里仍列出归给它的会话，只显示项目、摘要和是否关注，没有转交按钮。
- 不再关注存成 DAG 事件而不是本机表：归属本身是全网同步的事实，任一设备的 Bot 和网页都看到同一状态，重启和重建投影后仍在，也和归属一样只由运行设备写。

主 Bot：

- 每个个人网络一个，由用户在 Bot 页“设为主 Bot”选定，是同步的设置。未选或所选 Bot 已不存在时，用最早创建的 Bot。
- 外来会话直接归主 Bot。换主 Bot 后，下一个外来会话归新主 Bot，已归属的不变。

## 3. 数据形状

```text
const BOTS_SCOPE = "bots" // 网络级的 Bot 设置

PrimaryBotSet {                 // 写在 BOTS_SCOPE，须本人签名
	conversation_id: ScopeId,         // 主 Bot 的会话
}

Subject {                             // 问题所涉的设备、项目和 session
	node: DeviceId,                   // 运行该 session 的设备
	agent: String,                    // "codex" | "claude"
	session_id: String,               // Codex thread ID 或 Claude session ID
	cwd: String,                      // "workspace-N/…"：该设备第 N 个可用文件夹（roots，从 1 数）下的相对路径，不在任何文件夹下时为 "workspace"；网页和手机按该设备公开的 roots 还原成文件夹名
	summary: String,                  // 一行，session 最近在说什么
	request_id: Option<String>,       // 该设备上挂起的 App Server 请求
}

Choice {
	label: String,                    // 1-120 字符，同一问题内唯一
}

QuestionAsked {                       // 写在拥有该 session 的 Bot 会话
	question_id: String,              // "q-" + digest(node, event_id)
	severity: Severity,               // info | attention | urgent
	text: String,                     // 问题本身，≤1024 字节
	choices: Vec<Choice>,             // ≤8；文字回答始终可用
	about: Subject,
}

MessagePosted {                       // 现有事件，新增一个字段
	...,
	answers: Option<String>,          // 仅 role=user：所答问题；此时 message_id 必须等于 "a-" + question_id
}

SessionAttributed {                   // 写在接手 Bot 的会话 scope；rank 最大者为准
	node: DeviceId,
	agent: String,
	session_id: String,
	cwd: String,
	summary: String,
}

SessionMuted {                        // 写在当前接手 Bot 的会话 scope：不再关注
	node: DeviceId,
	session_id: String,
}
```

签名规则：普通本人消息仍须设备签名。回答消息签名可选：网页回答带本人签名，Grok 经 decision token 回答由收到它的 Node 无签名写入。回答一轮的触发类型为 `answer`，不提供改设置的工具。

投影：

| 表 | 主键 | 规则 |
|---|---|---|
| `questions` | `(network_id, question_id)` | 先到者胜；存会话 ID、事件体、作者、时间 |
| `messages.answers` | 新列，索引 `(network_id, answers)` | 回答即消息；同 ID 先到者胜 |
| `session_owners` | `(network_id, node, session_id)` | 来自 `WorkProgress.thread_id`（rank 最小者胜）与 `SessionAttributed`（rank 最大者胜，覆盖工作记录）；`muted` 由同一 Bot 会话里 rank 更大的 `SessionMuted` 置位，新的 `SessionAttributed` 清除 |

未回答 = `questions` 左连接 `messages.answers` 为空。不另设“已回答”状态。

主 Bot 不建表，也不改数据纪元。读取时取 `BOTS_SCOPE` 里 rank 最大的 `PrimaryBotSet`，所指 Bot 仍在则用它，否则用最早创建的 Bot。旧版本 Node 不认识这个事件，升级前会拒收。

删除：`AttentionRaised`、`AttentionResolved`、`AlertResolution`、`ChoiceLabel`、`ATTENTION_SCOPE`，表 `alerts`、`alert_resolutions`、`triage_cards`、`triage_matters`、`decision_journal`，`runtime/decision.rs`，RPC `alerts`、`resolve_alert`、`send_decision`，以及 matter 合并窗口。

## 4. 接口

Agent 侧（CLI → `/api/triage`）：

- `vibeface triage raise --event-id ID --severity S --question TEXT [--choice LABEL]... [--request-id R]`：写入拥有该 session 的 Bot 会话。同一 session 同一请求已有未答问题时返回原问题。带 `--request-id` 时请求挂起到回答为止，回答后再给 Bot 120 秒应答，超时按方法回退。
- `vibeface triage dismiss --event-id ID`：不变。
- `vibeface respond REQUEST_ID RESULT_JSON [--node DEVICE]`：带 `--node` 时经设备转发，应答仍只一次。本机 Node 地址参数为 `--node-url`。
- 协调工具 `hand_over_session { node, session_id, version, to? | stop_following? }`：Bot 把名下的会话转给 `to`（Bot 的会话 ID 或名称），或不再关注；`version` 是 `bot.sessions` 里的版本，即最后改动归属的事件 hash 前 16 位。返回 `{node, session_id, conversation_id, muted, version, changed}`，状态已成立时 `changed = false`、不写事件；版本已变时拒绝。

用户侧 RPC：

- `questions`：全网未回答问题，含所属会话与 Bot 名。
- `answer { question_id, text, attestation }`：写回答消息。会话问题按普通消息准入或转交调度 Node；同一问题已有不同回答时返回已有回答，`status = "already_answered"`。
- `conversation` 快照新增 `questions`（含回答）和 `sessions`（归给本 Bot 的外来会话 `{node, agent, session_id, cwd, summary, muted, created_at_ms}`）。
- `bots` 返回 `primary`，即主 Bot 的会话 ID，没有 Bot 时为 `null`。
- `set_primary_bot { conversation_id, attestation }`：本人选定主 Bot，签名事件体为 `{"type":"primary_bot_set", conversation_id}`，scope 为 `bots`。

Grok 侧：

- webhook 事件 `{"type":"question", question_id, severity, text, choices[{index,label}], about, bot, answer:{url, device_id, question_id}}` 和 `{"type":"answered", question_id, text}`，`event_id` 分别为问题 ID 与 `answered-<问题 ID>-<回答事件哈希前 16 位>`。两端同时回答时 rank 小者胜，同步把输方本机的回答换成胜者；提问方 Node 按新的回答再发一次 `answered`，未送出的旧 `answered` 撤下。接收方以同一 `question_id` 最后收到的为准。
- `POST /api/decision { device_id, question_id, choice | text }`：返回 `{question_id, answer_id, status: "answered" | "already_answered", text}`。重试同一请求只读回。

Bot 一轮的输入：`trigger.event` 是回答消息，另附 `trigger.question`（问题全文、选项和 Subject），说明它回答哪个问题。提示要求 Bot 用回答消息的 `message_id` 作为 `vibeface codex send --id`。这次发送是这一轮的工作，同一轮重试只读回；回合结束后结果作为 `work_result` 轮回到 Bot。
