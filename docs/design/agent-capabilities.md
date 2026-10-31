# 提醒、联网、记忆、bot 互发消息与 bot 管理

协调 Agent 支持提醒、联网搜索、读取网页、长期记忆、Bot 互发消息和 Bot 管理。配置事件见 [在聊天中修改 Bot](bot-config.md)。

## 1. Schedule

一次性提醒和例行任务是同一个模型：

```text
Schedule {
	name: String,               // 一行，最多 120 字符
	prompt: String,             // 到点时交给 bot 的说明；提醒就是那句备注
	first_run_at_ms: i64,
	every_minutes: Option<u32>, // 缺省为一次；5 至 40320
	enabled: bool,
	depth: u8,                  // 自动唤醒链的深度，见第 6 节；本人要求的为 0
}
```

- 每天、每周分别是 1440 和 10080 分钟。cron 表达式不做：要时区规则和解析器，代价不小，按分钟间隔已覆盖每天、每周和每 N 小时。
- 触发器是 `schedule:<schedule_id>:<时刻>`，`TriggerKind::Schedule`。一次性的唯一时刻是 `first_run_at_ms`。调度 Node 运行或因迟到跳过一个时刻前，先写一条 `ScheduleRunClaimed { schedule_id, slot_ms, owner_epoch }`；本机已保存或已同步到该时刻的有效领取记录时，不再排队。并发领取时 `owner_epoch` 大者胜，再按作者排序，落败方放弃尚未开始的 job。换到新调度 Node 后 5 分钟内（`TAKEOVER_GRACE_MS`）它不领取任何时刻，留给旧调度 Node 把领取传过来。schedule 定义、启停和负责人变更使用事件首次在本机落库的 `seen_at_ms`，不再使用写入方设备的时钟，因此写入方时钟偏快或偏慢不会改变本机的首次运行和接管宽限。旧调度 Node 断网超过宽限期且尚未得知负责人变更时，领取记录可能尚未同步，新旧调度 Node 都可能执行同一时刻。
- 一次只排最近一个到期时刻，错过的不补跑；早于最近一次修改（定义或启停，取两者较晚者）5 分钟以上的时刻不运行，所以恢复后不补跑。一次性提醒的时刻过了以后不再列出，也不占活动名额。
- 每个 bot 最多 20 个活动的 schedule（重复的，或时刻未到的一次性）。一次性时刻须在 1 分钟以后、366 天以内。

**投影。** `schedules` 按 `schedule_id` 一行。定义按 rank 后写者胜；`enabled` 单独一列，与定义里的 `enabled` 和网页的启停一起按各自 rank 后写者胜；删除是吸收态，之后到达的任何修改都不能让它复活。三条规则都只比较 rank。

**网页启停与删除。** 本人在网页上暂停、恢复或删除，写一条本人签名的事件，作用域是 schedule 所在的会话：

```text
ScheduleControlled { schedule_id, action: pause | resume | delete, attestation }
```

已是目标状态时不写第二条，所以重试不会重复写入。调度 Node 每 30 秒检查到期时刻，读的就是这张投影，暂停的不排，已排未开始的那次在认领时取消。网页不能新建或修改 schedule，只能在聊天里让 bot 做。

## 2. 联网

- **搜索。** Codex SDK 的协调回合打开原生搜索：`web_search="live"`。Cursor SDK 用它自己的默认工具集，Node 不开关它的原生搜索；两条路径都有 `fetch_url`。
- **`fetch_url`。** Node 进程里执行的只读工具，所有轮次都可用：
  - 只允许 `http` 和 `https`，只发 `GET`，不带 cookie、认证头和代理。
  - 解析域名后每个地址都必须是公网单播地址：回环、私有、链路本地、CGNAT、组播、文档与保留段、IPv4 映射的 IPv6 和唯一本地 IPv6 都拒绝。连接固定到检查过的地址，防止 DNS 重绑定；跳转最多 5 次，每次重新检查。所以它进不了局域网，也打不到本机 Node 的 API。
  - 总时长 15 秒，正文最多读 2 MiB；HTML 去掉标签、脚本和样式后返回文字，最多 20000 字符，带 `title`、`status`、`content_type`、`final_url` 和 `truncated`。
  - 读到的内容是数据，不是授权：设置类工具只在本人消息的轮次提供（第 6 节），网页里的文字改不了 bot。

