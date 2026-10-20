# Codex App Server 方法覆盖表

本表由 `crates/bridge/src/methods.rs` 维护。方法名单来自 `crates/bridge/schema/app-server-methods.json`，用 `scripts/codex-schema.py` 从已安装的 Codex 生成（当前 0.163.0）。`methods.rs` 的覆盖测试要求名单里每个方法都出现在方法表和本文件里。协调 Agent 用 `vibeface codex call` 可调用任何方法；网页不调用 App Server 方法；分类决定写方法结果不明时不重发。Codex 升级新增方法时 CI 会失败，直到补上分类。

执行的命令、命令输出、diff 和改动文件列表只进观察状态，送往网页前由 `sanitize` 去掉。服务端请求交给协调 Agent 处理，见下文。

## 客户端请求

| 方法 | 分类 | 处理与展示 |
|---|---|---|
| `initialize` | 隐藏 | 连接建立时由客户端发送，不对外暴露 |
| `thread/start` | 写 | `dispatch_work` 先登记再发；协调 Agent 也可用 `vibeface codex call` 自开线程 |
| `thread/resume` | 写 | Node 订阅观察；`send_decision` 先 resume |
| `thread/fork` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/archive` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/delete` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/unsubscribe` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/name/set` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/goal/set` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/goal/get` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/goal/clear` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/metadata/update` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/attachment/add` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/attachment/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/attachmentOwner/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/attachment/remove` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/section/move` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/unarchive` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/compact/start` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/shellCommand` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `thread/approveGuardianDeniedAction` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/revert` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `threadSection/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `threadSection/create` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `threadSection/update` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `threadSection/delete` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `thread/loaded/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/turns/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/items/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `thread/inject_items` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `skills/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `skills/extraRoots/set` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `hooks/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `marketplace/add` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `marketplace/remove` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `marketplace/upgrade` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `plugin/installed` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `plugin/reconcile` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `plugin/skill/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `plugin/share/save` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/share/updateTargets` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/share/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `plugin/share/checkout` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/share/delete` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `app/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `app/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `app/installed` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `fs/readFile` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/writeFile` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/createDirectory` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/getMetadata` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/readDirectory` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/remove` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/copy` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/watch` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `fs/unwatch` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `skills/config/write` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/install` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `plugin/uninstall` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `turn/start` | 写 | `dispatch_work`、`vibeface codex send` 与 `send_decision` 发送，带 `clientUserMessageId` |
| `turn/steer` | 写 | `vibeface codex send` 与 `send_decision` 共用同一发送函数，带 `clientUserMessageId` |
| `turn/interrupt` | 写 | 停止按钮经 Node 发送；协调 Agent 也可直接调用 |
| `review/start` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `model/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/gatewayOAuth/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/gatewayOAuth/login` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `account/gatewayOAuth/cancel` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `modelProvider/capabilities/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `experimentalFeature/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `permissionProfile/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `experimentalFeature/enablement/set` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `mcpServer/oauth/login` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `config/mcpServer/reload` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `mcpServerStatus/list` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `mcpServer/resource/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `mcpServer/tool/call` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `windowsSandbox/setupStart` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `windowsSandbox/readiness` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/login/start` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `account/login/cancel` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `account/logout` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `account/rateLimits/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/rateLimitResetCredit/consume` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `account/usage/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/workspaceMessages/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/sendAddCreditsNudgeEmail` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `feedback/upload` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `command/exec` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `command/exec/write` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `command/exec/terminate` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `command/exec/resize` | 隐藏 | 不经 Node 暴露给网页；协调 Agent 用 `vibeface codex call` 可调用 |
| `config/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `externalAgentConfig/detect` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `externalAgentConfig/import` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `externalAgentConfig/import/recordHistory` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `externalAgentConfig/import/readHistories` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `config/value/write` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `config/batchWrite` | 写 | 协调 Agent 经 `vibeface codex call` 调用；结果不明时对账，不重发 |
| `configRequirements/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `account/read` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |
| `fuzzyFileSearch` | 只读 | 协调 Agent 经 `vibeface codex call` 调用 |

## 客户端通知

| 方法 | 处理 |
|---|---|
| `initialized` | `initialize` 成功后发送一次 |

## 服务端请求

工作线程用 `approvalPolicy=never` 和 `danger-full-access` 启动，正常不会收到审批。Node 订阅的线程上的请求和不属于线程的请求都没有固定策略：Node 挂起请求，记一行日志（方法和线程，不含命令文本），立即唤醒本机协调 Agent。Agent 用 `vibeface codex requests` 查看、`vibeface codex respond <request_id> '<result-json>'` 回答；需要本人拍板时用 `vibeface triage raise --request-id` 生成拍板卡片，卡片挂住请求直到本人选择，忽略卡片等于拒绝。没有协调 Agent、唤醒失败或 120 秒内没有回答也没有卡片时，按下表兜底并记录。Node 没有订阅的线程留给原客户端回答。

| 方法 | 兜底回复 |
|---|---|
| `item/commandExecution/requestApproval` | `{"decision":"decline"}` |
| `item/fileChange/requestApproval` | `{"decision":"decline"}` |
| `item/tool/requestUserInput` | `{"answers":{}}` |
| `mcpServer/elicitation/request` | `{"action":"cancel"}` |
| `item/permissions/requestApproval` | `{"permissions":{},"scope":"turn"}`，不授予权限 |
| `item/tool/call` | 返回失败，Node 不提供动态工具 |
| `account/chatgptAuthTokens/refresh` | 返回 JSON-RPC 错误，Node 不持有凭证 |
| `attestation/generate` | 返回 JSON-RPC 错误，Node 不持有凭证 |
| `applyPatchApproval` | `{"decision":"denied"}` |
| `execCommandApproval` | `{"decision":"denied"}` |

## 服务端通知

| 方法 | 类别 | 进入观察状态的方式 |
|---|---|---|
| `error` | Turn | 回合开始、结束、错误进入线程观察状态，驱动工作状态机 |
| `thread/started` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/status/changed` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/readState/changed` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/archived` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/deleted` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/unarchived` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/closed` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/reverted` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `skills/changed` | Skills | 记录技能变化时间，网页重新 skills/list |
| `thread/name/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/attachment/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/goal/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/prediction/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/goal/cleared` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/queue/changed` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `project/changed` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/project/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/environment/connected` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/environment/disconnected` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/settings/updated` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `thread/tokenUsage/updated` | Usage | token 用量进入线程观察状态 |
| `turn/started` | Turn | 回合开始、结束、错误进入线程观察状态，驱动工作状态机 |
| `hook/started` | Hook | 记录最近一次同名事件 |
| `turn/completed` | Turn | 回合开始、结束、错误进入线程观察状态，驱动工作状态机 |
| `hook/completed` | Hook | 记录最近一次同名事件 |
| `turn/diff/updated` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `turn/plan/updated` | Plan | 计划步骤与说明进入线程观察状态，并节流写入 WorkProgress |
| `item/started` | Item | 条目按 id 存入当前回合；最终回答进入 last_assistant |
| `item/autoApprovalReview/started` | Review | 记录最近一次同名事件 |
| `item/autoApprovalReview/completed` | Review | 记录最近一次同名事件 |
| `autoApprovalReview/strictReviewRequired` | Review | 记录最近一次同名事件 |
| `item/completed` | Item | 条目按 id 存入当前回合；最终回答进入 last_assistant |
| `item/agentMessage/delta` | Item | 条目按 id 存入当前回合；最终回答进入 last_assistant |
| `item/plan/delta` | Plan | 计划步骤与说明进入线程观察状态，并节流写入 WorkProgress |
| `command/exec/outputDelta` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `process/outputDelta` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `process/exited` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `item/commandExecution/outputDelta` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `item/commandExecution/terminalInteraction` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `item/fileChange/outputDelta` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `item/fileChange/patchUpdated` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `serverRequest/resolved` | Request | 从待答请求中移除 |
| `item/mcpToolCall/progress` | Item | 条目按 id 存入当前回合；最终回答进入 last_assistant |
| `mcpServer/oauthLogin/completed` | Mcp | MCP 启动状态按服务器名进入全局观察状态 |
| `mcpServer/startupStatus/updated` | Mcp | MCP 启动状态按服务器名进入全局观察状态 |
| `mcpServer/event/stream/notification` | Mcp | MCP 启动状态按服务器名进入全局观察状态 |
| `account/updated` | Account | 账户类型与套餐进入全局观察状态，不含凭证 |
| `account/gatewayOAuth/changed` | Account | 账户类型与套餐进入全局观察状态，不含凭证 |
| `account/rateLimits/updated` | RateLimits | 额度进入全局观察状态 |
| `app/list/updated` | Apps | 应用列表进入全局观察状态 |
| `remoteControl/status/changed` | Platform | 记录最近一次同名事件 |
| `externalAgentConfig/import/progress` | Import | 记录最近一次同名事件 |
| `externalAgentConfig/import/completed` | Import | 记录最近一次同名事件 |
| `fs/changed` | Output | 存入 `hidden`，sanitize 后不送网页 |
| `item/reasoning/summaryTextDelta` | Reasoning | 推理摘要按段累积进入线程观察状态 |
| `item/reasoning/summaryPartAdded` | Reasoning | 推理摘要按段累积进入线程观察状态 |
| `item/reasoning/textDelta` | Reasoning | 推理摘要按段累积进入线程观察状态 |
| `thread/compacted` | Thread | 线程状态、名称、归档、设置、目标等进入线程观察状态 |
| `model/rerouted` | Model | 记录最近一次同名事件 |
| `model/verification` | Model | 记录最近一次同名事件 |
| `modelProvider/authRecoveryStarted` | Account | 账户类型与套餐进入全局观察状态，不含凭证 |
| `modelProvider/authRecoveryCompleted` | Account | 账户类型与套餐进入全局观察状态，不含凭证 |
| `turn/moderationMetadata` | Turn | 回合开始、结束、错误进入线程观察状态，驱动工作状态机 |
| `model/safetyBuffering/updated` | Model | 记录最近一次同名事件 |
| `warning` | Warning | 进入最近 20 条警告 |
| `guardianWarning` | Warning | 进入最近 20 条警告 |
| `deprecationNotice` | Warning | 进入最近 20 条警告 |
| `configWarning` | Warning | 进入最近 20 条警告 |
| `fuzzyFileSearch/sessionUpdated` | Search | 记录最近一次同名事件 |
| `fuzzyFileSearch/sessionCompleted` | Search | 记录最近一次同名事件 |
| `thread/realtime/started` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/itemAdded` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/item/started` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/item/transcript/delta` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/item/completed` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/transcript/delta` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/transcript/done` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/outputAudio/delta` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/sdp` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/error` | Realtime | 记录最近一次同名事件 |
| `thread/realtime/closed` | Realtime | 记录最近一次同名事件 |
| `windows/worldWritableWarning` | Platform | 记录最近一次同名事件 |
| `windowsSandbox/setupCompleted` | Platform | 记录最近一次同名事件 |
| `account/login/completed` | Account | 账户类型与套餐进入全局观察状态，不含凭证 |
