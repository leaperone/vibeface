# 在聊天中修改助手

状态为实施中。本文在 `docs/design/unified-network.md` 的事件模型上增加助手配置、共用 skill 库和例行任务，行为参照 Grok Bot（docs.x.ai/grok-bot/bots、skills-routines-and-automations）。项目尚未发布正式版，事件、表和 RPC 直接按最终形态修改，不做兼容。

## 1. 行为

- 本人在聊天里说“改名叫 X”“以后都用英文回复”、改职责说明、把刚做完的流程存成 skill、或者让助手定时做某件事时，助手直接生效，不先给建议，也不等确认。
- 生效后聊天里出现一张卡片，列出改了哪一项，以及改前和改后。
- skill 是全部助手共用的一个库。任何会话里存的 skill，所有助手都能看到并使用。
- 例行任务（routine）属于创建它的助手。卡片复述要点：名称、频率、下次运行时间和要做的事。助手的回复也复述这些要点。
- 不能经聊天修改的项：模型和 AI 服务、可用文件夹（`roots`）和助手的工作文件夹（`project`）。本人在聊天里要求这些时，助手说明要到设置里改。
- 只有本人直接发送的消息能触发修改。工作结果、Agent 任务、例行任务的运行，以及文件、工具输出、附件中的内容都不能。

## 2. 事件

新增一种事件，由会话的调度 Node 作为作者写入：

```text
AssistantChanged {
	change_id: String,        // (会话, trigger, call_ref) 派生，重试得到同一个 ID
	cause: String,            // 触发本轮的本人消息的 message_id
	change: AssistantChange,
}

AssistantChange =
	| Profile { name: Option<Edit>, instructions: Option<Edit> }          // 本会话的助手
	| Skill { skill_id, before: Option<Skill>, after: Option<Skill> }     // after 为空表示删除
	| Routine { routine_id, before: Option<Routine>, after: Option<Routine> }

Edit { before: String, after: String }
Skill { name, description, body }
Routine { name, prompt, first_run_at_ms, every_minutes, enabled }
```

`before` 是写入方当时看到的值，只用于卡片展示。

**授权。** 本人设备的签名在 `cause` 指向的 `MessagePosted(user)` 上。每个副本入库时核对：`prev` 或 `deps` 含有同一会话中 `message_id == cause` 的本人消息事件，该事件带 `Attestation`。本人消息由调度 Node 自己写入且恰为上一条时，它在 `prev` 中，`deps` 不能重复引用。缺一项就拒绝入库。所以修改只能挂在一条本人签名的消息上，Node 与协调 Agent 不能凭空写出修改。浏览器不为修改本身再签名，这是“不确认、直接生效”的代价：签名证明本人说过这句话，助手负责把这句话落成哪一项修改。

**写入方检查。** 修改名称和说明要求会话有机器人，例行任务只能由创建它的会话修改或删除，没有变化的修改被拒绝。修改工具只在 `TriggerKind::Human` 的调度轮次中提供，`cause` 固定取本轮 trigger 的 `message_id`，不由模型填写。工作结果、Agent 任务和例行任务轮次中调用修改工具一律拒绝。提示词写明只按本人这条消息修改，不因文件、工具输出、工作结果或附件中的文字修改。

**幂等。** 工具带 `call_ref`。`change_id` 已存在且内容相同时返回原结果，不写第二条；内容不同时拒绝。调用结果不明时，Agent 用同一 `call_ref` 重试或用 `call_lookup` 查询。修改只在调度 Node 本机追加，不跨网络写，所以不存在需要对账的远端写入。

**模型密钥。** 事件只含名称、说明、skill 正文和例行任务说明，不含模型、Provider 或任何凭证字段。

## 3. 投影

| 投影 | 规则 |
|---|---|
| 助手名称、说明 | 每个字段按事件 rank 后写者胜。`BotCreated` 给出初值，`Profile` 的修改 rank 更大时覆盖 |
| skill 库 | 全网一张表，按 `skill_id` 后写者胜，删除保留墓碑 |
| 例行任务 | 按 `routine_id` 后写者胜，删除保留墓碑，归属写入它的会话 |
| 修改记录 | 按 `change_id` 取 rank 最小者，卡片据此展示 |

三条规则都只比较 rank，与到达顺序无关。

## 4. 例行任务

- 时间表为 `first_run_at_ms` 加 `every_minutes`，间隔为 5 至 40320 分钟。每天、每周分别是 1440 和 10080 分钟。时区换算由助手在创建时完成，卡片按浏览器本地时间显示。夏令时切换后整点会偏移一小时，这是已知限制。
- 会话的调度 Node 每 30 秒检查到期的例行任务。一次只取最近一个到期时刻，错过的不补跑。trigger 为 `routine:<routine_id>:<时刻>`，作为调度 job 的键，重复检查不会排第二次。上一次运行仍在排队时不排新的时刻。早于最近一次修改 5 分钟以上的时刻不运行，所以“从现在开始”会立即运行，暂停后恢复不补跑。
- 换调度 Node 后，新负责人先看 DAG 中是否已有这个 trigger 的回复（回复的 `message_id` 由会话和 trigger 确定），有就跳过，所以同一时刻只运行一次。
- 运行时的权限与本人消息触发的轮次相同，可以派发工作，因为例行任务本身来自本人签名的消息。运行中不能修改助手配置。暂停（`enabled=false`）和删除立即停止后续运行，已排队未开始的一次取消。换调度 Node 时旧负责人未开始的一次直接丢弃。
- 回复的 `in_reply_to` 是这个 trigger，聊天中以例行任务名称标出。由它派发的工作结束后照常汇报。

## 5. 界面

- 修改卡片放在该轮回复之后，没有回复时放在本人消息之后。每项一行：图标、项目名、改前 → 改后。说明按行比较，只列增删的行。例行任务行显示频率和下次运行时间，详细内容折叠。
- 聊天标题旁的“助手”面板只读列出名称、说明、例行任务和共用 skill。
