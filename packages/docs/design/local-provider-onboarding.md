# 本机 AI 服务接入

本机接入支持 Codex、Claude Code、Cursor，保留 Pi、OpenCode。发现、认证来源与实际可用性分别呈现。自动接入不安装程序，也不启用 Bot 或任务执行。

## 程序与设置

启动为已有用途补缺，设备页的 `configure_provider {provider, connect_local:true}` 可以单独准备服务。Web 进入 AI 设置及返回页面时只自动接入并检查 Cursor，登录变化会重新观察，不复用旧检查结果。手动点**自动接入**时按 Codex、Claude Code、Cursor 顺序处理，一项失败仍继续其余服务；写入结果未知时先读完整设置对账。手机开启相应用途走同一接入路径。

程序先使用保存的选择和 PATH，再查进程 PATH、常见用户目录、Homebrew、npm/Volta/Bun/NVM 安装。Cursor 支持官方 `agent`、`cursor-agent` 入口；Windows 使用 PATHEXT。NVM 按有效 Node 版本降序选择。npm wrapper 启动时补齐 sibling Node 或其他已发现的 Node 目录；仅能从登录 shell 找到所需 Node 时，保存受限 PATH 快照。已选择的绝对程序路径即使失效也不覆盖，Cursor 的旧 SDK Bridge 选择除外，自动接入会重新发现并保存官方 CLI 的绝对路径。有效自定义 CLI 选择保持不变。

仍找不到程序且没有保存 bin/PATH 时，仅查询操作系统登记登录 shell 的 PATH。Codex 的声明认证变量也沿用既有补缺流程。只读取验证过的固定名称，不导出整个环境或执行任意命令。shell 使用随机 NUL 帧、输出与时限限制，结束时回收进程树；用户启动脚本可能有自身副作用。

除 Cursor 外，保存的显式认证优先，其次是 Node 真实继承的命名认证环境，最后才复用本机 CLI 登录。`null` 只删除私有环境覆盖，不禁止进程继承或后续补缺。认证值不回读，不进入共享事件、工作记录或日志。

## Codex 与 Claude Code

Node 的 CODEX_HOME、CLAUDE_CONFIG_DIR、会话和 hooks 继续私有。模型配置投影与认证导入是两件事；投影不复制个人会话、hooks、MCP 或项目权限。

Codex 从本机 CODEX_HOME 或默认 `.codex/auth.json` 读取 CLI 登录。私有镜像采用官方 `chatgptAuthTokens` 外部认证模式，保留访问令牌、ID token 与账户信息，必填的 refresh_token 写为空字符串。该模式不执行原 CLI 的 managed refresh。不能仅清空 refresh_token 并保留 managed `chatgpt` 模式。仅有系统 Keychain 的 Codex 登录目前需要为 Node 独立登录。

Claude 默认 macOS CLI 登录从当前 OS 用户的 `Claude Code-credentials` Keychain 项读取，跳过需要交互授权的项目，不弹出授权窗口。只有未设置 CLAUDE_CONFIG_DIR 且 HOME 是实际 OS 账户目录时才查询默认 Keychain。显式配置目录只读取其中 `.credentials.json`，不猜其 Keychain 命名空间。其他平台读取官方 credential 文件。Node 专属访问快照移除单次使用的 refreshToken，只保留访问令牌和元数据，不创建或覆盖 Claude CLI 的 `.credentials.json`。

Codex 的导入标记保存首次私有 auth.json 的摘要，该文件以不覆盖方式创建。后续检查或必要启动只更新 Node 专属的 local-login-access.json。文件仍匹配导入标记时，Codex 把 external auth 作为启动目标的一部分，在空闲切换后、发布 App Server endpoint 之前通过官方 account/login/start 写入内存。用户后来独立登录或修改 auth.json 后，Node 放弃借用，不再替换该文件。

