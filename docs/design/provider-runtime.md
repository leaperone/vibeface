# Bot Provider 运行契约

本设计约束 Bot 在本机 Provider 上的启动、回合配置、上下文续接和失败恢复。
每台 Node 选择本机协调 Provider，Bot 的共享聊天仍以个人网络的事件 DAG 为事实源。
Provider 的登录材料和原生会话保留在运行设备上。

## 准备状态

程序存在、进程能够启动、账户已认证和模型可用是不同的事实。
状态响应必须说明检查阶段、检查时间、配置标识和可操作原因。
无法确认认证或模型时返回未知，不能把程序路径检查当作模型可用。
状态查询不运行模型，也不安装程序或替用户登录。

## 回合配置

回合启动时固定 Provider、模型、推理档位和本地配置标识。
配置变更只作用于后续回合。Provider 不支持的档位在发送前拒绝。
回合记录保存提交给 Provider 的配置，不把提交值当作 Provider 已确认的实际选择。
Provider 没有确认实际模型或推理档位时，实际选择保持未知。
日志与页面不得通过原始账户响应或环境变量展示凭证。

## 上下文和恢复

原生会话是 DAG 的本地执行缓存。会话更换要保留上下文来源、未完成工作和用户决定。
历史读取只返回当前聊天的有限分页，省略内容必须能够按稳定标识读回。
Provider 能原生压缩时记录压缩状态，否则通过有界上下文交接建立新会话。
恢复只读取原回合结果，不从历史重新生成工作。

## 工具和重试

原生工具与 CLI 使用同一目录、权限检查、参数解析和调用记录。
同一意图使用同一 `call_ref`，结果未知时先查询原记录。
发送未确认、回合已失败和副作用未知是不同状态，页面必须提供对应动作。
备用模型默认不自动启用。自动切换只允许已经确认未执行的回合，并限制每个候选的尝试次数。

## 验证

使用受控 Provider 进程验证认证、模型目录、配置传递、断线、重启、取消和重复请求。
用真实 Node 进程验证同一意图只执行一次，并检查 Web 和 Mobile 的恢复入口。
协议夹具不证明真实模型的理解质量或 Provider 账户可用。

## 当前实现

Bot 模型和执行任务默认模型是两个独立设置。执行任务在 Node 接纳边界按显式请求、`providers.execution_models`、Provider 默认的顺序解析，并把 `model` 请求值与必需的 `resolved { provider, model }` 冻结到 `WorkDispatched` 和投影。`resolved.model=null` 明确选择 Provider 默认，与缺少整个 `resolved` 不同，不保证 Provider 内部的默认型号永久不变。远端任务不接受发起端显式模型，由目标 Bot 冻结自己的模型。

同一请求重试先比较并回读已接纳记录，之后关闭执行、切换 Bot Provider 或修改模型不会改变原接纳结果，也不会再次入队或发送。首次并发接纳由 Store 事务裁决，所有成功返回都使用已持久化的同一选择。本机省略 `agent` 的请求在首次接纳时选择 Provider，后续重试沿用该选择；显式更换 Provider 或请求内容则拒绝。

已接纳的远端 Task 只有在当前 Bot 仍使用同一 Provider 时才可启动。Bot 关闭或换用其他 Provider 时，尚未启动的 Task 记为明确失败，执行证据为 `NotStarted`，保留原 Provider 和模型。此收尾不启动 SDK、不改变其他活跃回合，也不将已启动或结果未知的工作降为未启动。

Codex 默认使用 Node-owned `<data-dir>/providers/codex`、sessions 和 hook 根，并由 Node 启动独立 App Server。Claude 使用 Node-owned `CLAUDE_CONFIG_DIR`、transcript 和 hook 根。Codex system 只有通过显式 socket 才接入外部服务，并共享该服务的会话。pi 只固定 `<data-dir>/providers/pi/sessions`，保留本机认证配置。OpenCode 使用 `<data-dir>/providers/opencode` 中的私有 XDG 数据、配置、状态和缓存，强制数据库与配置目录；外部 config override 和项目配置不参与该服务。启动和 session Shell 环境使用相同根。

Codex 通过只读账户和模型接口检查，Claude Code 通过无 prompt 的登录状态命令检查，pi 通过无会话、无工具的 RPC 提供模型建议。Cursor 和 opencode 缺少已核实的账户查询时显示未确认。目录不完整时保留手填模型，不因某个模型未出现在列表中而阻止发送。

每轮提交模型与推理档位，并记录配置 generation 和会话 lineage。未指定模型时，实际模型保持未知。Codex、Claude Code 和 pi 的原生压缩事件更新 lineage。没有原生压缩的服务在上下文超过有界输入时交接新会话，摘要、决策、最近消息和 open work 仍来自 DAG。

运行配置 generation 与探测实例 ID 分开。generation 使用设备私有密钥，对固定回合的有效启动配置计算 HMAC；相同配置在 Node 重启后保持一致。进程模式包含程序路径与显式环境变量，系统 App Server 模式只包含所选 socket。模型、推理档位、工作目录和工具指引继续参与会话兼容性判断。随机探测 ID 和 epoch 只负责拒绝过期探测结果，不再使原生会话失效。generation 不包含可反推低熵凭证的公开摘要，原始环境变量不进入共享记录。

重启后的回合处于 `reconciling`。按原执行标识读取结果，明确结束后才提交最终回复。只有进程退出、没有可读历史或查询失败时继续等待确认，不重新发送 prompt。

备用策略保存在 Node 本地，默认关闭。开启后按顺序选择一个不同候选，最多追加一次尝试。跨服务候选须在本机启用执行角色，使用该设备配置的账户。只有原回合明确未启动、没有工具调用和待确认记录时才切换。已接纳的远端 Task 不自动切换备用，始终保留冻结的 Provider 和模型。记录保留原选项、实际提交选项和切换原因。

私有 Codex、Claude Code 和 OpenCode 根可能需要重新认证。`ProviderStatus.setup` 返回非敏感目录和可复制登录命令，Web 与 Mobile 提供复制和重新检查入口；Node 不自动登录，不读取或复制用户凭证。未确认的认证仍显示未知，已确认缺少登录时显示需要设置。

存储使用 epoch 9 最终结构与事件协议 1。同一网络要求所有 Node 使用本轮最终格式，不能混用 epoch 8 或 epoch 9 的旧开发快照；本机 epoch 校验不是 peer 握手。旧形状会拒绝新增字段。程序不会迁移或清空旧 epoch 8 数据。受控协议、真实账户、模型理解质量和手机验收分别记录证据。
