# 聊天里的提问与回答

状态为实施中。本文取代分诊卡片（`attention` 范围的 `AttentionRaised`/`AttentionResolved`、`send_decision` 直写 Codex 线程）。项目尚未发布正式版，事件、表和 RPC 直接改成最终形态，`DATA_EPOCH` 升到 10。

## 1. 行为

- 协调 Agent 要本人拍板时，在拥有该会话的助手聊天里发一条提问：问题、可选答案、所涉设备和项目。提问是会话事件，和消息一样同步。
- 本人的回答（点选项或输入文字）是这个聊天里发给助手的一条消息。助手把它当作普通一轮的输入，输入里写明回答的是哪个问题，再自己执行，例如 `vibeface codex send|respond`。
- 每个问题只有一个回答 ID，由问题 ID 派生。网页、Grok 或重试写的回答都是这一个 ID，所以“已回答”是一个事实，助手只为它跑一轮。两端同时写下不同文字时 rank 小者胜，输方网页把自己的回答换成“已由另一端回答：<胜者文字>”，不让它悄悄消失。
- Grok 经现有 webhook 发件箱收到同一个问题，经现有 `POST /api/decision` 回答，落成聊天里的同一条回答。任一侧回答后，提问方 Node 再经发件箱通知 Grok 已回答。
- 待处理页列出全部会话里未回答的问题，可以就地回答，并能跳到对应聊天。

## 2. 会话归属

Codex 线程或 Claude Code 会话（统称 session）属于某个助手，只有两种已记录的事实：

1. 工作记录：`WorkProgress { thread_id }` 由目标设备写入工作所在会话。派发即归属，不新增字段。App Server 的请求可能先于这条记录到达，所以运行它的设备在记录写入前以本机工作已绑定的线程为准。
2. 认领：`SessionAttributed` 事件，写入被认领助手的会话。只有运行该 session 的设备能写，签名形状要求 `node` 等于作者。

`vibeface codex send` 是助手在本聊天里登记的工作，`WorkDispatched.thread` 记下目标线程；它和其他工作一样经 `WorkProgress` 归属。

两者都投影到 `session_owners`，主键 `(network_id, node, session_id)`，先到（rank 最小）者胜。没有归属的 session 是“外来会话”。

外来会话：

- 第一个事件到来时，运行它的 Node 直接写一个确定性的会话提问（不经 Agent），问题是“谁来接这个 Codex 会话？”，附 cwd 和一行摘要。选项为 `忽略`、`处理`，以及每个助手一项 `交给 <助手名>`。它只出现在待处理页和 Grok，不进任何聊天。
- 提问未回答或回答为 `忽略` 时，该 session 之后的事件静默丢弃。`忽略` 按 session 记住，因为问题 ID 由 session 派生。运行它的 Node 把已忽略的 session 记在内存里，事件不碰数据库就丢弃；认领或绑定工作时移出。重启后从数据库重新得知。
- 回答 `交给 <助手>` 时，Node 直接写 `SessionAttributed`。回答 `处理` 或其他文字时，交给主助手判断。
- 认领后该 session 的事件和提问都进入那个助手的聊天，聊天里显示一张交接卡片。

主助手：

- 每个个人网络一个，由用户在助手页“设为主助手”选定，是同步的设置。未选或所选助手已不存在时，用最早创建的助手。
- 没有归属的东西都先交给主助手判断。目前就是回答了 `处理` 的外来会话。
- 判断在主助手自己的聊天里跑一轮（触发类型 `route`，触发 ID `route:<问题 ID>`），用户能看到它的理由。这一轮拿到会话提问全文，以及其他每个助手的名称、项目和最近 5 条消息摘要。
- 它要么留下（`vibeface triage assign` 指向自己），要么交给另一个助手。交接卡片出现在双方聊天里，主助手聊天里写明交给了谁。
- 这一轮结束仍无人认领，或回答后 10 分钟内主助手没有回复时，会话归主助手。不再按项目路径前缀兜底。
- 换主助手后，下一个外来会话的判断轮在新主助手的聊天里跑。已经开始的判断轮不受影响。

外来会话上的 App Server 请求不等人：Node 写会话提问（若尚无）并立即按方法回退应答。

## 3. 数据形状

