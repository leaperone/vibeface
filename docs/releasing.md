# 构建与发布

[CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml) 在 PR、`main` push 和 `v*` tag 上运行同一流程。PR 与 `main` 只上传 Actions 制品，不创建公开版本，也不部署主机。公开版本在 [Releases](https://github.com/leaperone/vibeface/releases) 下载。

## 可下载的包

每个平台的 Actions artifact 包含 `vibeface-node-<version>-<target>.tar.gz` 和同名 `.sha256`。四种包均在对应架构的原生 runner 构建，解包后运行 Node 多进程验收，不调用模型。

| 平台 | Rust target | 构建和验收环境 |
| --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | Ubuntu 24.04 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| macOS Intel | `x86_64-apple-darwin` | macOS 15 Intel |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 ARM64 |

Linux 包依赖 glibc，未在旧版发行版验收。macOS 包没有 Apple 开发者签名或 notarization。根据设备选择包，在存放包的目录先校验再解压：

```sh
shasum -a 256 -c vibeface-node-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-node-<version>-<target>.tar.gz
cd vibeface-node-<version>-<target>
./vibeface-node --version
./vibeface-node --help
```

包内有 Rust 程序、SDK sidecar 源码、文档、用户级服务安装脚本和 `BUILD-INFO.json`，不含身份、配置、数据库、Provider 程序、模型凭证或 npm 依赖。干净 Git 提交的正常构建会记录版本、target 和源码提交；`--skip-build` 或未提交源码构建的 `source_commit` 为 `null`，不能作为提交身份的证明。

运行任务前须另装并登录相应 Provider。Codex SDK 管理路径还需 Node.js 18+，在包的 `sidecars/` 下执行 `npm ci --omit=dev`。Cursor 管理路径需另装 Cursor SDK Bridge 并配置凭证。初始化、配对、服务安装和公网 TLS 见 [Node 使用说明](node.md)。

## CI 验证范围

质量检查包含 Rust fmt、workspace tests、Clippy、声明的最低 Rust 版本编译检查、工作流和 shell 静态检查、JavaScript 语法，以及 Atlas 迁移文件校验；不连接用户数据库。打包任务校验 SHA256、源码提交、版本和 target，再启动解包后的程序运行现有多进程验收。Linux x64 还安装包内锁定的 SDK 依赖并导入模块，不开启模型会话。

验收失败时只上传报告和三个测试进程日志，不上传测试目录或数据库。正常制品保留 14 天，诊断保留 7 天。CI 不验证真实模型调用、实体手机、物理局域网、公网反向代理或用户后台服务安装。

## 显式发布版本

整个 workspace 使用同一版本；Git tag 在 Cargo 版本前加 `v`，不按组件分别发布。

| 渠道 | Tag 格式 | 示例 | GitHub Release |
| --- | --- | --- | --- |
| stable | `vX.Y.Z` | `v0.0.1` | 正式版本，标记为 Latest |
| nightly | `vX.Y.Z-nightly.YYYYMMDD.N` | `v0.0.1-nightly.20261017.1` | 预发布，不标记为 Latest |
| rc | `vX.Y.Z-rc.N` | `v0.0.1-rc.1` | 预发布，不标记为 Latest |

`X.Y.Z` 是目标正式版本，数字不带前导零。nightly 日期按发布时的 UTC 日历日期填写，`N` 是该目标版本、当天从 `1` 递增的发布序号；rc 的 `N` 则在该目标版本内从 `1` 递增。日期必须有效，序号不带前导零。例如 `0.0.1-nightly.20261017.1` 表示目标正式版本 `0.0.1` 在 UTC 2026-10-17 的第一个 Nightly。

nightly 表示开发预发布，需选择 Nightly 更新渠道或手动下载，不能通过 GitHub 的 Latest 稳定版入口获取。tag 一经发布不可移动、删除重用或覆盖；不创建可变的 `nightly` tag，不追加 build metadata 或 SHA。包内 `BUILD-INFO.json` 记录实际源码提交。

1. 通过 PR 更新 workspace 版本及 `Cargo.lock`，完成检查并合并到 `main`。
2. 按上述格式在已合并的提交创建并推送 `v<version>` tag。tag 必须与 Cargo 版本完全相同，且指向已进入 `main` 的提交；CI 拒绝其他后缀或格式。
3. 查看 tag 触发的 CI，确认质量检查和四个平台的包验收全部成功。
4. 发布任务创建相应渠道的 draft Release，上传四个包和四个 checksum，核对资产清单并重新下载逐字节比较，最后按渠道公开 Release。恢复已有 draft 时也会校正预发布和 Latest 属性。

只有发布任务拥有 `contents: write` 权限。失败留下的 draft 可以在修复原因后重跑；公开 Release 不会被覆盖。同名公开版本需要更正时，使用新的版本和 tag。合并 PR 不会自动推送 tag。

## 更新渠道

Rust 更新器从 `leaperone/vibeface` 的公开 GitHub Releases 发现版本，不使用 Actions 的临时制品。Stable 只选择 `vX.Y.Z` 正式版，Nightly 只选择 `vX.Y.Z-nightly.YYYYMMDD.N` 预发布；两者都不选择 rc。更新器比较数值版本，不按 Release 创建时间或字符串排序，不自动降级。尚无正式版时，Stable 报告没有匹配版本，不回退到 Nightly。

自动更新默认关闭，默认渠道为 Stable。策略保存在数据目录的私有 `update.json`，不进入共享会话或 DAG。配置、检查及手动应用命令见 [选择更新渠道](node.md#选择更新渠道)。旧版本没有更新器，首次使用须按下一节手动安装包含更新器的新包。

匿名 GitHub API 请求被限流时，更新器明确报错并保留现版，自动更新在下轮再次检查。可在更新服务的运行环境提供 `GITHUB_TOKEN` 提高额度；token 只发往 `api.github.com`，不发给下载 CDN，也不写入配置或更新策略。后台服务不保证继承终端的临时环境变量。

更新器只接受受管安装的 `current` 符号链接，下载对应平台的完整包，核对 SHA256、版本、target、源码身份和存储兼容声明后再安装。包和 checksum 都通过 GitHub HTTPS 下载，这依赖 GitHub 账户、仓库和 TLS 的可信性，不是独立发布签名。macOS 开发者签名和 notarization 仍未提供。

每小时的独立用户服务调用 `update apply --automatic`，不调用模型，不消耗模型 token。更新器先下载并校验，再请求 Node 进入维护状态。正在执行的操作、管理 Agent 调用或 `unknown` 结果会推迟升级。更新器不会为升级强行结束任务。更新只更换程序和随包 sidecar，不覆盖账户、身份、历史、配置和凭证，也不更新 Codex、Cursor 或其他 Provider 程序。使用 Codex SDK 时，更新器在停服务前按新包的 lock 安装 sidecar 依赖，需要可用的 npm；安装失败保留现版，不复制旧 `node_modules`。Cursor SDK 不需要这一步。

更新器只安装声明与当前数据格式兼容的包。安装后会重启用户服务并核验本机健康状态；失败时恢复上一份程序入口。更新器不自动恢复业务数据库，不能以程序回退代替数据恢复。网络、校验或安装错误通过命令退出状态和私有更新日志报告，不当作“没有更新”。

## 首次安装更新器或手动更换程序

先停止 Node 用户服务，确认进程退出，再备份完整数据目录。备份包括身份和历史，应保持私密。不要让新旧程序同时使用同一数据目录。

校验并解压新包到受管安装根目录下的独立目录，通过 `current` 符号链接选择程序。例如，先将解压后的包移动到 `$HOME/.local/share/vibeface/releases/<version>`，再创建指向它的 `$HOME/.local/share/vibeface/current`。保留整个包，不单独复制 Rust binary；更新器需要同包的脚本、sidecar 和 `BUILD-INFO.json`。

用 `current/vibeface-node` 前台启动并核对原账户和历史，随后按 [后台运行说明](node.md#作为后台应用运行) 安装指向这一固定路径的用户服务。停止前台进程后再启动用户服务，不更改原数据目录。已有私有凭证启动脚本时，保留它，并让它通过 `current` 启动 Node。

确认新程序可用后，再执行 `update configure` 设置渠道和自动更新。普通 `cargo install` 或直接复制 binary 不构成受管安装，更新器会要求先建立完整包与 `current` 链接。

若版本改变存储格式且不声明兼容，应按该版本说明手动处理。需要数据恢复时，停止程序并恢复对应版本的完整备份，不直接让旧程序打开已经升级的数据。
