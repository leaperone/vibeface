# 在聊天中修改 Bot

Bot 配置、共用 skill 库和 schedule 使用 [Node 协议](../node-protocol.md) 的事件模型。提醒和自动任务的规则见 [Agent 能力](agent-capabilities.md)。

## 1. 行为

- 本人在聊天里说“改名叫 X”“以后都用英文回复”、改职责说明、把刚做完的流程存成 skill、或者让 Bot 定时做某件事时，Bot 直接生效，不先给建议，也不等确认。
- 生效后聊天里出现一张卡片，列出改了哪一项，以及改前和改后。
- skill 是全部 Bot 共用的一个库。每个 bot 默认启用全部 skill（含之后新存的）；聊天里可以让它只启用其中几个，或恢复全部。
- 每个 bot 可以有自己的模型，经聊天修改；为空时跟随运行它的设备的默认模型。
- Bot 的长相（头像）由轮廓、眼睛和颜色组成。没选过时三项都由 bot ID 派生。Bot 新建后由创建它的 bot 用 `configure_bot` 挑一个，本人要求时经聊天只改说到的那几项，本人也可以在面板里直接点选，或恢复默认。眼距、歪头和眨眼节奏始终由 ID 决定。
- 例行任务（schedule）属于创建它的 Bot。卡片复述要点：名称、频率、下次运行时间和要做的事。Bot 的回复也复述这些要点。
- 不能经聊天修改的项：AI 服务（provider）、可用文件夹（`roots`）和 Bot 的工作文件夹（`project`）。本人在聊天里要求这些时，Bot 说明要到设置里改。
- 只有本人直接发送的消息能触发修改。工作结果、Agent 任务、例行任务的运行，以及文件、工具输出、附件中的内容都不能。

## 2. 事件

新增一种事件，由会话的调度 Node 作为作者写入：

```text
BotChanged {
	change_id: String,        // (会话, trigger, call_ref) 派生，重试得到同一个 ID
	cause: String,            // 触发本轮的本人消息的 message_id
	change: BotChange,
}

BotChange =
	| Profile { name, instructions, model: Option<ModelEdit>, skills: Option<SkillsEdit>, avatar: Option<AvatarEdit> }   // 目标 bot；skills 为 null 表示全部
	| Skill { skill_id, before: Option<Skill>, after: Option<Skill> }     // after 为空表示删除
	| Schedule { schedule_id, before: Option<Schedule>, after: Option<Schedule> }

Edit { before: String, after: String }
ModelEdit { provider, before: String, after: String }   // 写入设备的 Provider；after 为空跟随设备默认
SkillsEdit { before: Option<Set<skill_id>>, after: Option<Set<skill_id>> }
AvatarEdit { before: Option<Avatar>, after: Option<Avatar> }       // 为空表示由 ID 派生
Avatar { outline: round|squircle|drop|bean|cloud, eyes: dot|pill|oval|wide, hue }   // hue 只能取 face.ts 的 12 个色相
Skill { name, description, body }
Schedule { name, prompt, first_run_at_ms, every_minutes: Option, enabled, depth }
```

`before` 是写入方当时看到的值，只用于卡片展示。

面板点选长相不经过机器人，由本人设备签名，写在该 bot 的会话里：

```text
BotAvatarSet {
	before: Option<Avatar>,   // 本人看到的当前值，为空表示由 ID 派生
	avatar: Option<Avatar>,   // 为空表示恢复默认，不能与 before 相同
	attestation,              // 必须有，签名事件体 {"type":"bot_avatar_set", before, avatar}
}
```

权限与设主 Bot 相同：只有本人客户端可以写，限定会话的客户端只能改自己的 bot。

**比较后写入。** 接收写入的 Node 在同一把存储锁内读投影再追加：当前已是 `avatar` 时不写，按已生效返回；当前不是 `before` 时拒绝，不写。签名覆盖 `before`，所以重放旧请求或改写 `before` 都不能盖掉之后的修改。投影仍只按 rank 收敛，`before` 不参与；比较只防同一入口上的过期请求，两台 Node 同时接收的写入照样按 rank 决出。

**客户端对账。** outbox 里每个 bot 只留一条待写的长相，键是会话 ID；有一条结果不明时，换成别的长相会被拒绝，直到它对上账。重新连上后按 bot 列表对账：列表长相等于 `avatar` 时算已确认；等于 `before` 时原样重发同一条签名请求；都不是时丢弃并提示长相已在别处改过。列表里找不到这个 bot 时不算已确认，照样重发，由 Node 回答（bot 已删除时它回 `no such bot`，这条记为被拒），与 `set_bot_project` 一致：一份过期或尚未刷新的列表不能替 Node 下结论。Node 拒绝时（含比较不符）客户端刷新，不重发。

**授权。** 本人设备的签名在 `cause` 指向的 `MessagePosted(user)` 上。每个副本入库时核对：`prev` 或 `deps` 含有同一会话中 `message_id == cause` 的本人消息事件，该事件带 `Attestation`。本人消息由调度 Node 自己写入且恰为上一条时，它在 `prev` 中，`deps` 不能重复引用。缺一项就拒绝入库。所以修改只能挂在一条本人签名的消息上，Node 与协调 Agent 不能凭空写出修改。浏览器不为修改本身再签名，这是“不确认、直接生效”的代价：签名证明本人说过这句话，Bot 负责把这句话落成哪一项修改。