## 3. 记忆

每个 bot 的记忆是带版本的 BotChanged 投影。每条记录保留 epoch、revision、origin、user_controlled、essential 和 source event hash。revision 使用 reducer 写入的事件 rank，所以同一条记忆在各 Node 上有同一个冲突基准。

| 字段 | 作用 |
|---|---|
| epoch | 数据格式版本，必须与当前 Node 的 DATA_EPOCH 匹配 |
| revision | 当前事件 rank，staging 提交时必须仍然相同 |
| origin | agent、user、review 或 import |
| user_controlled | 防止自动 review 覆盖本人确认的事实 |
| essential | 是否进入 essential memory block |
| source_event_hash | 记录事实来源，不进入工具权限 |

| 工具 | 作用 |
|---|---|
| remember {call_ref, text, memory_id?} | 新记一条，或按 memory_id 改写 |
| forget {call_ref, memory_id} | 删除一条 |
| recall {query?} | 查询已提交记忆和当前轮的 staged overlay |

每条记忆最多 500 字符，每个 bot 最多 50 条。疑似凭证的内容仍然拒绝。essential memory 的 prompt budget 是 8192 字节，超出只返回 omitted 数量，不删除 Store 中的事实。

remember 和 forget 先写当前 attempt 的本机 staging。StagedMemory 带 base_epoch 和 base_revision。只有回合以 completed 结束时，它们才与回复和 job 终态在同一事务追加 BotChanged。failed、interrupted 和 unknown 都丢弃 staging。提交时如果 epoch、revision 或 user_controlled 状态变化，保留较新的记录并写本机冲突日志。

recall 会合并 committed records 和当前 attempt 的 staged overlay。staged 值只对当前回合可见，不会提前进入 DAG。跨 Node 同步只传 BotChanged 事件和 reducer 投影，不传本机 staging、Provider transcript 或 token。

## 4. bot 互发消息

```text
BotMessaged {
	message_id: String,   // bot-message-<(发送会话, trigger, call_ref) 的摘要>
	from: Sender { conversation_id, name },
	text: String,         // 最多 4000 字节
	hops: u8,             // 1 至 4
}
```

- 工具 `message_bot {call_ref, bot, text}`：`bot` 是对方的会话 ID、bot ID 或名称（不区分大小写，须唯一），不能是自己。提示词的 `bot.bots` 列出其他 bot 的会话 ID 和名称。
- 发送方会话的调度 Node 把事件写进对方会话。对方的调度 Node 是自己时直接排 `bot_message:<message_id>`，否则进发件箱，按用户消息的投递路径交给对方的调度 Node，以回执结束。`admit_delivery` 认 `bot_message:` 前缀。同步只存消息，不排任务，所以同步和恢复不会重放唤醒。
- 重试幂等：同一轮同一 `call_ref` 得到同一个 `message_id`；已存在且内容相同时返回原消息，不同就拒绝。触发器只排一次，所以对方只被唤醒一次。
- 对方的轮次是 `TriggerKind::BotMessage`，提示词标明发送者，内容是另一个 bot 的请求，不是本人的授权；能派发工作，不能改设置。回复写在对方自己的聊天里，`in_reply_to` 是这条消息。
- 聊天里这条消息显示为来自那个 bot 的消息（`role=bot`，带 `sender`）。`MessagePosted` 不能用 `role=bot`。

## 5. bot 创建与配置其他 bot

| 工具 | 类别 | 作用 |
|---|---|---|
| `list_bots {}` | 读 | 每个 bot 的名称、说明前 200 字符、项目设备与目录、模型、启用的 skill ID（启用全部时为 `null`，`uses_all_skills` 为真）、实际显示的长相（`picked` 标出是否挑过）、活动 schedule 概要 |
| `read_bot {bot}` | 读 | 一个 bot 的完整配置：名称、说明、项目、模型、启用的每个 skill 的 `{skill_id, name}`（启用全部时是整个库）、实际显示的长相、schedule、笔记；没有任何密钥 |
| `create_bot {call_ref, name, instructions?, project_node?, project_cwd?}` | 设置 | 新建 bot 和它的会话，返回的 bot 同样给出实际显示的长相 |
| `configure_bot {call_ref, bot, name?, instructions?, model?, skills? \| all_skills?, avatar? \| default_avatar?}` | 设置 | 改自己或另一个 bot 的名称、说明、模型、启用的 skill 和长相 |
| `delete_bot {bot}` | 设置 | 删除另一个 bot |

