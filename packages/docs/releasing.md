---
title: 构建与发布
description: 独立发布 Core、Web App、CLI、Desktop、Mobile 与官网，并核对依赖、更新和部署结果。
---

# 构建与发布

各应用使用自己的版本、Tag、Release 和完成清单。一个应用失败不阻止无依赖关系的应用发布。npm 是 CLI 的分发方式，共用 CLI 版本。共享 Rust crate、TypeScript client、theme 和密码学绑定仍由 workspace 管理，其源码和锁文件是对应应用的构建输入。

| 应用 | Tag | 发布入口 | 交付物与依赖 |
| --- | --- | --- | --- |
| Core | `core-v<version>` | `release-core.yml` | 四个原生目标的 Core 归档；完成后自动部署公共 Node |
| Web App | `app-v<version>` | `release-app.yml` | 独立静态网页归档；完成后自动部署 App |
| CLI/npm | `cli-v<version>` | `release.yml` | 四个完整 CLI 运行包与 npm 包；精确锁定已发布的 Core 和 App |
| Desktop | `desktop-v<version>` | `release-desktop-app.yml` | 四个目标的桌面安装包和 ZIP；精确锁定一个 CLI 运行包 |
| Mobile | `mobile-v<version>` | `release-mobile.yml` | Android APK、iOS 设备归档与配置签名后的 IPA；不携带 App 网页或 Core |
| WWW | `www-v<version>` | `release-www.yml` | 官网与文档静态归档；完成后自动部署 WWW |

所有版本使用 `X.Y.Z`、`X.Y.Z-rc.N` 或 `X.Y.Z-hourly.YYYYMMDDHH`。首个正式版前基准为 `0.0.1`，main 固定保留开发版本 `0.0.1-hourly.2026111300`。各应用的正式版本号独立，不要求一起递增。不再使用全局 `v<version>` 产品 Tag。

## 精确依赖与制品校验

每个 Release 的 `release.json` 使用同一闭合 schema，包含 `schema`、`app`、`version`、`tag`、`source_commit`、`channel`、`contracts`、`dependencies`、`assets`。`contracts` 声明当前 `data_epoch=1` 和 `update_protocol=1`。每项 asset 包含文件名、字节数和 SHA256。

CLI 的生成源码中，`releases/cli.json` 锁定 Core 和 App；Desktop 的 `releases/desktop.json` 锁定 CLI。每个 pin 包含依赖的精确 Tag、版本、源码 SHA 和 `release.json` 的 SHA256。发布器先验证依赖清单和 Tag，再下载其中对应平台的归档并校验摘要。组包消费已发布的字节，不重新编译或签名依赖。

