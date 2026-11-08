<div align="center">

# Vibeface

**在聊天里提需求，用自己的电脑执行，在同一段对话里查看结果。**

[English](README.md) · [简体中文](https://github.com/leaperone/vibeface/blob/main/README.zh-CN.md)

[![CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml/badge.svg)](https://github.com/leaperone/vibeface/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/leaperone/vibeface?include_prereleases&label=release)](https://github.com/leaperone/vibeface/releases)
[![Platforms](https://img.shields.io/badge/platform-macOS%20arm64%20%7C%20Linux%20x64%20%7C%20Linux%20arm64-lightgrey)](packages/docs/install.md)

</div>

> [!NOTE]
> Vibeface 目前处于 Nightly 阶段。正式版发布前，协议、数据格式和 API 调整不保留向后兼容。同一网络里的电脑需要使用同一版本。

## 目录

- [可以做什么](#可以做什么)
- [快速开始](#快速开始)
- [添加另一台设备](#添加另一台设备)
- [公共发现服务](#公共发现服务)
- [隐私与权限](#隐私与权限)
- [使用文档](#使用文档)
- [从源码构建](#从源码构建)
- [仓库结构](#仓库结构)
- [参与贡献](#参与贡献)
- [许可证](#许可证)

## 可以做什么

- **在聊天里提需求。** 给 Bot 设置名称和职责，在同一段对话里安排工作、查看结果。
- **选择执行的电脑。** 使用自己 Mac 或 Linux 上的 Codex、Claude Code、pi、opencode 或 Cursor 完成任务。
- **换台电脑继续。** 已授权的电脑会同步聊天、任务进展和保存的结果。在另一台已同步的电脑打开对话，选择由它继续处理。
- **用手机查看。** 通过浏览器或原生 iOS、Android 应用连接自己的设备。

Vibeface 负责协调工作。每台电脑上的 Agent、登录、模型服务和项目目录由你准备。聊天同步不会自动复制项目文件、依赖或登录状态。

## 快速开始

准备一台 Apple Silicon Mac、Linux x64 或 Linux ARM64（glibc）电脑，并在上面配置一个支持的 Agent。

1. 安装 Nightly。安装器自动选择并校验本机平台的完整包：

   ```sh
   curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash -s -- --channel nightly --install-root "$HOME/.local/share/vibeface" --bin-dir "$HOME/.local/bin"
   export PATH="$HOME/.local/bin:$PATH"
   vibeface --version
   ```

2. 启动 Node，也就是连接这台电脑的本机服务：

   ```sh
   vibeface
   ```

3. 在这台电脑的浏览器打开 `http://localhost:4790`。
4. 创建个人网络，下载恢复文件，并妥善保存恢复密码。
5. 在设备的 AI 设置中选择 Agent，完成登录并检查可用性。平时在终端里的登录不一定适用于 Vibeface 的私有目录。
6. 创建 Bot 并发送消息。需要操作项目时，在 AI 设置中启用任务执行，添加项目目录。

数据目录默认为 `~/.vibeface`，首次运行只初始化不存在或为空的目录。完整步骤见[快速上手](packages/docs/quick-start.md)。

将 `~/.local/bin` 加入 shell 配置，便于在新终端使用 `vibeface`。已有安装通过 `vibeface update` 更新，通过 `vibeface service` 管理后台服务，不重复运行安装器。更新配置和其他安装方式见[安装与更新](packages/docs/install.md)。

已有 Node.js 22 或更新版本时，也可用 `npm install -g @leaperone/vibeface@nightly` 安装 CLI；这种安装由 npm 管理更新。

## 添加另一台设备

在管理设备的**添加设备**中生成加入链接，用另一个浏览器或手机 App 打开。添加另一台电脑时运行：

```sh
vibeface join '<加入链接>' --name '<设备名称>'
```

在两台设备上核对 6 位安全码后再批准。请加入已有网络，不要重新建网。加入和恢复步骤见[首次配置](packages/docs/onboarding.md)。

## 公共发现服务

官方公共节点是一台不加入任何个人网络的普通 Node，提供发现服务、开放 Relay 和浏览器接入，各有很小的上限。它的设备编号和 UDP 地址固定在 [`config/public-bootstrap.json`](config/public-bootstrap.json)：

```text
vibeface://<issuer>@8.217.72.118:4792
```

新建的个人 Node 默认向它登记，直连不通时经它的开放 Relay 连通。`https://app.vibeface.dev` 经它按设备编号连到你的 Node，局域网直连仍优先。它只转发密文。首次初始化时可用 `--bootstrap off` 关闭公共引导，也可以指向自建服务。配置方法、可见元数据和连接限制见[使用公共发现服务](packages/docs/node.md#使用公共发现服务)。加入个人网络仍需邀请和批准。

## 隐私与权限

- 新设备加入私有网络需要你批准。只添加你信任的设备。
- 连接经过加密。公共节点只转发加密流量，不会因此成为网络成员。
- 配置中的凭证留在本机。发送到聊天里的秘密会成为共享对话的一部分。
- 已授权的电脑和你选择的模型服务会处理任务内容。连接加密不等于磁盘文件加密。
- Agent 能运行命令、修改文件。项目目录设置不是操作系统沙箱。
- 任务结果未知时，先核对原任务再决定是否重发。同步历史不会重新启动旧任务。

启用项目访问前，请阅读[隐私与安全](packages/docs/privacy-security.md)。

## 使用文档

详细指南目前以中文提供。

| 文档 | 用途 |
| --- | --- |
| [快速上手](packages/docs/quick-start.md) | 安装并完成第一次对话。 |
| [日常使用](packages/docs/user-guide.md) | 管理 Bot、任务、通知和电脑。 |
| [产品概览](packages/docs/overview.md) | 了解用途和功能边界。 |
| [Bot 与任务执行](packages/docs/bot-and-execution.md) | 配置协调和执行任务的 Agent。 |
| [历史同步](packages/docs/history-sync.md) | 查看同步范围，以及如何换电脑继续。 |
| [安装与更新](packages/docs/install.md) | 管理安装包、后台服务和更新。 |
| [排障](packages/docs/troubleshooting.md) | 诊断连接和任务问题。 |
| [Node 参考](packages/docs/node.md) | 配置连接、Agent 和恢复。 |
| [CI](packages/docs/ci.md) | 查找改动需要的检查。 |

[文件交付](packages/docs/paid-delivery.md)是实验功能，默认关闭。付费交付只使用本机测试链，不处理真实资产。

## 从源码构建

需要 Rust 1.91+、Node.js 22.12+、pnpm 11.9.0 和 `wasm-bindgen-cli` 0.2.129。

```sh
rustup target add wasm32-unknown-unknown
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
cargo build --release -p vibeface -p vibeface-core --locked
bash scripts/package-node.sh
```

平台安装包输出到 `.artifacts/packages/`。运行方法见[源码配置](packages/docs/quick-start.md#从源码准备当前版本)，打包细节见[发布说明](packages/docs/releasing.md)。

## 仓库结构

```text
.
├── apps/
│   ├── app/         # 网页应用
│   ├── cli/         # 命令行
│   ├── core/        # Node 本机服务
│   ├── mobile/      # iOS 和 Android 应用
│   └── www/         # 公共网站
├── crates/          # 共享 Rust 库
├── packages/
│   ├── client/      # 共享应用客户端
│   ├── docs/        # 用户和开发者指南
│   └── theme/       # 共享样式
├── contracts/       # 实验性文件交付合约
├── scripts/         # 构建、打包和验证工具
├── tests/           # 测试夹具和端到端测试
└── install.sh       # 安装脚本
```

## 参与贡献

在 [Issues](https://github.com/leaperone/vibeface/issues) 中附上平台、`vibeface --version` 和相关的 `vibeface status` 输出。分享日志前请移除凭证和个人数据。

提交 PR 时从 `main` 创建独立分支，运行与改动相关的检查。所需检查见 [CI](packages/docs/ci.md)。

## 许可证

仓库目前没有 `LICENSE` 文件。在协议发布前，代码默认保留所有权利。
