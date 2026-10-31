# OpenBot 启发的 Agent 工作区设计基线

> 状态：设计基线。本文描述目标信息架构和当前数据边界，不表示网页、协议或数据库已经实现。

本文面向一个人管理自己网络中的多个 Bot、任务和设备。工作区聚合已有投影，方便查看跨 Bot 任务、步骤、审批、进度、上下文、会话、队列和协作。它不替换事件 DAG、conversation、scheduler 或 work 的事实来源。

## 范围和排除项

工作区包括：

- **跨 Bot 任务总览。** 按 Bot conversation、任务标题、目标 Node、状态和摘要列出未结工作，并可回到原 conversation。
- **步骤、审批和进度。** 读取 scheduler job、`WorkProgress`、`Question` 和 `PendingCheck`，显示阶段摘要、需要本人处理的卡片和确定结果。
- **上下文和会话。** 展示聊天、决定、checkpoint、会话归属和可续接线索，区分共享事实与本机 Provider session。
- **队列和协作。** 展示一个 conversation 的排队顺序、Bot 消息和跨 Node work，不虚构一个全 Node 全局队列。

本设计明确排除浏览器登录和操作、远程浏览器或桌面画面、手机投屏、组织共享设备池、团队设备管理员、跨个人网络的任务负责人和全局调度员。每个人管理本人设备，公网 Server 仍是 Node 的一种角色。

OpenBot 只提供可借鉴的行为，例如任务分栏、步骤摘要、审批卡、队列状态和会话交接。实现使用 Vibeface 的签名事件、Node 授权和现有投影，不复制 OpenBot 的 PolyForm 许可证源码、资源或实现细节。

## 当前数据形状

以下字段来自当前源码，不能在 UI 层改成掩盖语义的状态名。

| 对象 | 当前事实和工作区用途 |
| --- | --- |
| `open_works` | `Store::open_works(network, scope)` 按 SQL 找出存在 dispatch 且没有结束记录、结果为 `unverified`，或仍待本人看的失败工作；后者是会话里最近结束的失败或被拒工作，之后没有更晚结束的工作、Bot 没有对它做出后续回复（它的结果报告轮的回复，或对失败之后本人消息的回复；派发它的那一轮的回复不算），也没有本人在待处理关掉它（`NotificationAcked`，`client_id = "inbox"`）。本机被拒的派发（未接收）同样列出，直到本人关掉、Bot 对它的报告轮写出已完成的回复，或在没有报告轮时（派发轮内直接得知被拒，或关掉了结果汇报），派发它的那一轮之后本人发的消息有了已完成的回复。已完成的工作不在此列表。 |
| `ConversationPayload` | 客户端快照包含 `conversation`、`messages`、`works`、`scheduler`、`checks`、`local_node`、`changes`、`overwritten`、`bot_settings`、`questions`、`sessions`、`sent`。工作区从这些字段建立链接，不另建任务事实。 |
| `scheduler_jobs` | job 主键语义为 `(conversation_id, trigger)`。`trigger` 可为 `message:<id>`、`work:<id>`、`task:<id>`、`bot_message:<id>` 或 `schedule:<id>:<slot>`。job 还保存 `kind`、顺序、`basis` 版本向量、`reply_to`、固定 `message_id` 和状态。 |
| `scheduler_sessions` | 本机 Provider resume 指针，含 `native_id`、`native_token`、`covers`、`owner_epoch`、`sdk_config`、`permission_version`、`active` 和下一序号。它不是完整 transcript，也不是同步到 DAG 的上下文。 |
| `ContextCheckpoint` | DAG 事件，含 `covers`、`summary`、`recent_user_requests`、`open_work`、`decisions`。它压缩提示预算，不删除旧事件，不等于 Provider 的 KV memory。 |
| `message_bot` | 写入接收 Bot conversation 的 `BotMessaged { message_id, from, text, hops }`，触发 `bot_message:<message_id>` job。远端目标先进入 outbox。相同轮次和 `call_ref` 重试只回读同一消息。 |
| `dispatch_work` | 用 `WorkSpec { work_id, conversation, trigger, target, dispatcher, call_ref, kind, cwd, title, prompt, continues, parent, thread }` 描述工作。`work_id` 由 `(conversation, trigger, call_ref)` 派生；跨 Node 时 `target` 指明执行设备。 |