Desktop 的来源收据记录自身身份、GPUI 版本和内置 CLI 归档的真实版本、源码与摘要。CLI 包的来源记录见 [CLI 包的构建信息](#cli-包的构建信息)。

每个应用只汇总自身的构建目标。生产者通过本次运行的不可变 artifact ID 传递制品；完整校验后才追加公开 `release.json`。缺少清单的 Release 不供安装器、更新器、下游组包或部署消费。已公开 Tag 和资产字节不覆盖；同一 Tag 重跑时先对账，字节冲突时停止并使用新版本。

信任边界是官方 GitHub 仓库、不可变 Tag、HTTPS 和 GitHub 提供的资产摘要。SHA256 清单不等同于独立发布签名。macOS 程序另有 Developer ID 签名与 notarization；Windows Stable 需要 Authenticode，Hourly/RC 可交付明确标注的未签名预览。

### CLI 包的构建信息

CLI 包中的 `BUILD-INFO.json` 记录包的来源。发行包中，CLI 的身份与当前 Release 一致，Core 与 App 的身份分别与对应的精确 pin 一致。CLI、Core 和 App 可以使用不同的版本和源码提交。源码包中的三项版本取自各自的清单，源码提交取自当前 checkout。

`verify_build_info` 定义完整的字段名和取值校验。`seal_bundle` 写入 `BUILD-INFO.json`，并用 `FILES.sha256` 封存包内文件。[字段校验](../../scripts/release_format.py)和[组包实现](../../scripts/release_archive.py)

## 自动发布

`hourly.yml` 在 UTC 每小时 17 分、main push CI 成功后或手动运行时对账 Core。`daily.yml` 仅在 `America/Los_Angeles` 每日 09:00 和 18:00 定时启动客户端发布，并跟随夏令时。构建和发布可能稍后完成。

每个客户端批次固定一个通过 main push CI 的源码和 UTC 小时版本后缀，继续使用 Hourly 分发渠道。Web App、CLI/npm、Desktop 和 Mobile（Android APK 与 iOS）按构建输入或依赖 pin 的变化发布，无变化时复用完整 Release。CLI 锁定批次截止时最新完整发布且验证通过的 Hourly Core，以及本批次选定的 Web App。Desktop 锁定选定的 CLI。协调器跟踪各个组件的发布运行，可跨小时继续依赖构建。官网仅手动发布。

生成提交只改所选应用版本、相应 Cargo.lock 本地包版本或 Mobile build number，以及该应用的依赖 pin。Android versionCode 使用 `YYMMDDHH`，iOS buildNumber 使用 `YYMM.DD.HH`。发布来源校验重建这棵 Git tree，并验证唯一 main parent 的 CI。Tag 不移动。GitHub 的定时运行可能延迟或跳过。

## 人工发布与构建验证

人工发布指定应用、渠道和精确版本。先在一个 PR 中准备该应用的版本及依赖 pin；不同时改动无关应用。Stable 依赖必须也是 Stable。核对版本源码已进入 main，且同一 SHA 的 main push CI 成功，再创建该应用的不可变 Tag。Hourly 由协调器生成。

```sh
python3 scripts/release.py plan --app cli --tag 'cli-v<version>' --require-main
```

真实发布只从对应的精确 Tag 运行，并显式设置 `publish=true`。构建、签名、公开发布、npm、服务器部署与移动端分发分别核对，不能以其中一项成功代表其余步骤完成。

```sh
gh workflow run release.yml --ref 'cli-v<version>' -f tag='cli-v<version>' -F publish=true
```

官网通过以下命令手动发布。完整发布后部署官网。

```sh
gh workflow run release-www.yml --ref 'www-v<version>' -f tag='www-v<version>' -F publish=true
```

各入口的手动构建默认 `publish=false`，不写公开 Release、不发布 npm、不部署。CLI/Desktop 构建需要其源码中已有完整的依赖 pin；缺少 pin 时明确拒绝，不从其他产品猜测依赖。分支 CI 的源码运行包检查仍可从同一 checkout 构建 CLI/Core/App，用于开发验证。

失败任务优先重跑原 run 的失败 job，复用原不可变 artifact。响应未知时先查询远端事实，不直接重复写入。新的修复源码使用新的 Tag；不能移动旧 Tag 或替换旧资产。

## 安装与更新

Unix 与 Windows 安装器、Rust 更新器只跟踪 `cli-v` 发布线。Stable 选择 `cli-vX.Y.Z`，Hourly 选择 `cli-vX.Y.Z-hourly.YYYYMMDDHH`，均不选择 RC、其他应用或缺少完整清单的 Release。按数值版本比较，不自动降级，不在 Stable 缺失时退回 Hourly。

安装器下载整份运行包并核对 CLI 身份、Core/App 精确 pin、平台、文件摘要、协议与 epoch，随后创建受管的 `releases/<cli-version>` 和 `current`。完整命令见[安装说明](install.md)。不兼容的旧开发安装按新格式重新安装；不提供旧产品 manifest、旧 Tag 别名或更新协议回退。

更新器保留已有的维护租约、空闲检查、数据快照、激活 journal、原子入口切换与健康检查。存在活动任务或结果未知操作时推迟更新，不强行结束任务。数据 epoch 不匹配时拒绝安装；不隐式转换、清空身份或覆盖数据。失败时按 journal 恢复程序入口，数据恢复需使用匹配的备份，不能用旧程序读取不同 epoch 的数据。

安装 CLI 或 Desktop 受管 Node 的后台服务时会同时注册定时更新。Desktop App 本体跟踪独立的 `desktop-v` 发布及自己的渠道。下载的 App 包通过 Release 清单、制品摘要和适用的系统签名校验后，在正常退出时切换。更换桌面 App 不替换仍在运行的后台服务。

npm 安装仍由 npm 更新，不由 Rust updater 更新。npm 的 Stable、RC、Hourly 分别使用 `latest`、`rc`、`hourly` dist-tag。CLI Release 完成后，`npm-publish.yml` 通过 OIDC 发布原不可变 `.tgz`；registry 对账确认 integrity 和渠道后才报告成功，不重复发布未知结果，也不把 dist-tag 回拨到较旧版本。

npm Trusted Publishing 的自动调用方保持 `release.yml`，手动入口为 `npm-publish.yml`，两者使用 `npm-release` environment。权限配置由 npm/GitHub 控制，代码改动不能证明远端 trust 已生效。

## 原生平台与签名

| 平台 | Rust target | 构建环境 |
| --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | Ubuntu 24.04 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 |
| Windows x64 | `x86_64-pc-windows-msvc` | Windows 2025，静态 MSVC CRT |

macOS Core 和 CLI 各自签名、公证，Desktop 保留其已封存依赖的签名，不再重签依赖。仓库需要现有 `MACOS_CERTIFICATE_P12_BASE64`、`MACOS_CERTIFICATE_PASSWORD`、`APPLE_API_KEY_P8_BASE64`、`APPLE_API_KEY_ID` 和 `APPLE_API_ISSUER_ID`。Windows 使用现有 `WINDOWS_CERTIFICATE_PFX_BASE64` 和相关校验配置；不会购买证书或修改系统信任设置。

iOS 设备归档与实际可分发的 App 区分记录。缺少分发证书与描述文件时，Hourly 的 unsigned archive 不可直接安装。App Store Connect IPA 的构建成功不代表 TestFlight 已接收，也不代表商店已审核；上传、处理与测试组分配按[移动端分发说明](ios.md)单独完成。

## 源码构建

先按[源码构建工具](#源码构建工具)准备环境，再依次生成浏览器密码学资源、网页和源码运行包。

```sh
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
bash scripts/package-node.sh
```

Windows 使用 `python3 scripts/package-node-windows.py`。从干净的 checkout 构建源码运行包。包的来源记录见 [CLI 包的构建信息](#cli-包的构建信息)。

Web 资源在 `apps/app/dist/client`，Core 只读取静态输出，不运行 SSR 服务。打包器包含用户级服务脚本和离线文档，不包含身份、配置、数据库、Provider 程序或模型凭据。

### 源码构建工具

- Python 和 Node.js 采用 CI 的 `package` 作业所用的版本。[CI 配置](../../.github/workflows/ci.yml)
- Rust 的最低版本由 workspace 的 `rust-version` 声明。[Rust 清单](../../Cargo.toml)
- pnpm 的版本由根清单的 `packageManager` 指定。[JavaScript 清单](../../package.json)
- `wasm-bindgen-cli` 的版本需与 `vibeface-secure` 的 wasm32 依赖 `wasm-bindgen` 精确匹配。[浏览器密码学模块清单](../../crates/secure/Cargo.toml)

在仓库根目录安装浏览器密码学构建所需的 Rust target 和 CLI。把命令中的占位符替换为清单中 `wasm-bindgen` 的数字版本号，去掉前导的 `=`。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version '<wasm-bindgen 版本号>' --locked
```

## 本地检查

```sh
python3 -B scripts/release.py self-test
python3 -B scripts/tests/release/test_cli.py
python3 -B scripts/tests/release/test_publisher.py
python3 -B scripts/tests/release/test_source.py
python3 -B scripts/hourly.py --self-test
python3 -B scripts/check-release-workflow.py --self-test
python3 -B scripts/release-dispatch.py --self-test
bash install.sh --self-test
node scripts/package-npm.mjs --self-test
node scripts/build-desktop.mjs --self-test
```

发布工具的职责与最小复制依赖见[模块边界](design/release-modules.md)。源码构建、CI 分层与受控进程检查见[CI 说明](ci.md)。官方 Core/App/WWW 上线使用精确已完成 Release，见[服务部署](service-deployments.md)。