Claude 在检查或每次必要启动前，先用不含借用令牌的官方 `auth status --json` 检查私有登录，包括私有目录对应的 Keychain。只有明确未登录且没有私有 `.credentials.json` 时，才更新 Node 的访问快照，并临时注入 CLAUDE_CODE_OAUTH_TOKEN。私有登录成功、检查失败或状态不明时均不借用。后来为 Node 独立登录后，下一次检查或启动会自动使用该私有登录。

原 CLI 始终保有刷新权，Node 不消耗源刷新令牌，也不替原 CLI 登录。源令牌没有更新且借用快照过期时显示需登录，可先在原 CLI 完成刷新再检查，或使用设备页的私有登录命令。

认证存在不能证明有效。探针仍使用实际运行目录和环境，发现和导入不会把状态设为 ready。Codex 新认证与配置变化使用既有空闲重载边界，保留活跃任务，不重发提示。

## Cursor

Cursor 只通过官方 `agent` 或 `cursor-agent` 的 ACP 接入，复用本机 CLI 的订阅登录。Node 和网页不请求、导入、传递或使用 CURSOR_API_KEY、CURSOR_AUTH_TOKEN，也不覆盖官方 credential store。已有私有文件中的这些认证覆盖不会参与运行，新设置会被拒绝，且不会阻止 Node 加载。缺认证时提供所选官方 CLI 的 `login` 命令，命令会清除继承的认证覆盖，不自动打开浏览器。旧 SDK Bridge 不是可运行入口，也不生成其登录命令。

订阅检查运行 `agent status --format json`，只保留状态枚举和认证布尔值，丢弃身份及消息。未登录也可能退出 0，退出码不能作为已认证证据。只有完整本地登录才继续 ACP 初始化。默认检查和执行从不调用会触发浏览器登录的 ACP authenticate。

ACP 会话使用每个 Node owner 的私有 CURSOR_CONFIG_DIR 和 CURSOR_DATA_DIR，保持真实 HOME 供官方 CLI 读取认证。该目录行为由 Cursor `2026.11.05-86047df` 官方发行源码确认；ACP 历史实际在 config 根的 `acp-sessions` 下，不能仅设置数据根。Node 只新建或加载自己持久记录归属的会话，不扫描个人历史。官方 CLI 仍可读取用户及项目的 MCP、规则或 hooks，私有根不代表禁用这些 CLI 资源。

检查可以创建私有空 ACP 会话以取得模型目录，不提交 prompt 或调用模型。模型目录失败不等于确定认证失败。本地登录状态、元数据检查与真实订阅调用分别报告。

Node 工具通过仅回环监听的 `/mcp` 接入，所有请求使用当前 attempt 的 bearer，调用同一 Runtime.tool 路径，保留个人网络、聊天、任务、撤销和 call_ref 去重规则。加载旧会话只提供当前 token；历史 update 不作为本轮回复，加载期间的权限请求不授予执行。当前已授权任务的已知工具仅选择 allow_once，未知工具、未知请求与阻塞交互均明确拒绝或取消，不授予持续权限。

## 提交、停止与恢复

Cursor 每轮使用独立受管 ACP 进程，同一 owner 串行，不同 owner 可并行。会话归属先落盘，再保存 Node 提交凭证，之后才能发送 prompt；真实活动出现后才记录启动证据。终态先写私有 journal，再向调用方报告完成。

Node 的提交凭证不是 Cursor 原生 run ID。ACP 没有持久 run 查询或幂等 prompt 契约；恢复只读取精确 journal，不新发 prompt，不加载历史来推断某个提交成功。断流或崩溃缺少终态时保持待核对。停止只有收到原 prompt 的 cancelled 终态才算确认，杀掉进程不能证明取消。

受控夹具验证以上边界，不读取个人认证、不调用真实模型。真实订阅、Keychain 访问授权及其他平台制品需单独核验，不能由程序发现、文件存在或 HTTP 成功替代。