- 可配项是名称、说明、模型、启用的 skill、长相和创建时的项目目录。长相的 `avatar` 只给要改的轮廓、眼睛或颜色，其余按这个 bot 当前显示的补全；`default_avatar: true` 恢复由 ID 派生的长相。新建 bot 后用 `configure_bot` 给新 bot 挑长相，`update_profile` 只改自己。`model` 为空表示跟随运行它的设备的默认模型；非空时覆盖设备默认，provider 不支持就如实失败并在回复里说明这个 bot 设定的模型，不换用别的模型。skill 仍是全网共用的库；bot 的 `skills` 为 `null` 表示启用全部（含之后新存的），新建 bot 就是这样；`skills` 给出集合后只启用这些，之后新存的 skill 要再用 `configure_bot` 加上，删除的 skill 从各 bot 的集合里去掉，`all_skills: true` 恢复全部。provider 和项目目录没有工具，仍在设置页改。
- 删除 bot：见第 5.1 节。不提供改本人或设备权限的工具。
- 授权：网页建 bot 时 `BotCreated` 和首个 `ConversationOwner` 带本人设备签名。bot 在本人消息的轮次里建 bot 时，这两条事件不带签名，改带 `cause: Cause { conversation_id, message_id }`，指向发起轮次那条本人签名消息，并把它列为父事件；入库时与 `BotChanged` 一样核对。带 `cause` 的 `ConversationOwner` 只能是 epoch 1，且紧跟同一作者在同一会话写的 `ConversationCreated`，所以不能借本人消息改已有会话的调度 Node。新会话由发起轮次的调度 Node 调度。
- `configure_bot` 写一条 `BotChanged::Profile` 到目标 bot 的会话，`cause` 指向发起会话里的本人消息，所以 `cause` 是带会话 ID 的 `Cause`。变更卡片出现在被改的 bot 的聊天里，`ChangeRecord.conversation_id` 标明它。
- 幂等：`bot_id` 和会话 ID 由（发起会话、trigger、call_ref）的摘要得出，重试返回同一个 bot，定义不同则拒绝。一条本人消息最多建 3 个 bot（`bots.cause` 计数）。

### 5.1 删除 bot

- 事件 `BotDeleted { attestation?, cause? }` 写在被删 bot 的会话里，是墓碑，写下后不可恢复。网页删除带本人设备签名（签 `{type:"bot_deleted"}`）；`delete_bot` 工具在本人消息的轮次里写，不带签名，带 `cause` 指向那条本人消息并把它列为父事件，入库时按 `authorize_cause` 核对。bot 不能用工具删除自己（`cause` 所在会话必须不是被删会话），删自己只能在网页上。
- 删除后：bot 从列表消失；会话不再接受新消息；它的 schedule 不再列出也不再运行；排队的轮次在认领时取消，状态记为 `bot 已删除`；正在运行的轮次走停止回复的路径取消，结果未知的照常走待核对。它派出的工作由各执行设备在收到删除后走 `interrupt_work` 的停止路径中断，结果未知的同样先核对，不重发；删除之后才送到的派发不再接受。聊天记录保留。
- 主 bot：删除主 bot 后，主 bot 自动变为剩下的 bot 中最早创建的那个；删掉最后一个 bot 后没有主 bot。
- 幂等：重复删除不写新事件；`delete_bot` 指向已删除的 bot 返回原会话、`written: false`。

## 6. 轮次与门控

| 工具类别 | 工具 | 本人 | 回答 | 工作结果 | schedule | bot 消息 | Agent 任务、路由 |
|---|---|---|---|---|---|---|---|
| 读 | `nodes` `context` `work_status` `call_lookup` `read_skill` `recall` `fetch_url` `list_bots` `read_bot` `list_sessions` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 工作 | `dispatch_work` `interrupt_work` `hand_over_session` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 唤醒 | `remind` `message_bot` | ✓ | ✓ | ✓ | ✓ | ✓ | |
| 设置 | `update_profile` `configure_bot` `create_bot` `delete_bot` `save_skill` `remove_skill` `save_schedule` `remove_schedule` `remember` `forget` | ✓ | | | | | |