`WorkProjection` 的共享状态是 `accepted`、`running`、`interrupting`、`finished`、`unverified`，结果可以是 `completed`、`failed`、`interrupted`、`rejected`、`unverified`。本机运行记录还会暂时使用 `starting`，不应扩展为共享状态。

`scheduler_jobs` 当前状态是 `queued`、`claimed`、`running`、`cancelling`、`completed`、`failed`、`cancelled`、`interrupted`、`steering`、`steered`。`claim_next` 对一个 conversation 一次只领取一个 active job。不同 conversation 可以并行，源码没有 OpenBot 式 Node-wide turn slot。

## 目标信息架构

### 总览和任务详情

总览优先读取 `workspace_overview`，没有总览快照时从 `open_works` 推导。页面按需要留意、进行中、等待中分组，Web 另提供 Bot 筛选。卡片显示 Bot、任务标题、目标 Node、`kind`、状态、最近摘要和是否需要核对。`dispatch_waiting` 不能显示为已接收，`unverified` 和离线待核对工作优先放到需要留意。

总览来自当前 Node 已知的记录。远端 scheduler 未查询、尚未同步和达到返回上限都需要明确展示，不能把局部快照标成全网络实时任务。已有 `open_works` 不应因为总览截断而从页面消失。首次读取前或冷启动离线时，任务集合未知，不能显示确定的空任务结论。

详情按 `trigger` 和 `reply_to` 说明来源，按 scheduler job 展示排队、领取、执行、取消和回复状态，按 `summary` 与 `WorkProgress` 展示阶段摘要，再展示 `Question`、`PendingCheck`、结果和失败原因。审批卡只代表已有问题或核对记录，不新增绕过设备签名、Bot 轮次或 Node 授权的批准接口。工作区新增的步骤和状态卡只使用经过清理的摘要；Provider token、隐藏命令输出和原始 diff 不应由新增投影引入。当前已有任务详情仍会展示用户交代的 prompt、结果文本、失败原因和项目路径，设计不能把这些现有字段描述成完全不存在。

审批只关联尚未结束的工作，避免复用同一 Provider session 的新审批出现在旧任务里。Web 和手机都可展开真实的步骤摘要；普通摘要保留原文，不生成额外步骤、百分比或完整执行历史。手机详情也需直接显示所属 Bot。

### 上下文和会话

上下文页整理已返回的消息、工作记录（包含终态）、Bot 设置、持久记忆和定时任务。当前 `ConversationPayload` 未提供 checkpoint 或完整 decisions，页面不能把未提供的数据显示为零或已清空。

`sessions` 来自外部会话的 `SessionAttributed` 归属记录。页面可列出设备、Provider、session ID、目录和静音状态，但它不证明 Bot 自身的原生运行会话存在或不存在，也不证明该会话仍可续接。没有归属记录时，只能说明没有已接入的外部会话。原生 resume token 不进入工作区。

页面必须区分共享聊天事实、Node 本地的 `scheduler_sessions` 指针和 Provider 自己的历史。原生 session 丢失时显示“从共享记录重建”或“原生会话不可用”，不能声称 KV context 已迁移。

### 队列和协作

队列按 job `sequence` 排序，并保留人类消息优先规则。跨 conversation 的并行状态按各自 Node 和 job 展示，不合并成未定义的全局并发限制。

快照未提供执行顺序字段时，UI 只能显示观察到的调用状态，不能称为实际排队顺序。`queued`、`cancelling` 和已写回复但尚待同步的 `publishing` 使用各自的状态文案。历史调用与等待执行的调用分开，不能统一标为正在运行。

`message_bot` 是 Bot 间消息，显示发送方、接收方、正文、hops 和投递状态。`dispatch_work` 是在某个 Node 上执行的工作，显示 dispatcher、target、work ID、结果和回执。两者可以关联，但不能互换。Bot 消息没有工作结果，工作派发也不等于聊天消息。

当前消息快照未返回 hops 和投递回执，已有消息记录不能被标成目标已接收或已回复。手机上下文和协作视图复用同一份已认证快照，保持这些边界，不另建频道事实或执行权限。

