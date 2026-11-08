# 五个平台的 App 与桌面本机运行

Windows、macOS 和 Linux 使用 Electron 桌面宿主，复用 React 业务界面，并内置各自平台的 Rust Core 与 CLI。iOS 和 Android 使用现有 React Native/Expo 原生 App，共享客户端与 Rust 密码学模块。手机通过自己的设备身份连接个人网络中的 Node。用户指定 Grok Bot 为参考；本机 0.79.2 安装包包含 Electron Framework 与 app.asar。Electron 固定为本轮核对过的稳定版本 45.2.3。本轮目标是实现、验证并提交一个 PR，合并与发布另行按授权执行。

## 模块与调用关系

```text
macOS / Windows / Linux
  Electron 主进程 → 启动器与本机节点管理 → 捆绑的 Rust CLI / Core
  Electron 业务窗口 → 内置 React 客户端 → 加密连接本机或远端 Node

iOS / Android
  React Native App → 共享客户端与密码学模块 → 加密连接远端 Node

Node Core → 持久记录、同步、调度 → Provider / SDK → 所在电脑执行
CLI → 本机控制、服务注册、安装与更新
```

桌面负责窗口、单实例、菜单/托盘、系统交互以及启动、离线和设置状态。Core 继续负责持久化、同步、调度与 Provider。业务窗口开启 sandbox、contextIsolation，关闭 nodeIntegration，不向页面暴露任意进程、文件或管理者能力。

启动器与业务窗口分开。启动器只能通过校验过的窄 IPC 操作本机运行。业务页面来自安装包内经过清单校验的 Web 文件，固定使用 `vibeface-app://app/` 来源；即使没有本机 Node，也能打开客户端并使用邀请链接。原有非导出设备私钥保存在这个固定来源的 IndexedDB 内，不复制既有浏览器私钥。

业务页通过专用 preload 使用本机接入和 LAN 传输，不获得通用命令、文件或 HTTP 能力。本机接入由主进程重新检查所选节点的目录、设备身份和私有 loopback 来源；长期启动能力和 HttpOnly cookie 保留在主进程。首次注册继续使用已有身份确认、恢复文件下载和保存确认流程。普通远端入口继续使用现有 WebSocket 与 Noise；LAN 使用捆绑 Core 的客户端桥接模式和原有 Iroh QUIC，客户端模式不初始化节点。

可独立打开的客户端与可选节点分别建模。每次启动只读检查当前用户的默认目录、已记住的目录、App 节点目录、已注册用户服务和运行中 Core。只有一个可用节点时自动选择并打开客户端；多个节点显示选择，记住的选择须再次验证。选择或扫描不初始化、不启动、不修改已有节点。已停止节点需要明确点击启动。客户端选择与 App 自有进程状态分别显示，避免将“可访问”误报为业务会话已经认证。

## 运行与更新所有权

主进程分别记录客户端选中的 Node、App 自己启动的 Core 子进程和可验证的用户后台服务。连接已有服务时不取得启停所有权，不因为端口或数据目录占用而杀进程、清空数据或另起重复实例。

App 自己启动的 Core 使用内置固定路径和显式数据目录，保持私有父进程管道。关闭窗口保留托盘和运行；明确退出时只停止自己的 Core。父进程异常退出也须使自有 Core 正常结束，重启或重新加入网络后仍保留这项生命周期约束。独立服务不启用这个父进程退出条件。

用户选择安装后台节点时，App 只准备自己配置目录下的节点，并将完整运行包复制到持久的受管目录，再注册当前用户的 launchd、systemd 或计划任务。若同一节点正由 App 子进程运行，须先等该子进程完全退出再交给服务。后台节点能在退出 App 或 Linux AppImage 挂载路径消失后继续运行；移除服务保留节点数据。原有外部节点不能被静默收编。

签名 App 中的 CLI、Core 和 Web 内容来自同一源码的完整运行包，并保持校验清单一致。CLI 更新器管理独立受管安装，不能改写签名 App 内的程序。桌面更新由 App 的安装或更新流程负责，同一安装只有一个更新管理者。

## Windows 范围

Windows 移植包含当前用户隔离的私有 IPC、文件权限、数据锁、进程树、命令/路径、用户级服务以及更新与恢复。正常功能不能用空实现或整块 cfg 排除替代。持久记录中的进程标识不能变成不核实归属就结束新 PID 的操作。

桌面验收矩阵为 macOS ARM64、Windows x64、Linux x64 和 Linux ARM64。Windows 使用 `x86_64-pc-windows-msvc` 原生目标和 `.exe` 程序。独立 Core/CLI 继续使用完整 `.tar.gz` 包格式，Windows 安装和运行不依赖 WSL、Git Bash、管理员权限或开发者模式。已有 Linux 目标保留。

网络或执行结果未知时保留原工作身份并核对，不能重放写操作。现有仅在 Linux 提供的证明生成能力维持其原有平台约束，不以此关闭 Windows 的其他 Core 能力。

## 构建与验收

桌面和 Windows 制品纳入统一产品版本与并行构建，官网独立。数据 epoch、更新协议保持 1，正式版前保持 0.0.1 Nightly。每个平台只等待自身必要输入，完整汇总后由现有唯一发布者交付。

实现按 Windows 运行时、桌面宿主、构建与交付三个边界分工，主 Agent 负责接口、集成和最终验收。开发分支使用已注册的 CI 手动入口运行真实 Windows/MSVC 与 macOS 构建；本机跨目标类型检查不代表 Windows 编译、链接或运行已通过。

验收覆盖当前用户权限与私有通道、已有/自有进程的启停与异常恢复、服务及更新、实际桌面窗口、包内来源与摘要。Provider 使用受控夹具。开发预览与实际发布的签名状态分别记录，不把无签名预览、普通浏览器页面或 pending CI 报告成原生正式交付。


Windows 的维护通道与 Codex App Server 使用不同接口。维护通道是带当前 SID DACL 的命名管道；Codex 使用官方 `unix://PATH`，底层为 Windows AF_UNIX。连接前核对私有父目录和内核报告的对端进程账户。App Server 的路径指针使用 `.endpoint`，不把指针文件当作 socket，也不向上游传入未定义的命名管道 URL。


## 面向用户的安装形态

| 平台 | App | 本机执行与交付 |
|---|---|---|
| Windows x64 | Electron 桌面 App | 当前用户安装器与 ZIP，内置原生 Core、CLI 和 Web |
| macOS Apple Silicon | Electron 桌面 App | DMG 与 ZIP，内置原生 Core、CLI 和 Web |
| Linux x64、ARM64 | Electron 桌面 App | AppImage 与 ZIP，内置对应架构的 Core、CLI 和 Web |
| Android | React Native App | APK，连接个人网络中的 Node |
| iOS、iPadOS | React Native App | 设备归档及配置签名后的 IPA，安装渠道由 Apple 分发方式决定 |

没有已有节点时，桌面首次打开可选择仅作控制端，或在这台电脑启用节点；只有明确启动或安装后台节点时才初始化 App 的 Node。已有节点会自动发现，多个节点由用户选择。iOS 和 Android 只提供控制与访问入口，不安装或运行 Node。

五个平台共享业务和身份规则，不要求它们使用同一套窗口技术。iOS 按用户指定先走 TestFlight，再准备 App Store。未签名设备归档用于证明原生代码能构建，不能安装到 iPhone；可安装 IPA 必须有适用于该 App 的 iOS 证书和 provisioning profile。
