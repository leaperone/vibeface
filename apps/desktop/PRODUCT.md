# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Electron 桌面宿主，复用现有 React 业务界面；本机 Core 与 CLI 使用 Rust。技术实现由 Agent 根据用户指定的 Grok Bot 参考和现有代码选择。系统窗口、菜单、托盘、通知和文件交互使用桌面平台能力。

## Users

用户管理自己的电脑和个人设备网络，通过对话委派工作，并在长任务期间补充要求、处理提问和查看结果。不设计组织共享设备池。

## Product Purpose

本轮要求提供可安装运行的 macOS、Windows 桌面程序，并补齐原生 Windows Core 与 CLI。桌面入口、业务会话与本机执行使用同一套 Core/客户端协议。

## Operating Context

用户可能已经有常驻 Core 服务，也可能首次在这台电脑运行 Vibeface。桌面端需要区分已有服务与自己启动的进程；关闭窗口、退出程序、睡眠、断线和恢复时保留工作记录，并按真实状态反馈。已有浏览器身份不自动复制到桌面存储。

## Capabilities and Constraints

本轮要求窗口、单实例、菜单/托盘、连接与离线引导、受限系统交互、本机运行管理和可验证的安装包。Windows 的 Core/CLI 要实现本机执行、私有控制通道、用户级服务和更新能力。

数据 epoch 和更新协议保持 1，正式版前版本基准保持 0.0.1 Nightly。桌面和 Windows 制品接入现有统一产品构建，不另建发布渠道。已有运行实例的数据和服务不被开发或验收接管。未知结果先核对，不因重连或重启重发副作用。业务页面不获得通用命令执行或任意文件读写权限。

## Brand Commitments

沿用 Vibeface 已有名称、图标、界面语气和视觉体系。用户指定 Grok Bot 为桌面方案参考。本轮扩展桌面宿主、启动与设置状态，不替换业务聊天的信息结构，不新增项目文件管理入口。

## Evidence on Hand

现有 `apps/app`、`packages/client`、Rust Core/CLI，以及 macOS 已有的安装与服务机制。基线源码为 5eb58c9660b44e4a2b0e9b52fd6576bf80987ad5。本机 Grok Bot 0.79.2 安装包确认包含 Electron Framework 与 app.asar；没有读取其用户会话或凭证。

## Product Principles

- 业务状态和执行由 Core 管理，桌面层集中处理窗口与系统交互。
- 同一个运行实例只有一个进程管理者和更新管理者。
- 凭证与私钥按本机用户隔离，不进入共享业务记录或日志。
- 显示实际接收、连接、执行和完成状态，保留未确认结果。

## Accessibility & Inclusion

继承现有产品的键盘操作、可读文字和深浅主题，遵守系统减少动态效果设置。平台窗口及常用快捷键遵循 macOS 和 Windows 的习惯。
