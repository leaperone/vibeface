---
title: 已有 Agent，为什么还用 Vibeface
description: 按工作方式比较 Codex、Claude Code、OpenClaw 与 Vibeface，选择需要的协调方式。
---

# 已有 Agent，为什么还用 Vibeface

Vibeface 适合需要协调自己的多台设备、保留共享 Bot 历史的人。已有 Codex 或 Claude Code 工作流程足够时，可以继续直接使用原工具。

## 先按你的工作方式选择

- 你主要在终端、IDE 或官方远程入口完成编码，希望直接控制该 Agent 的流程，可以选择 Codex 或 Claude Code 原生工具。
- 你希望把常用聊天应用接到自托管的 Agent Gateway，可以了解 OpenClaw。
- 你希望在同一段 Bot 聊天中选择自己的执行电脑，并在授权设备间查看任务历史，可以了解 Vibeface。

Codex、Claude Code 和 OpenClaw 也有远程或自动化方案。选择的区别在于你需要怎样组织工作，具体能力和使用条件以各产品官方文档为准。

## 各种产品关注什么

| 产品 | 官方方案的关注点 | 适合的工作方式 |
| --- | --- | --- |
| Codex 原生工具 | 在终端检查与修改本地仓库、运行命令，通过 `codex exec` 组合脚本与流水线。 | 直接围绕 Codex 的原生入口和自动化流程工作。 |
| Claude Code 原生工具 | 读取代码、编辑文件、运行命令，提供终端、IDE、桌面与浏览器入口。 | 直接在 Claude Code 支持的入口中完成编码与开发自动化。 |
| OpenClaw | 自托管 Gateway，把多个聊天渠道接到编码 Agent，并提供会话、路由和自动化能力。 | 围绕常用聊天应用接入和组织 Agent。 |
| Vibeface | 个人设备身份、长期 Bot 协调、选定电脑执行，以及签名 DAG 共享历史。 | 复用自己电脑上的 Agent，在自己的设备之间协调工作和查看结果。 |

这里描述产品定位，不代表完整能力清单，也不判断各产品的安全性或模型质量。

## Vibeface 增加了什么

Bot 的聊天与设置独立于本机 SDK 会话。协调 Agent 可以把具体工作交给另一台已授权电脑，结果再回到原聊天。协调和执行可以分别选择产品。复用 Codex 或 Claude Code 的条件见 [Bot 与任务执行](bot-and-execution.md)。

签名 DAG 保存 Bot 聊天、设置、任务阶段和结果，已授权 Node 重连后补齐缺失记录。另一台 Node 接着协调前，需要先同步，再由本人切换负责人。项目、依赖、登录、逐字进度和原生 SDK 状态不会自动迁移，详见 [历史同步](history-sync.md)。

这也增加了你要管理的部分：每台 Node 的程序、登录、项目目录、设备授权与备份。Vibeface 不自带模型，不替你登录 Agent，也不提供免费模型额度。本机 Agent 调用外部模型时，仍受该服务的费用和数据处理政策约束。

## 比较依据与范围

以下官方文档于 2026-10-31 核对，产品能力会继续变化：

- [Codex CLI](https://developers.openai.com/codex/cli/) 说明终端编码、本地仓库和脚本工作流。
- [Claude Code overview](https://code.claude.com/docs/en/overview) 说明编码能力及终端、IDE、桌面和浏览器入口。
- [OpenClaw 文档](https://docs.openclaw.ai/) 说明自托管 Gateway、多渠道、Agent 路由和自动化。

Vibeface 的说明依据当前公开实现，不代表全部 Agent 产品、模型与设备组合都经过实测。执行能力和权限应结合自己的配置核验，详见 [隐私与安全](privacy-security.md)。代码可在 [Vibeface 仓库](https://github.com/leaperone/vibeface) 查看，使用与分发授权以仓库当前声明为准。

先了解完整工作方式可阅读 [产品概览](overview.md)，决定使用后从 [安装](install.md) 开始。
