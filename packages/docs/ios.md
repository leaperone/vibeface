---
title: iOS 构建与分发
description: 构建 Vibeface iOS 设备归档和签名 IPA，先通过 TestFlight 内测，再准备 App Store 发布。
---

# iOS 构建与分发

iOS 先通过 TestFlight 内测，验证后再上 App Store。当前工作流只构建和校验，
不上传 App Store Connect，也不提交商店审核。

`release-ios.yml` 是可复用工作流，由产品 `release.yml` 或手动 `ci-mobile-device.yml` 的 `platforms=ios`、`both` 入口调用。
手动设备构建默认不发布。`mobile.yml` 的 iOS 模拟器 UI 验收使用另一条路径。
它从同一份 React Native App 与 Rust secure-native XCFramework
构建 `arm64` iPhoneOS 设备归档，核对真正的设备架构、源码提交、bundle ID、
发行版本、App 版本和构建号。模拟器 App 不能替代设备归档。

未配置签名 secrets 时，工作流只生成绑定源码的
`vibeface-<releaseVersion>-ios-device-unsigned.xcarchive.zip`、SHA256 和来源收据。
这个归档未签名，只用于开发检查，无法安装到 iPhone，也不是 IPA。

签名导出需要完整配置以下仓库 secrets：

- `IOS_CERTIFICATE_P12_BASE64`，含私钥的 Apple Distribution 证书。
- `IOS_CERTIFICATE_PASSWORD`，证书的导出密码。
- `IOS_PROVISIONING_PROFILE_BASE64`，适用于该 App 和分发方式的描述文件。

配置完整后，工作流校验描述文件的精确 bundle ID、team、有效期、分发权限和
请求的分发方式，再签名归档并导出 `vibeface-<releaseVersion>-ios.ipa`。
输出包含经签名与描述文件校验的 SHA256 和来源收据。只配置部分 secrets 时构建失败。

Hourly 协调器在 Tag 提交中把 iOS `buildNumber` 设为 `YYMM.DD.HH`（UTC），main 上的值不变。
构建号按 Apple 的 CFBundleVersion 分段规则验证；末段超出上限时停止，不重置或自动进位。
Mobile 的 marketing version 仍保持 `0.0.1`。

`distribution_method` 默认是 `app-store-connect`。对应收据中的
`testflight_eligible=true` 只表示导出方式和描述文件校验通过，
不代表 App Store Connect 已接收或测试者已经可以安装。内测安装还需要单独授权上传、
等待 Apple 处理，并分配到 TestFlight 内部测试组。

`ad-hoc` 仅用于描述文件中已注册的设备。只有这种 IPA 的收据标记
`installable=true`，且安装资格限于已注册设备。

当前仓库尚未配置这些 iOS 签名材料。已有的 macOS Developer ID 证书与公证 API key
不能替代 iOS 分发签名。缺少签名材料时不生成 IPA，上传 TestFlight 或提交
App Store 审核须另行授权。
