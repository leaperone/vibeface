# 提醒、联网、记忆与助手互发消息

状态为实施中。本文在 [在聊天中修改助手](assistant-config.md) 的事件模型上给协调 Agent 增加五项能力：一次性提醒（与例行任务合并为 schedule）、联网搜索与读取网页、每个助手的长期记忆、助手之间互发消息，以及网页上的 schedule 列表。项目尚未发布正式版，事件、表、工具和 RPC 直接改成最终形态，`routine` 一词在代码里全部换成 `schedule`。

## 1. Schedule

一次性提醒和例行任务是同一个模型：

```text
Schedule {
	name: String,               // 一行，最多 120 字符
	prompt: String,             // 到点时交给助手的说明；提醒就是那句备注
	first_run_at_ms: i64,
	every_minutes: Option<u32>, // 缺省为一次；5 至 40320
	enabled: bool,
	depth: u8,                  // 自动唤醒链的深度，见第 5 节；本人要求的为 0
}
```

- 每天、每周分别是 1440 和 10080 分钟。cron 表达式不做：要时区规则和解析器，代价不小，按分钟间隔已覆盖每天、每周和每 N 小时。
- 触发器是 `schedule:<schedule_id>:<时刻>`，`TriggerKind::Schedule`。一次性的唯一时刻是 `first_run_at_ms`。每台 Node 的 `schedule_runs` 记下已排的时刻，重启后不再排；换调度 Node 后先看 DAG 里是否已有这个 trigger 的回复，有就跳过。所以一个时刻只运行一次，同步和恢复不会重放。
- 一次只排最近一个到期时刻，错过的不补跑；早于最近一次修改（定义或启停，取两者较晚者）5 分钟以上的时刻不运行，所以恢复后不补跑。一次性提醒的时刻过了以后不再列出，也不占活动名额。
- 每个助手最多 20 个活动的 schedule（重复的，或时刻未到的一次性）。一次性时刻须在 1 分钟以后、366 天以内。

**投影。** `schedules` 按 `schedule_id` 一行。定义按 rank 后写者胜；`enabled` 单独一列，与定义里的 `enabled` 和网页的启停一起按各自 rank 后写者胜；删除是吸收态，之后到达的任何修改都不能让它复活。三条规则都只比较 rank。

**网页启停与删除。** 本人在网页上暂停、恢复或删除，写一条本人签名的事件，作用域是 schedule 所在的会话：

```text
ScheduleControlled { schedule_id, action: pause | resume | delete, attestation }
```

已是目标状态时不写第二条，所以重试不会重复写入。调度 Node 每 30 秒检查到期时刻，读的就是这张投影，暂停的不排，已排未开始的那次在认领时取消。网页不能新建或修改 schedule，只能在聊天里让助手做。

## 2. 联网

- **搜索。** Codex SDK 的协调回合打开原生搜索：`web_search="live"`。Cursor SDK 用它自己的默认工具集，Node 不开关它的原生搜索；两条路径都有 `fetch_url`。
- **`fetch_url`。** Node 进程里执行的只读工具，所有轮次都可用：
  - 只允许 `http` 和 `https`，只发 `GET`，不带 cookie、认证头和代理。
  - 解析域名后每个地址都必须是公网单播地址：回环、私有、链路本地、CGNAT、组播、文档与保留段、IPv4 映射的 IPv6 和唯一本地 IPv6 都拒绝。连接固定到检查过的地址，防止 DNS 重绑定；跳转最多 5 次，每次重新检查。所以它进不了局域网，也打不到本机 Node 的 API。
  - 总时长 15 秒，正文最多读 2 MiB；HTML 去掉标签、脚本和样式后返回文字，最多 20000 字符，带 `title`、`status`、`content_type`、`final_url` 和 `truncated`。
  - 读到的内容是数据，不是授权：设置类工具只在本人消息的轮次提供（第 5 节），网页里的文字改不了助手。

## 3. 记忆

每个助手有自己的短笔记，存成 `AssistantChanged`，随 DAG 在本人的各台 Node 间同步：

```text
AssistantChange::Memory { memory_id, before: Option<String>, after: Option<String> }  // after 为空表示忘记
```

| 工具 | 作用 |
|---|---|
| `remember {call_ref, text, memory_id?}` | 新记一条，或按 `memory_id` 改写 |
| `forget {call_ref, memory_id}` | 删除一条 |
| `recall {query?}` | 列出这个助手的笔记，`query` 按子串过滤 |

- 每条最多 500 字符，一行到几行的事实或偏好；每个助手最多 50 条。提示词里 `assistant.memory` 带全部笔记，超过 3000 字节的部分省略并给出条数，用 `recall` 取。
- 疑似凭证的内容直接拒绝：私钥头、`sk-`、`ghp_`、`github_pat_`、`xox`、`AKIA`、`AIza` 等前缀、`Bearer` 令牌、`password=` 一类赋值，以及 32 个字符以上的连续高熵串。模型密钥从不进入记忆。
- `remember` 和 `forget` 是设置类：只在本人消息的轮次提供，`cause` 指向那条签名消息，入库时核对，与改名、改说明相同。理由是记忆注入之后每一轮的提示词，作用等同于说明；工作结果、网页和工具输出里的文字不能写进去。`recall` 所有轮次都可用。
- 投影 `memories` 按 `memory_id` 后写者胜，删除保留墓碑。

