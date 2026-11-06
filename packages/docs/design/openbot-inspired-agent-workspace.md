# OpenBot 启发的 Agent 工作区设计基线

本文描述个人多 Bot 工作台的领域、页面和数据边界。实现位于 `apps/core/src/workspace.rs`、`store/workspace.rs`、`runtime/workspace.rs`，两端通过 `packages/client/src/workspace` 使用同一协议。源码能力与已发布制品、真实 Provider 和手机验收分别记录。

本文面向一个人管理自己网络中的多个 Bot、任务和设备。工作区聚合已有投影，方便查看跨 Bot 任务、步骤、审批、进度、上下文、会话、队列和协作。它不替换事件 DAG、conversation、scheduler 或 work 的事实来源。

## 范围和排除项

工作区包括：

- **跨 Bot 任务总览。** 按 Bot conversation、任务标题、目标 Node、状态和摘要列出未结工作，并可回到原 conversation。
- **步骤、审批和进度。** 读取 scheduler job、`WorkProgress`、`Question` 和 `PendingCheck`，显示阶段摘要、需要本人处理的卡片和确定结果。
- **上下文和会话。** 展示聊天、决定、checkpoint、会话归属和可续接线索，区分共享事实与本机 Provider session。
- **队列和协作。** 真实频道维护成员、默认协调 Bot 和任务图。每个业务 Task 有稳定身份、独立对话和 Assignment。排队和跨 Node work 保留各目标的真实状态。

本设计明确排除浏览器登录和操作、远程浏览器或桌面画面、手机投屏、组织共享设备池、团队设备管理员、跨个人网络的任务负责人和全局调度员。每个人管理本人设备，公网 Server 仍是 Node 的一种角色。

OpenBot 只提供可借鉴的行为，例如任务分栏、步骤摘要、审批卡、队列状态和会话交接。实现使用 Vibeface 的签名事件、Node 授权和现有投影，不复制 OpenBot 的 PolyForm 许可证源码、资源或实现细节。

## 当前数据形状

以下字段来自当前源码，不能在 UI 层改成掩盖语义的状态名。

| 对象 | 当前事实和工作区用途 |
| --- | --- |
| `WorkspaceTask` | 稳定 `task_id` 保存请求、预期结果、来源、依赖、revision 和当前 Assignment。后续输入保持同一 Task，不跳回 Main。底层 Work 仍是一次执行生命周期。 |
| `Channel` | 保存目的、成员、默认协调 Bot、协调 Node、revision 和 owner epoch。频道历史只合并关联任务及已核验身份的发言。 |
| `Assignment` | 固定任务 revision、Bot、执行 Node、资源目录和因果 basis。执行 Node 要求目录为实际 canonical 目录，拒绝符号链接别名。接受、采用、完成分别记录，旧 revision 的结果不能证明新请求已完成。 |
| `InputPosted` | 本人签名绑定目标、意图、目标内 sequence、预期 revision、正文、附件和汇报选项。相同 message ID 只允许回读原意图。 |
| `open_works` | `Store::open_works(network, scope)` 按 SQL 找出存在 dispatch 且没有结束记录、结果为 `unverified`，或仍待本人看的失败工作；后者是会话里最近结束的失败或被拒工作，之后没有更晚结束的工作、Bot 没有对它做出后续回复（它的结果报告轮的回复，或对失败之后本人消息的回复；派发它的那一轮的回复不算），也没有本人在待处理关掉它（`NotificationAcked`，`client_id = "inbox"`）。本机被拒的派发（未接收）同样列出，直到本人关掉、Bot 对它的报告轮写出已完成的回复，或在没有报告轮时（派发轮内直接得知被拒，或关掉了结果汇报），派发它的那一轮之后本人发的消息有了已完成的回复。已完成的工作不在此列表。 |
| `ConversationPayload` | 客户端快照包含 `conversation`、`messages`、`works`、`scheduler`、`checks`、`local_node`、`changes`、`overwritten`、`bot_settings`、`questions`、`sessions`、`sent`。该快照继续提供 Work 汇总和链接。业务 Task、频道、Assignment 和输入队列从同一签名 DAG 中的 `WorkspaceCommanded`、`InputPosted` 等事件投影，经 `workspace_state` 和 `workspace_history` 读取，不另存 transcript。 |
| `scheduler_jobs` | job 主键语义为 `(conversation_id, trigger)`。`trigger` 可为 `message:<id>`、`work:<id>`、`task:<id>`、`assignment:<id>`、`bot_message:<id>` 或 `schedule:<id>:<slot>`。job 还保存 `kind`、顺序、`basis` 版本向量、`reply_to`、固定 `message_id` 和状态。 |
| `scheduler_sessions` | 本机 Provider resume 指针，含 `native_id`、`native_token`、`covers`、`owner_epoch`、`sdk_config`、`permission_version`、`active` 和下一序号。它不是完整 transcript，也不是同步到 DAG 的上下文。 |
| `ContextCheckpoint` | DAG 事件，含 `covers`、`summary`、`recent_user_requests`、`open_work`、`decisions`。它压缩提示预算，不删除旧事件，不等于 Provider 的 KV memory。 |
| `message_bot` | 写入接收 Bot conversation 的 `BotMessaged { message_id, from, text, hops }`，触发 `bot_message:<message_id>` job。远端目标先进入 outbox。相同轮次和 `call_ref` 重试只回读同一消息。 |
| `dispatch_work` | 用 `WorkSpec { work_id, conversation, trigger, target, dispatcher, call_ref, kind, cwd, title, prompt, continues, parent, thread }` 描述工作。`work_id` 由 `(conversation, trigger, call_ref)` 派生；跨 Node 时 `target` 指明执行设备。 |