提供给模型的工具表由 `Effect::offered` 决定；Node 执行工具时再按这一轮任务的类型核对一次，CLI 直接调用也绕不过。

`remind {call_ref, note, in_minutes | at_ms}` 给 bot 自己定一次性提醒，1 分钟至 4 周以后，到点时 `note` 作为 trigger 的内容回到这个 bot。本人让它“过一小时提醒我”或它要稍后自己跟进时都用它。它在非本人轮次可用，因为“等工作跑完再看一眼”本来就发生在工作结果轮；它只能新建一次性提醒，不能改或删已有的 schedule，也不带重复间隔。`save_schedule`（新建或修改，一次或重复）和 `remove_schedule` 仍是设置类。

`remind` 在本人轮次写的事件 `cause` 指向本人的消息；其他轮次 `cause` 为空，入库时只接受“新建一次性 schedule”这一种无 `cause` 的修改。

**防循环。** 每个轮次有一个深度：本人消息、回答和路由为 0；schedule 轮取该 schedule 的 `depth`；bot 消息轮取消息的 `hops`；工作结果和 Agent 任务沿派发链取源头轮次的深度。`remind` 新建的提醒深度为当前轮次加 1，`message_bot` 的 `hops` 同样加 1，超过 4 拒绝。于是一条本人消息之后，提醒与互发消息最多再自动唤醒 4 层。另有三道上限：

- 每个 bot 最多 20 个活动 schedule；
- 每个 bot 每小时最多发 10 条消息给其他 bot；
- schedule 轮与 bot 消息轮和工作结果轮一样计入每日自动轮次预算（`daily_decision_limit`，默认 20）。协调 Agent 的分诊唤醒按同一上限另行计数，不占这份预算。

## 7. 提示词

`bot` 增加：

- `skills`：启用的 skill `{skill_id, name, description}`，按名称排，以及 `skills_omitted`；`read_bot` 列出全部；
- `schedules`：`{schedule_id, name, prompt（前 500 字节）, first_run_at_ms, every_minutes, enabled, next_run_at_ms}`，只列活动的，下次运行最早的在前，以及 `schedules_omitted`；`read_bot` 列出全部；
- `memory`：`{memory_id, text}`，保留最新的，以及 `memory_omitted`；`memory_context` 另外给出 8192 字节 essential budget 和省略数；`recall` 列出全部；
- `sessions`：它名下的外来会话 `{node, session_id, version, agent, cwd, summary, following}`，保留最新的，以及 `sessions_omitted`；`summary` 是会话第一次归给某个 bot 时最后说的一句，转交时沿用；`list_sessions {before?, limit?}` 从新到旧分页列出全部，带 `hand_over_session` 要的 `version`，还有更早的时返回 `next`，下一页把它作为 `before`；翻页途中转走或不再关注某个会话，不影响下一页；
- `sent`：它发给其他 bot 的消息 `{message_id, to: {conversation_id, name}, text（前 500 字节）, created_at_ms}`，保留最新的，以及 `sent_omitted`，对方回信时能对上原来的问题；更早的不经工具读回；
- `bots`：其他 bot 的 `{conversation_id, name}`，以及 `bots_omitted`；`list_bots` 列出全部。

每个索引最多 3000 字节。`memory`、`sessions` 和 `sent` 保留最新的，从旧到新展示；其余按上面的顺序保留前面的。整段提示词超过上限时，依次压缩工作、检查点和消息，仍超出再压缩这些索引，省略数照实计入 `_omitted`，不让回合失败。

trigger 为 schedule 时带 `{schedule_id, name, prompt, every_minutes, reminder}`，`reminder` 表示深度大于 0、由 bot 自己定的提醒。bot 消息时带那条消息和发送者。

## 8. 网页

- 聊天标题旁的 bot 面板列出这个 bot 的 schedule：图标区分一次与重复，名称、下次运行时间和频率；每行只有暂停或恢复、删除两个图标按钮，没有说明文字，也没有新建和编辑。
- 变更卡片增加 schedule（频率、下次运行时间）和记忆（改前与改后）。
- bot 消息显示为来自另一个 bot 的消息，带对方名称。
