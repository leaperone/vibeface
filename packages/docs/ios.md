---
title: iOS 构建与分发
description: 构建 Vibeface iOS 设备归档和签名 IPA，先通过 TestFlight 内测，再准备 App Store 发布。
---

# iOS 构建与分发

iOS 先通过 TestFlight 内测，验证后再上 App Store。当前工作流只构建和校验，不上传 App Store Connect，也不提交商店审核。公开 Mobile Release 因此不代表已有可安装的 iOS 渠道。

`release-ios.yml` 是可复用工作流，由 `release-mobile.yml` 和
`verify-mobile-ui.yml` 调用。后者不传签名 secrets，只校验 unsigned 设备归档，
不创建发布，也不代替模拟器或原生 UI 验收。
它从同一份 React Native App 与 Rust secure-native XCFramework
构建 `arm64` iPhoneOS 设备归档，核对真正的设备架构、源码提交、bundle ID、
发行版本、App 版本和构建号，同时核对内嵌 Share Extension 的身份、版本和设备架构。
系统分享入口显示为 `Vibeface`，扩展 target 与文件名保持 `expo-sharing-extension`。
模拟器 App 不能替代设备归档。

未配置签名 secrets 时，工作流只生成绑定源码的
`vibeface-<releaseVersion>-ios-device-unsigned.xcarchive.zip`、SHA256 和来源收据。
这个归档未签名，只用于开发检查，无法安装到 iPhone，也不是 IPA。

签名导出需要完整配置以下仓库 secrets：

- `IOS_CERTIFICATE_P12_BASE64`，含私钥的 Apple Distribution 证书。
- `IOS_CERTIFICATE_PASSWORD`，证书的导出密码。
- `IOS_PROVISIONING_PROFILE_BASE64`，精确对应 `one.leaper.vibeface` 的分发描述文件。
- `IOS_SHARE_PROVISIONING_PROFILE_BASE64`，精确对应 `one.leaper.vibeface.share` 的独立分发描述文件。

两个描述文件必须属于同一 team、使用不同 UUID、允许同一个 Apple Distribution
证书，并包含 App Group `group.one.leaper.vibeface`。工作流校验有效期、分发权限和
请求的分发方式，分别给两个 target 指定描述文件，再导出 `vibeface-<releaseVersion>-ios.ipa`。
临时钥匙串与两个已安装描述文件在退出时清理或恢复原文件，unsigned 构建不访问钥匙串。
输出包含经两个 bundle 的签名、证书和 App Group 权限校验的 SHA256 与来源收据。
unsigned 收据只证明 bundle 元数据和设备架构，不证明签名权限。只配置部分 secrets 时构建失败。

Hourly 协调器在 Tag 提交中把 iOS `buildNumber` 设为 `YYMM.DD.HH`（UTC），main 上的值不变。
构建号按 Apple 的 CFBundleVersion 分段规则验证；末段超出上限时停止，不重置或自动进位。
Mobile 的 marketing version 仍保持 `0.0.1`。

`distribution_method` 默认是 `app-store-connect`。对应收据中的
`testflight_eligible=true` 只表示导出方式和描述文件校验通过，
不代表 App Store Connect 已接收或测试者已经可以安装。内测安装还需要单独授权上传、
等待 Apple 处理，并分配到 TestFlight 内部测试组。

`ad-hoc` 仅用于主 App 描述文件中已注册的设备，扩展描述文件必须覆盖这些设备。
扩展额外注册的设备不会扩大主 App 的安装范围。只有这种 IPA 的收据标记
`installable=true`，且安装资格限于已注册设备。

运行时以传入的完整签名 secrets 为准。已有的 macOS Developer ID 证书与公证 API key
不能替代 iOS 分发签名。缺少签名材料时不生成 IPA，上传 TestFlight 或提交
App Store 审核须另行授权。

正式分发还需要 Apple Distribution 证书、描述文件、App Store Connect 上传凭证和测试组配置。上传、Apple 处理和测试组分配完成后，才能把对应 Mobile Release 标记为可安装。