`WorkProjection` 的共享状态是 `accepted`、`running`、`interrupting`、`finished`、`unverified`，结果可以是 `completed`、`failed`、`interrupted`、`rejected`、`unverified`。本机运行记录还会暂时使用 `starting`，不应扩展为共享状态。

`scheduler_jobs` 当前状态是 `queued`、`claimed`、`running`、`cancelling`、`reconciling`、`completed`、`failed`、`cancelled`、`interrupted`、`steering`、`steered`。active job 按 `SessionScope` 隔离，同一 scope 不并行启动新轮；不同 scope 仍受 Node 的 `machine_slots`、Bot 的 `task_limit` 和 Provider instance 串行门约束。

管理回合结束不等于 Assignment 完成。它派生的 Work、未知结果和必要汇报仍未结清时，Assignment 保持有效。关闭结果汇报只省略汇报回合，仍须等待真实 Work 结束。同一 Assignment 的后续输入保留原归属，替换 Assignment 不能重新授权旧输入。最终回复使用共享因果证据核验来源与 revision，其他 Node 不依赖执行设备的私有绑定验证完成记录。

## 目标信息架构

### 总览和任务详情

总览优先读取 `workspace_overview`，没有总览快照时从 `open_works` 推导。页面按需要留意、进行中、等待中分组，Web 另提供 Bot 筛选。卡片显示 Bot、任务标题、目标 Node、`kind`、状态、最近摘要和是否需要核对。`dispatch_waiting` 不能显示为已接收，`unverified` 和离线待核对工作优先放到需要留意。

总览来自当前 Node 已知的记录。远端 scheduler 未查询、尚未同步和达到返回上限都需要明确展示，不能把局部快照标成全网络实时任务。已有 `open_works` 不应因为总览截断而从页面消失。首次读取前或冷启动离线时，任务集合未知，不能显示确定的空任务结论。

详情按 `trigger` 和 `reply_to` 说明来源，按 scheduler job 展示排队、领取、执行、取消和回复状态，按 `summary` 与 `WorkProgress` 展示阶段摘要，再展示 `Question`、`PendingCheck`、结果和失败原因。审批卡只代表已有问题或核对记录，不新增绕过设备签名、Bot 轮次或 Node 授权的批准接口。工作区新增的步骤和状态卡只使用经过清理的摘要；Provider token、隐藏命令输出和原始 diff 不应由新增投影引入。当前已有任务详情仍会展示用户交代的 prompt、结果文本、失败原因和项目路径，设计不能把这些现有字段描述成完全不存在。

审批只关联尚未结束的工作，避免复用同一 Provider session 的新审批出现在旧任务里。Web 和手机都可展开真实的步骤摘要；普通摘要保留原文，不生成额外步骤、百分比或完整执行历史。手机详情也需直接显示所属 Bot。未知结果的“先查结果”只刷新工作状态，保留在当前详情，不创建主聊天输入或重发原任务。稍后输入的编辑和取消使用该输入的最新控制事件，不通过取消整个协调回合替代。

### 上下文和会话

上下文页读取 `scope_context`，显示实际采用的来源、basis、generation、预算、省略数量和逐条输入回执。Main、业务 Task、Work 和 Routine 使用明确目标。未提供的 checkpoint 或完整 decisions 不能显示为零或已清空。持久 Memory、共享记录和 Provider 本地缓存分别展示。

`sessions` 来自外部会话的 `SessionAttributed` 归属记录。页面可列出设备、Provider、session ID、目录和静音状态，但它不证明 Bot 自身的原生运行会话存在或不存在，也不证明该会话仍可续接。没有归属记录时，只能说明没有已接入的外部会话。原生 resume token 不进入工作区。

页面必须区分共享聊天事实、Node 本地的 `scheduler_sessions` 指针和 Provider 自己的历史。原生 session 丢失时显示“从共享记录重建”或“原生会话不可用”，不能声称 KV context 已迁移。

### 队列和协作

队列按 job `sequence` 排序，并保留人类消息优先规则。跨 conversation 的并行状态按各自 Node 和 job 展示，等待原因保留设备槽、Bot 任务上限和 Provider 串行门的区别，不合并成跨 Node 的全局队列。