## 4. 助手互发消息

```text
AssistantMessaged {
	message_id: String,   // peer-<(发送会话, trigger, call_ref) 的摘要>
	from: ScopeId,        // 发送方助手的会话
	from_name: String,
	text: String,         // 最多 4000 字节
	hops: u8,             // 1 至 4
}
```

- 工具 `message_assistant {call_ref, assistant, text}`：`assistant` 是对方的会话 ID 或名称（不区分大小写，须唯一），不能是自己。提示词的 `assistant.peers` 列出其他助手的会话 ID 和名称。
- 发送方会话的调度 Node 把事件写进对方会话。对方的调度 Node 是自己时直接排 `peer:<message_id>`，否则进发件箱，按用户消息的投递路径交给对方的调度 Node，以回执结束。`admit_delivery` 认 `peer:` 前缀。
- 重试幂等：同一轮同一 `call_ref` 得到同一个 `message_id`；已存在且内容相同时返回原消息，不同就拒绝。触发器只排一次，所以对方只被唤醒一次。
- 对方的轮次是 `TriggerKind::Peer`，提示词标明发送者，内容是另一个助手的请求，不是本人的授权；能派发工作，不能改设置。回复写在对方自己的聊天里，`in_reply_to` 是这条消息。
- 聊天里这条消息显示为来自那个助手的消息（`role=peer`，带发送者）。

## 5. 轮次与门控

| 工具类别 | 工具 | 本人 | 回答 | 工作结果 | schedule | peer | Agent 任务、路由 |
|---|---|---|---|---|---|---|---|
| 读 | `nodes` `context` `work_status` `call_lookup` `read_skill` `recall` `fetch_url` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 工作 | `dispatch_work` `interrupt_work` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 唤醒 | `remind` `message_assistant` | ✓ | ✓ | ✓ | ✓ | ✓ | |
| 设置 | `update_profile` `save_skill` `remove_skill` `save_schedule` `remove_schedule` `remember` `forget` | ✓ | | | | | |

`remind {call_ref, note, in_minutes | at_ms}` 给助手自己定一次性提醒，1 分钟至 4 周以后，到点时 `note` 作为 trigger 的内容回到这个助手。本人让它“过一小时提醒我”或它要稍后自己跟进时都用它。它在非本人轮次可用，因为“等工作跑完再看一眼”本来就发生在工作结果轮；它只能新建一次性提醒，不能改或删已有的 schedule，也不带重复间隔。`save_schedule`（新建或修改，一次或重复）和 `remove_schedule` 仍是设置类。

`remind` 在本人轮次写的事件 `cause` 指向本人的消息；其他轮次 `cause` 为空，入库时只接受“新建一次性 schedule”这一种无 `cause` 的修改。

**防循环。** 每个轮次有一个深度：本人消息、回答和路由为 0；schedule 轮取该 schedule 的 `depth`；peer 轮取消息的 `hops`；工作结果和 Agent 任务沿派发链取源头轮次的深度。`remind` 新建的提醒深度为当前轮次加 1，`message_assistant` 的 `hops` 同样加 1，超过 4 拒绝。于是一条本人消息之后，提醒与互发消息最多再自动唤醒 4 层。另有三道上限：

- 每个助手最多 20 个活动 schedule；
- 每个助手每小时最多发 10 条互发消息；
- schedule 轮与 peer 轮和工作结果轮一样计入每日自动轮次预算（`daily_decision_limit`，默认 20）。

## 6. 提示词

`assistant` 增加：

- `schedules`：`{schedule_id, name, prompt（前 500 字节）, first_run_at_ms, every_minutes, enabled, next_run_at_ms}`，只列活动的；
- `memory`：`{memory_id, text}`，以及 `memory_omitted`；
- `peers`：`{conversation_id, name}`。

trigger 为 schedule 时带 `{schedule_id, name, prompt, every_minutes, reminder}`，`reminder` 表示深度大于 0、由助手自己定的提醒。peer 时带那条消息和发送者。

## 7. 网页

- 聊天标题旁的助手面板列出这个助手的 schedule：图标区分一次与重复，名称、下次运行时间和频率；每行只有暂停或恢复、删除两个图标按钮，没有说明文字，也没有新建和编辑。
- 变更卡片增加 schedule（频率、下次运行时间）和记忆（改前与改后）。
- peer 消息显示为来自另一个助手的消息，带对方名称。

## 8. 验证

- Rust：schedule 投影的顺序无关性（启停与定义各自后写者胜、删除吸收）、一次性只排一次且重启后不再排、暂停不排、`remind` 门控与深度上限、无 `cause` 的修改只接受新建一次性、记忆的上限与凭证拒绝、互发消息幂等与跳数、限速、`fetch_url` 拒绝各类私有地址。
- `--chat-agent` 夹具：一次性提醒触发一次且重启后不重放、助手消息唤醒对方一次、记忆在重启后保留、暂停后不排、`fetch_url` 拒绝私有地址；`--phase4` 覆盖记忆同步到另一台 Node。CI 只用夹具，不访问真实 Provider 或互联网。