```text
const SESSIONS_SCOPE = "sessions"     // 外来会话的提问只住在这里
const ASSISTANTS_SCOPE = "assistants" // 网络级的助手设置

PrimaryAssistantSet {                 // 写在 ASSISTANTS_SCOPE，须本人签名
	conversation_id: ScopeId,         // 主助手的会话
}

Subject {                             // 问题所涉的设备、项目和 session
	node: DeviceId,                   // 运行该 session 的设备
	agent: String,                    // "codex" | "claude"
	session_id: String,               // Codex thread ID 或 Claude session ID
	cwd: String,
	summary: String,                  // 一行，session 最近在说什么
	request_id: Option<String>,       // 该设备上挂起的 App Server 请求
}

Choice {
	label: String,                    // 1-120 字符，同一问题内唯一
	conversation_id: Option<ScopeId>, // 仅会话提问：选它即认领给该助手
}

QuestionAsked {                       // 会话 scope = 助手提问；SESSIONS_SCOPE = 外来会话提问
	question_id: String,              // 助手提问 "q-" + digest(node, event_id)；会话提问 "s-" + digest(node, session_id)
	severity: Severity,               // info | attention | urgent
	text: String,                     // 问题本身，≤1024 字节
	choices: Vec<Choice>,             // ≤8；文字回答始终可用
	about: Subject,
}

MessagePosted {                       // 现有事件，新增一个字段
	...,
	answers: Option<String>,          // 仅 role=user：所答问题；此时 message_id 必须等于 "a-" + question_id
}

SessionAttributed {                   // 写在被认领助手的会话 scope
	node: DeviceId,
	agent: String,
	session_id: String,
	cwd: String,
	summary: String,
	question_id: String,              // 促成认领的会话提问
}
```

签名规则：普通本人消息仍须设备签名。回答消息签名可选：网页回答带本人签名，Grok 经 decision token 回答由收到它的 Node 无签名写入。回答一轮的触发类型为 `answer`，不提供改设置的工具。

投影：

| 表 | 主键 | 规则 |
|---|---|---|
| `questions` | `(network_id, question_id)` | 先到者胜；存会话 ID、事件体、作者、时间 |
| `messages.answers` | 新列，索引 `(network_id, answers)` | 回答即消息；同 ID 先到者胜 |
| `session_owners` | `(network_id, node, session_id)` | 来自 `WorkProgress.thread_id` 与 `SessionAttributed`，先到者胜 |

未回答 = `questions` 左连接 `messages.answers` 为空。不另设“已回答”状态。

主助手不建表，也不改数据纪元。读取时取 `ASSISTANTS_SCOPE` 里 rank 最大的 `PrimaryAssistantSet`，所指助手仍在则用它，否则用最早创建的助手。旧版本 Node 不认识这个事件，升级前会拒收。

删除：`AttentionRaised`、`AttentionResolved`、`AlertResolution`、`ChoiceLabel`、`ATTENTION_SCOPE`，表 `alerts`、`alert_resolutions`、`triage_cards`、`triage_matters`、`decision_journal`，`runtime/decision.rs`，RPC `alerts`、`resolve_alert`、`send_decision`，以及 matter 合并窗口。

## 4. 接口

Agent 侧（CLI → `/api/triage`）：

- `vibeface triage raise --event-id ID --severity S --question TEXT [--choice LABEL]... [--request-id R]`：写入拥有该 session 的助手会话。同一 session 同一请求已有未答问题时返回原问题。带 `--request-id` 时请求挂起到回答为止，回答后再给助手 120 秒应答，超时按方法回退。
- `vibeface triage dismiss --event-id ID`：不变。
- `vibeface triage assign --question QID --assistant CONVERSATION_ID`：主助手的判断轮认领外来会话，重复调用返回同一归属。会话不在本机时转给运行它的设备写入。
- `vibeface codex respond REQUEST_ID RESULT_JSON [--node DEVICE]`：带 `--node` 时经设备转发，应答仍只一次。本机 Node 地址参数为 `--node-url`。

用户侧 RPC：

- `questions`：全网未回答问题，含所属会话与助手名。
- `answer { question_id, text, attestation }`：写回答消息。会话问题按普通消息准入或转交调度 Node；同一问题已有不同回答时返回已有回答，`status = "already_answered"`。
- `conversation` 快照新增 `questions`（含回答）和 `sessions`（交接卡片，`conversation_id` 为认领方；主助手聊天里也列出它交出去的会话）。
- `bots` 返回 `primary`，即主助手的会话 ID，没有助手时为 `null`。
- `set_primary_assistant { conversation_id, attestation }`：本人选定主助手，签名事件体为 `{"type":"primary_assistant_set", conversation_id}`，scope 为 `assistants`。

Grok 侧：

- webhook 事件 `{"type":"question", question_id, severity, text, choices[{index,label}], about, assistant, answer:{url, device_id, question_id}}` 和 `{"type":"answered", question_id, text}`，`event_id` 分别为问题 ID 与 `answered-<问题 ID>-<回答事件哈希前 16 位>`。两端同时回答时 rank 小者胜，同步把输方本机的回答换成胜者；提问方 Node 按新的回答再发一次 `answered`，未送出的旧 `answered` 撤下。接收方以同一 `question_id` 最后收到的为准。
- `POST /api/decision { device_id, question_id, choice | text }`：返回 `{question_id, answer_id, status: "answered" | "already_answered", text}`。重试同一请求只读回。

助手一轮的输入：`trigger.event` 是回答消息，另附 `trigger.question`（问题全文、选项和 Subject），说明它回答哪个问题。提示要求助手用回答消息的 `message_id` 作为 `vibeface codex send --id`。这次发送是这一轮的工作，同一轮重试只读回；回合结束后结果作为 `work_result` 轮回到助手。