快照未提供执行顺序字段时，UI 只能显示观察到的调用状态，不能称为实际排队顺序。`queued`、`cancelling` 和已写回复但尚待同步的 `publishing` 使用各自的状态文案。历史调用与等待执行的调用分开，不能统一标为正在运行。

自动分配遇到临时目录占用时，已接纳的输入保留原 ID 排队，并显示资源等待。资源释放后再固定 Assignment 和执行，不把临时占用保存成永久拒绝。等待中的输入保留 Task revision 和原 owner epoch；已绑定的输入不能借后续 Assignment 执行。Unknown 仍保留资源，不能为解除等待而自动释放。

`message_bot` 是 Bot 间消息，显示发送方、接收方、正文、hops 和投递状态。`dispatch_work` 是在某个 Node 上执行的工作，显示 dispatcher、target、work ID、结果和回执。两者可以关联，但不能互换。Bot 消息没有工作结果，工作派发也不等于聊天消息。

`workspace_history` 按 Task 或 Channel 过滤共享记录，发言身份由已验证事件确定。Main 不接纳其他 scope 的消息、问题或调度进度。来源不明的 Bot 消息保留省略数量，不能默认归入 Main。手机与网页使用同一已认证投影。

## 连续输入和显式待办

发送默认继续选定目标。显式“新任务”创建独立 Task，显式“稍后处理”才进入待办。客户端按 message ID 持久化多项输入，本地接纳后即可继续编辑；首条 RPC、附件上传或刷新尚未结束不阻止下一条保存。频道首次创建 Task 时，后续输入复用同一个已持久化的目标。

`input_receipt` 区分入口保存、owner 接纳、提交中、Provider 接受、实际回合归属、拒绝和未知。Provider 接受不表示模型已读。附件保留完整引用与内容，同目标不得越过尚未准备好的附件。不同目标的投递和旧输入回读独立推进。

稍后队列的编辑、释放、取消和移动通过 `ControlInput`，以 `control_event` 做 CAS。owner 重新核验尚未开始，不能仅凭客户端的 deferred 标记编辑已提交输入。明确启动前失败可由本人用新的签名 Release 重试，必须绑定最新 control_event，并由 Node 证明原 job 为 NotStarted、没有工具调用、未决检查或在途 outbox。任务 Assignment 同时引用原失败与未启动证据，保留原输入和旧失败历史，重试回复有独立稳定身份。取消、Unknown、已经开始或 revision 改变均不能复活。未知写入只查询原 ID，不换 ID 重发。停止使用独立控制路径，保留未发送草稿和焦点。

停止时，已获 Provider 接受回执的补充仍核对原 thread 和 turn；宿主结束本身不能替代逐条输入结果。没有接受回执且宿主已结束的取消输入保持 Unknown，迟到的未到达结果不能把它重新入队。两条路径均不重新发送原输入。

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

当前形状由这些源码提供：`apps/core/src/store.rs`、`apps/core/src/model.rs`、`apps/core/src/work.rs`、`apps/core/src/scheduling.rs`、`apps/core/src/runtime/work.rs`、`apps/core/src/runtime/chat_controls.rs`、`apps/core/src/runtime/schedule_delivery.rs`、`apps/core/src/store/questions.rs`、`apps/core/src/store/bot_settings.rs` 和 `packages/client/src/workspace/types.ts`。

验收需要在对应提交的真实制品上分别记录模块回归、Node 进程、跨 Node、浏览器和原生手机结果。受控夹具不证明真实模型理解质量。核心检查包括：

1. 多 Bot 总览能合并 `open_works`，并保持 settled、`unverified` 和 delivery 的边界。
2. 同一 `call_ref` 的 `message_bot` 和 `dispatch_work` 重试只回读，不产生第二条消息或第二个副作用回合。
3. scope 单 active job、跨 scope 并行、Node 槽、Provider 串行门、steering 和取消与 scheduler projection 一致。
4. Node 重启或 native session 丢失后，checkpoint 和事件能重建可核对输入，并保留未知结果。
5. Web 和移动端只显示清理后的摘要、步骤和审批，不泄露凭证、原始命令、diff 或 Provider token。

Node 保留 `apps/core/src/runtime/executor.rs` 中的 `Executor` trait 和 Provider 选择实现，与 [Agent 直连 App Server 设计](agent-direct-app-server.md) 一致。

## 页面与权限

Web 的 `/tasks`、`/tasks/$taskId`、`/channels` 和 `/channels/$channelId` 提供任务与频道。原生手机提供对应独立页面、Context、创建和频道设置入口。Work 详情独立读取终态记录，保留来源及覆盖不完整说明。终态 Work 关联业务 Task 时继续原 Task，否则通过显式新任务保留原请求、结果和真实来源引用。

自动子任务受深度、数量、依赖、资源目录和 `reply_only` 限制。频道配置、Bot owner 与 Work 执行授权分别核验。普通配置修改保留已捕获回合；撤权先围栏再取消，未知执行保留核对。手机接入沿用已有个人网络配对、客户端签名和 Node 授权，不新增组织设备池。