## 未知结果和用户动作

| 来源 | 需要保留的区别 |
| --- | --- |
| scheduler job | `queued` 是等待领取；`claimed`/`running` 是已调度；`cancelling` 是停止未确认；终态由 `completed`、`failed`、`cancelled`、`interrupted` 表示；`steering`/`steered` 表示消息加入已有回合。 |
| WorkProjection | `accepted` 表示已登记但未开始；`running` 表示目标报告进行中；`interrupting` 表示停止未确认；`finished` 有确定结果；`unverified` 无法证明结果。 |
| dispatch delivery | `dispatch_waiting` 等待目标接受；`dispatch_rejected` 是明确拒绝；`dispatch_unverified` 无法确认目标是否接收。 |

`unverified`、`dispatch_unverified`、不可达的 `remote` 和 `pending_check` 都是结果未知或仍需核对。工作区必须：

- 不显示“已完成”，不自动改派，不用新的 `call_ref` 重放副作用。
- 提供 `call_lookup`、同步、受控 probe 或用户结算入口。
- 分开显示设备离线、App Server 断开、进程未确认和执行失败。
- 用户选择完成或中断时走既有结算路径，保留未知证据。

连接恢复、事件同步或刷新页面不能单独证明副作用已经发生。跨 Node 的 work 去重是协议设计，不等于已经运行证明的 exactly-once 或高可用执行集群。

## 验证边界

当前形状由这些源码提供：`crates/node/src/store.rs`、`crates/node/src/model.rs`、`crates/node/src/work.rs`、`crates/node/src/scheduling.rs`、`crates/node/src/runtime/work.rs`、`crates/node/src/runtime/chat_controls.rs`、`crates/node/src/runtime/schedule_delivery.rs`、`crates/node/src/store/questions.rs`、`crates/node/src/store/bot_settings.rs` 和 `packages/client/src/workspace/types.ts`。

本轮只做静态阅读，没有安装依赖、编译、测试、启动应用、连接 Provider、操作浏览器或手机，也没有验证多 Node、弱网、NAT、relay、App Server resume、真实 UI 或无障碍表现。后续实现至少要用受控夹具验证：

1. 多 Bot 总览能合并 `open_works`，并保持 settled、`unverified` 和 delivery 的边界。
2. 同一 `call_ref` 的 `message_bot` 和 `dispatch_work` 重试只回读，不产生第二条消息或第二个副作用回合。
3. conversation 单 active job、跨 conversation 并行、steering 和取消与 scheduler projection 一致。
4. Node 重启或 native session 丢失后，checkpoint 和事件能重建可核对输入，并保留未知结果。
5. Web 和移动端只显示清理后的摘要、步骤和审批，不泄露凭证、原始命令、diff 或 Provider token。

现有 `docs/design/agent-direct-app-server.md` 声称已删除 executor，但当前 `crates/node/src/runtime/executor.rs` 仍有 `Executor` trait 和 Provider 选择实现。本设计以当前源码为准。

参考报告实际位于 `/Users/harry/.vibe/planning/Volumes/ssd/project/vibeface/openbot-source-comparison-2026-10-30_11-31-26/reports/openbot-vibeface-comparison.md`。任务指定的两个同名路径不存在，因此没有把不存在的路径当作证据。

## 本轮 UI 交付切片

本轮先把已有投影做成一个可读的个人工作台，不新增 Node RPC 或共享事实字段。Web 和手机任务页共享同一组概念：需要留意、进行中、等待中、Bot 归属、目标设备、执行步骤、审批和未知结果。任务详情继续使用现有的 `Question`、`PendingCheck`、`WorkProgress` 和失败原因；未知结果仍然要求核对，不能用重试按钮掩盖。

上下文面板把共享聊天事实、Bot 持久记忆和定时任务、Provider session 交接线索以及 owner 可达性分开显示。协作面板把本机队列、Bot 消息和 `dispatch_work` 分开显示，并把交接记录放在同一条可读时间线上。浏览器操控、远程画面、手机投屏、团队设备池和新的执行权限仍不属于这条 UI 交付切片。
