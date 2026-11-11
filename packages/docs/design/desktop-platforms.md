# 桌面平台与本机运行

macOS、Windows 和 Linux 使用 GPUI 原生桌面客户端，直接通过 Rust 客户端连接个人网络。iOS 和 Android 使用 React Native/Expo。Web 保持独立浏览器客户端。详细模块、身份和功能验收契约见 [GPUI 桌面设计](gpui-desktop.md)。

## 模块与调用关系

```text
GPUI Desktop → crates/client → 加密连接本机或远端 Node
GPUI Desktop → 本机 host → 经过验证的 CLI/Core 控制
Web / Mobile → packages/client → 加密连接 Node
Node Core → 持久记录、同步、调度 → 本机 Provider
CLI → 本机控制、服务注册、安装与更新
```

Desktop 的窗口、输入、时间线和业务设置由原生视图实现。客户端 actor 持有连接和写入顺序，主线程只消费快照与发送用户意图。扫描、选择和纯控制端启动都不初始化 Node。新原生身份通过已有管理设备邀请或恢复加入，不能从旧浏览器 profile 导出不可导出的设备私钥。

## 运行与更新所有权

选中的 Node、App 自己启动的 Child 和独立用户后台服务分别记录。附着已有服务不取得启停权，不按进程名称或未验证的 PID 杀进程。

明确启动时使用内置 Core 的固定路径、独立数据目录和私有父进程 stdin。明确退出时关闭自有 Child 的 stdin 并等待正常结束；升级终止只使用持有的 Child 句柄。父进程崩溃的 EOF 同样触发正常关闭，外部服务继续运行。

安装后台节点会把完整运行包复制到持久受管目录，再注册当前用户的 launchd、systemd 或 Windows 计划任务。交接前必须等自有 Child 完全退出。后台运行不依赖 AppImage 挂载路径。移除服务保留节点数据，独立 CLI 更新不改写签名 App。

## 平台交付

| 平台 | App | 本机交付 |
| --- | --- | --- |
| macOS ARM64 | GPUI | DMG、ZIP、封存 Core/CLI 运行包 |
| Windows x64 | GPUI | 当前用户 NSIS 安装器、ZIP、封存 Core/CLI 运行包 |
| Linux x64、ARM64 | GPUI | AppImage、ZIP、封存 Core/CLI 运行包 |
| Android | React Native | APK，连接个人网络 |
| iOS、iPadOS | React Native | 设备归档和配置签名后的 IPA |

Windows 保留当前 SID 权限、私有命名管道、Unicode 路径、数据锁、进程和用户级服务检查。Codex App Server 的 `unix://PATH` 使用 Windows AF_UNIX，与维护命名管道分开。安装和运行不依赖 WSL、Git Bash、管理员或开发者模式。

每个平台独立构建和校验。打包器复制同源、同版本运行包并在签名后复核其 `FILES.sha256`。来源收据记录 GPUI 版本和运行包归档摘要。macOS Developer ID/公证与 Windows Authenticode 状态分别核实；未签名开发预览不能视为正式签名发行。

真实窗口验收使用原生程序、独立 profile 和受控 Provider。图形启动、功能矩阵、IME、无障碍、系统通知、服务更新和性能分别提供运行证据。CI 成功、跨目标编译或依赖接口存在不能代替这些验收。
