---
name: release
description: 独立发布 Vibeface Core、App、CLI/npm、Desktop、Mobile 或 WWW，构建并签名本机 iOS 版本，核对不可变制品和分发结果。
---

# Vibeface release

只执行会话已授权的发布范围。合并、构建通过或修改本 skill 不构成新的发布授权；已有自动 Hourly 与服务自动上线授权继续适用。人工发布确定应用、渠道和精确版本，Stable 的 patch/minor/major 不自行猜测。Hourly 交给自动协调器，不手工改时间造版本。

先读[发布说明](../../../packages/docs/releasing.md)、[发布工具](../../../scripts/release.py)、[模块边界](../../../packages/docs/design/release-modules.md)和对应版本文件。首个正式版前基准保持 0.0.1，main 的开发版本固定；不保留旧 Product updater 的兼容路径。

| 应用 | Tag | 入口 | 依赖与结果 |
| --- | --- | --- | --- |
| core | `core-v<version>` | `release-core.yml` | 四个 Core 原生归档；自动更新公共 Node |
| app | `app-v<version>` | `release-app.yml` | 静态 App；自动更新 App 服务 |
| cli | `cli-v<version>` | `release.yml` | 四个完整运行包与 npm；锁定 Core/App |
| desktop | `desktop-v<version>` | `release-desktop-app.yml` | 四个平台桌面制品；锁定 CLI 运行包 |
| mobile | `mobile-v<version>` | `release-mobile.yml` | Android 与 iOS 独立生产者；不内置 App/Core |
| www | `www-v<version>` | `release-www.yml` | 静态官网；自动更新 WWW |

CLI/npm 共用版本。版本语法为 `X.Y.Z`、`X.Y.Z-rc.N`、`X.Y.Z-hourly.YYYYMMDDHH`。没有全局 `v<version>` 标签，不创建可变标签、别名、build metadata 或重复版本。

## 自动 Hourly

用户已授权自动 Hourly 和官方服务上线。`hourly.yml` 在 main push CI 成功后、每小时 cron 与上游发布唤醒时运行。协调器选已通过同 SHA main push CI 的源码，仅为构建输入或依赖 pin 变化的应用生成 Tag。Core/App/Mobile/WWW 独立；CLI 等 Core/App，Desktop 等 CLI。一个应用失败只阻止自己的消费者。

生成提交只有一个 main parent，只改所选版本、必要的 Cargo.lock 本地包或 Mobile build number，以及 `releases/cli.json`/`releases/desktop.json` 的精确依赖 pin。`scripts/hourly.py` 会重建 Git tree 校验来源，不改变 main。每个应用每 UTC 小时最多一个 Tag；已有 Tag 只能对账和补派发，不覆盖。

## 人工版本与发布

在一个独立分支/PR 准备所选应用版本、锁文件条目与精确依赖 pin。其他应用不用一起递增。依赖 pin 含 tag、version、source_commit、manifest_sha256；Stable 只消费 Stable 依赖。人工 Stable/RC 来源必须已在 main，且同 SHA main push CI 通过。具体命令如下，变量取已授权的实际值。

```sh
python3 -B scripts/release.py plan --app "$APP" --tag "$TAG" --require-main
git ls-remote --tags origin "refs/tags/$TAG" "refs/tags/$TAG^{}"
```

核对干净源码、版本、main CI 与远端同名 Tag。只在本地和远端都不存在该 Tag 时创建精确不可变 Tag，再只推送这一 Tag。已有同名 Tag 先解析到完整 SHA，不同源码立即停止，禁止移动、删除重建、强推或更改保护规则。

真实发布从精确 Tag ref 派发所选入口，`tag` 相同、`publish=true`。`publish=false` 只构建；CLI/Desktop 构建源码仍须携带完整依赖 pin。入口的身份检查、签名门和 source/CI 门不能跳过。

## 对账与验收

每个应用有自己的九字段 `release.json`，只有本应用全部资产验证成功才最后发布。组件的 `COMPONENT-INFO.json`、CLI 的 `BUILD-INFO.json`、Desktop/Mobile 来源收据和全部文件摘要必须匹配。CLI 包的字段和身份匹配规则见 [CLI 包的构建信息](../../../packages/docs/releasing.md#cli-包的构建信息)。

已公开字节不覆盖。失败优先重跑原 run 的失败任务，复用原 artifact ID；新源码使用新版本。写入响应未知时查询 Tag、Release、资产摘要、Actions 或 npm registry 后再决定，不能直接重复写入。公开附件不等于完成，缺少清单时任何自动消费者都必须拒绝。

跟踪本次 Tag/source 对应的全部运行到终态，不能用“最新一条”猜测，不把 pending 当成功。分别报告构建、公开 Release、npm、服务部署和移动端分发。Node 需读回版本、身份和健康；App/WWW 需读回页面/版本头。发布父 workflow 使用 `release-dispatch.py` 等待真实子部署结果。

npm 通过 OIDC 发布原不可变 tgz，Stable/RC/Hourly 分别使用 latest/rc/hourly。自动调用方仍为 `release.yml`，手动入口 `npm-publish.yml`；两者使用 npm-release environment。先核对真实 trust、provenance、integrity 与 dist-tag，不把代码配置视为权限已生效，不索取或记录长期 token。

## 本机 iOS 与平台分发

`release ios <SOURCE_SHA>` 只构建指定源码的本机开发签名 App。执行前读 [iOS 命令行构建](references/ios-local-build.md)，核对该源码自己的 Mobile 版本，复用现有开发签名。默认使用现有 Aqua LaunchAgent 执行 Expo、CocoaPods 和 xcodebuild，不操作 Xcode 窗口或后台 daemon。

本机构建、真机安装、公开 Mobile Release、TestFlight 和商店发布分别核验、按各自授权执行。安装已有 App 使用相邻 [deploy skill](../deploy/SKILL.md)。CI unsigned device archive 不可安装；App Store Connect IPA 构建不代表 Apple 已接收或测试者可用。TestFlight 上传、处理、分组、App Store 提交另行核对。

macOS 保留 Developer ID/notarization；Windows Stable 要求签名，预览制品明确记录 unsigned 状态。平台签名不能被 SHA256 清单替代。个人账号、设备标识、主机路径、凭据与证书私有信息只在运行时按需读取，不写入 skill、制品或公开 PR。
