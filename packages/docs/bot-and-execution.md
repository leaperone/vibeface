---
title: Bot 如何协调，Agent 在哪里执行
description: 分清共享 Bot、协调角色与本机执行，了解复用 Codex 和 Claude Code 的条件。
---

# Bot 如何协调，Agent 在哪里执行

Bot 负责在长期聊天里理解和协调工作，选定电脑上的 Agent 执行具体任务。两种角色可以使用不同产品，任务结果仍回到同一段 Bot 聊天。

## Bot 与执行分开

Bot 的名称、职责、项目偏好和聊天是共享记录。创建 Bot 不会立即启动模型。当前负责 Node 上的协调 Agent 根据这些记录组织一轮回复，再派发需要执行的工作。

执行 Agent 有自己的原生工作会话。即使协调和执行都选择 Codex，也不能把共享 Bot 聊天当作一份可以搬到任意电脑的 Codex 私有会话。共享的是业务历史，本机程序和 SDK 缓存各自保留。

当前接入 Codex、Claude Code、pi、opencode 与 Cursor。它们的图片、补充输入和续接会话能力有差异，接入列表不代表全部产品与模型组合都已实测。操作步骤见 [配置管理 Agent](node.md#配置管理-agent)。

## 复用本机 Codex 与 Claude Code

Vibeface 使用你已经安装的程序，以及为该 Node 配置的登录或认证环境。它不安装这些 Agent，不代你登录，也不包含模型与额度。原先的终端登录不一定适用于 Node 的私有运行目录，首次使用按设备 **AI** 设置提供的命令登录。本机运行程序仍可能把任务内容发送到所选模型服务。

### Codex

Node 可以连接已有 Codex App Server 的指定 socket，也可以用本机 `codex` 启动自己的 App Server。连接已有服务时沿用该服务的环境；由 Node 启动时，使用 Node-owned 的 `CODEX_HOME`、sessions 和 hooks 根，不复用用户的交互目录。后台服务需要能找到相应程序，并在 Node-owned 根中完成登录或配置。

这不意味着 Node 能接管任意已打开的 Codex 会话。连接方式、路径和工作条件见 [使用 Codex SDK](node.md#使用-codex-sdk) 与 [配置 SDK 与 App Server](deployment.md#配置-sdk-与-app-server)。

### Claude Code

Node 启动本机 `claude` CLI，并使用 `<data-dir>/providers/claude` 作为私有 `CLAUDE_CONFIG_DIR`，或使用为该服务配置的认证变量。按设备 **AI** 设置中的命令为此目录登录，不自动复制用户原有登录。续接原生 session 需要那台电脑保留对应的本机 transcript，不能只靠同步 Bot 聊天完成。

同一台电脑上，本人另外打开的交互会话也不自动成为 Vibeface 的任务会话。准备方式和会话限制见 [使用 Claude Code、pi 或 opencode](node.md#使用-claude-codepi-或-opencode)。

## 工作如何回到原聊天

协调 Bot 登记一项工作后，Node 才交给执行器。派到另一台设备时，目标设备自己的协调 Agent 选择本机执行产品。发起方不能指定远端使用哪一种 Provider。

任务阶段、摘要和持久结果写回共享历史，协调 Agent 再向原聊天汇报。逐字流式进度属于来源 Node 上的临时观察数据，重连后不能依靠它还原全部输出。

排队、开始执行和执行完成是不同状态。回执丢失或原会话不可达时，Node 先核对已登记的工作；无法确认就保留为待核对，不通过换设备重发来猜测结果。详见 [待核对](node.md#待核对) 与 [历史同步](history-sync.md)。

## 另一台设备需要准备什么

目标电脑需要加入同一个人网络，并具备相应的本机 Agent、登录、模型服务和工程目录。项目文件与依赖不会随聊天自动复制。添加设备见 [添加自己的设备](node.md#添加自己的设备)。

新任务的真实目录要通过目标 Node 的 `roots` 检查。Bot 的项目偏好不会自行授予目录权限。`roots` 也不是操作系统沙箱：协调 Agent 可以访问宿主机，Codex 与 Claude Code 使用完全访问权限。授权前应阅读 [执行权限与项目目录](privacy-security.md#执行权限与项目目录)。

从另一台 Node 接着协调，还需要先同步历史，再由本人切换负责人。它会建立自己的本机 Agent 上下文，条件见 [接着协调需要哪些条件](history-sync.md#接着协调需要哪些条件)。
