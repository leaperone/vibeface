# 本机 iOS 命令行构建

本机构建优先使用独立 Aqua LaunchAgent，在用户已登录的桌面安全会话中运行命令。已验证这条路径可以完成 Expo prebuild、CocoaPods、`xcodebuild` Release 和开发签名，无需操作 Xcode 窗口。它不是无需桌面登录的 CI 方案，也不替换 [现有分发归档脚本](../../../../scripts/build-ios.sh)。

## 准备源码与签名

- 固定本次目标源码 SHA、`expo.extra.releaseVersion`、marketing version、build number 和目标设备 UDID。先核对已有任务和制品，不复用旧任务的版本、路径或 label。
- 使用任务专属 worktree，依赖、原生工程和构建产物留在其忽略目录中，证据写入本任务 planning。外置盘 worktree 被系统拒绝访问时，改用内置盘上的任务 worktree；不扩大整机磁盘权限。worktree 的 Git 元数据也可能指向外置盘，控制端须核验完整 SHA 与 clean tree，并对构建输入生成摘要，在任务开始和结束时校验，不能只信一份手写 SHA。
- 确认现有开发证书有对应私钥，provisioning profile 的团队、bundle ID、有效期和目标设备均匹配。先在实际构建会话签一个任务自有的最小 Mach-O，再用 `codesign --verify --strict` 核验，并确认实际签名身份。只看到 `find-identity` 输出不算签名通过。
- `errSecInternalComponent` 是笼统错误。用 `launchctl managername`、`SessionGetInfo` 和任务日志区分 Background 与 Aqua 会话。桌面终端解锁成功不代表另一个后台会话已解锁；钥匙串界面的“允许所有应用访问”也不能排除 partition list 或会话限制。不要据此反复要求用户解锁、导出私钥或修改共享钥匙串权限。

## 创建独立命令任务

为本任务生成唯一 label，将 plist 放在用户的 `Library/LaunchAgents` 中，使用绝对路径。已验证的配置如下，路径和 label 按本次任务生成：

| 键 | 值 |
| --- | --- |
| `Label` | 本任务唯一名称 |
| `ProgramArguments` | `/bin/bash` 与任务构建脚本的绝对路径 |
| `LimitLoadToSessionType` | `Aqua` |
| `SessionCreate` | `false`，使用现有桌面会话 |
| `RunAtLoad` | `false`，仅按命令触发 |
| `StandardOutPath`、`StandardErrorPath` | 已存在、可写且该任务可访问的日志目录中的绝对路径 |

不要配置 `KeepAlive` 或修改当前 Codex daemon。`SessionCreate=true` 会创建新安全会话，不等于加入现有桌面会话。日志目录或外置盘访问失败也可能表现为 bootstrap 错误或 `EX_CONFIG`，须检查真实原因。

控制端核对用户 UID 和 plist 后执行，以下变量都应已绑定本任务实际值：

```bash
launchctl bootstrap "gui/$USER_UID" "$PLIST_PATH"
launchctl kickstart "gui/$USER_UID/$LABEL"
launchctl print "gui/$USER_UID/$LABEL"
```

不对正在运行的同名任务使用 `kickstart -k`。复用已有 label 前核对它的脚本、路径和任务归属；配置更新须等该任务退出，再只卸载并重新加载该任务。临时探针完成后移除其任务配置，不触碰其他用户服务。

## 构建脚本

脚本使用 `set -euo pipefail`，固定工作目录，显式提供 Node、pnpm、CocoaPods 的 PATH 和 UTF-8 locale。本地打包使用 `CI=1 EXPO_OFFLINE=1 EXPO_NO_TELEMETRY=1`；依赖下载另行完成，避免把 Expo 的外部连接等待当成签名失败。

1. 校验目标源码和输入摘要，为本次运行生成唯一标识并清除旧成功回执。
2. 在仓库根执行 `pnpm install --frozen-lockfile`，并用 [原生加密模块构建脚本](../../../../scripts/build-mobile-secure.sh)准备 iOS XCFramework。只有来源 SHA、输入和文件摘要完全匹配的已验证产物才可复用。
3. 在 `apps/mobile` 执行 `pnpm exec expo prebuild --platform ios --no-install`，再在生成的 `ios` 目录执行 `pod install`。复用原生工程前先核对已有本地签名配置，避免覆盖无关改动。
4. 使用 `xcodebuild`、`Vibeface.xcworkspace`、`Vibeface` scheme、`Release` 配置、`iphoneos` SDK 和 `generic/platform=iOS` 构建。显式设置任务内的 `-derivedDataPath`、已核对的 `DEVELOPMENT_TEAM`、Automatic 开发签名和 `Apple Development` 身份；不要照搬 CI 的禁用签名参数。仅构建时使用 `build`，不让运行命令隐式安装手机。
5. 对最终 App 执行 `codesign --verify --deep --strict`；检查实际签名身份、entitlements、embedded profile、版本和非空 `main.jsbundle`，用 [设备程序校验](../../../../scripts/ios_device.py)确认每个 Mach-O slice 均为 iPhoneOS 且包含 arm64。再校验输入摘要，全部通过后才写成功回执。

`kickstart` 返回只表示触发请求完成。等待本次任务退出，核对该次运行的 exit code、日志及新成功回执，记录源码 SHA、版本、build number、产物路径和校验结果。旧日志中的成功记录、上一轮的 exit code 或仅有 `BUILD SUCCEEDED` 都不足以证明本次签名和产物检查通过。

## 真机安装

只在用户已授权安装时，使用精确 UDID 执行 `devicectl device install app` 原位安装，再独立读回版本、启动 App 并查询运行进程。保留安装和启动回执，不卸载、不清数据或钥匙串。安装成功、进程启动、界面验收和数据完整性分别报告，文件元数据相同不能代替完整数据校验。本机开发签名产物不代表公开 Release、TestFlight 或 App Store 已交付。

原理与平台说明见 [Apple TN2083](https://developer.apple.com/library/archive/technotes/tn2083/_index.html) 和 [GitLab macOS Runner](https://docs.gitlab.com/runner/install/osx/)。真正无人值守的 CI 专用钥匙串方案见 [GitHub 签名文档](https://docs.github.com/en/actions/how-tos/deploy/deploy-to-third-party-platforms/sign-xcode-applications)，不得直接在共享开发 Mac 上照搬覆盖钥匙串搜索列表的命令。