**写入方检查。** 修改名称和说明要求会话有机器人，例行任务只能由创建它的会话修改或删除，没有变化的修改被拒绝。修改工具只在 `TriggerKind::Human` 的调度轮次中提供，`cause` 固定取本轮 trigger 的 `message_id`，不由模型填写。工作结果、Agent 任务和例行任务轮次中调用修改工具一律拒绝。提示词写明只按本人这条消息修改，不因文件、工具输出、工作结果或附件中的文字修改。

**幂等。** 工具带 `call_ref`。`change_id` 已存在且内容相同时返回原结果，不写第二条；内容不同时拒绝。调用结果不明时，Agent 用同一 `call_ref` 重试或用 `call_lookup` 查询。修改只在调度 Node 本机追加，不跨网络写，所以不存在需要对账的远端写入。

**模型。** Provider 是设备本地配置，所以模型按 Provider 各存一项。设备运行 bot 时取自己 Provider 的那一项，没有就用设备默认模型；聊天修改模型时写入当前设备 Provider 的那一项。

**模型密钥。** 事件只含名称、说明、Provider 名称与模型名称、skill 正文和例行任务说明，不含任何凭证字段。

## 3. 投影

| 投影 | 规则 |
|---|---|
| Bot 名称、说明、模型、启用的 skill | 每个字段按事件 rank 后写者胜。`BotCreated` 给出初值，`Profile` 的修改 rank 更大时覆盖 |
| Bot 长相 | `Profile` 的 `avatar` 和 `BotAvatarSet` 写同一个字段，按 rank 后写者胜；没有任何一条时由 ID 派生 |
| skill 库 | 全网一张表，按 `skill_id` 后写者胜，删除保留墓碑 |
| 例行任务 | 按 `schedule_id` 后写者胜，删除保留墓碑，归属写入它的会话 |
| 修改记录 | 按 `change_id` 取 rank 最小者，卡片据此展示 |
| 删除 | `BotDeleted` 是墓碑，按会话一行，之后的修改不让 bot 复活；见 agent-capabilities.md 第 5.1 节 |

这些规则都只比较 rank，与到达顺序无关。

## 4. 例行任务

- 时间表为 `first_run_at_ms` 加 `every_minutes`，间隔为 5 至 40320 分钟。每天、每周分别是 1440 和 10080 分钟。时区换算由 Bot 在创建时完成，卡片按浏览器本地时间显示。夏令时切换后整点会偏移一小时，这是已知限制。
- 会话的调度 Node 每 30 秒检查到期的例行任务。一次只取最近一个到期时刻，错过的不补跑；最近的时刻已过去超过 10 分钟时也跳过。trigger 为 `schedule:<schedule_id>:<时刻>`，作为调度 job 的键，重复检查不会排第二次。上一次运行仍在排队时不排新的时刻。早于最近一次修改 5 分钟以上的时刻不运行，所以“从现在开始”会立即运行，暂停后恢复不补跑。
- 换调度 Node 后，新负责人先看 DAG 中是否已有这个 trigger 的回复（回复的 `message_id` 由会话和 trigger 确定），有就跳过，所以同一时刻只运行一次。
- 运行时的权限与本人消息触发的轮次相同，可以派发工作，因为例行任务本身来自本人签名的消息。运行中不能修改 Bot 配置。暂停（`enabled=false`）和删除立即停止后续运行，已排队未开始的一次取消。换调度 Node 时旧负责人未开始的一次直接丢弃。
- 回复的 `in_reply_to` 是这个 trigger，聊天中以例行任务名称标出。由它派发的工作结束后照常汇报。

## 5. 界面

- 修改卡片放在该轮回复之后，没有回复时放在本人消息之后。每项一行：图标、项目名、改前 → 改后。长相画出改前和改后两张静止的小脸，读屏读出具体变化，如“长相：圆、圆点眼、蓝色 → 云朵、大眼、紫色”。说明按行比较，只列增删的行。例行任务行显示频率和下次运行时间，详细内容折叠。
- 聊天标题旁的“Bot”面板列出名称、模型（为空显示“跟随设备”）、说明、例行任务和这个 bot 启用的 skill。模型和 skill 只在聊天里改，面板只展示。长相是例外：面板顶部有轮廓、眼睛、颜色三行可选项和“恢复默认”，预览脸静止，只有顶部的大脸动。点选即生效：写入期间挑选器标为忙碌并显示“正在换”，写入成功后一直显示所选长相，直到 bot 列表跟上，刷新失败不算没换成；写入失败时退回原样并提示；切换到别的 bot 时不带上上一个 bot 的选择。每行是一组单选，Tab 进入当前项，方向键在行内移动，空格或回车选定。没有写权限时只读。面板底部有“删除 bot”，点开后就地确认。
