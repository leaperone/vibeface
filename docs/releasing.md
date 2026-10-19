# 构建与发布

[CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml) 在 PR、`main` push 和 `v*` tag 上运行同一流程。PR 与 `main` 只上传 Actions 制品，不创建公开版本，也不部署主机。公开版本在 [Releases](https://github.com/leaperone/vibeface/releases) 下载。

## 可下载的包

每个平台的 Actions artifact 包含 `vibeface-<version>-<target>.tar.gz` 和同名 `.sha256`。三种包均在对应架构的原生 runner 构建，解包后运行 Node 多进程验收，不调用模型。

| 平台 | Rust target | 构建和验收环境 |
| --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | Ubuntu 24.04 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 ARM64 |

Linux 包依赖 glibc，未在旧版发行版验收。macOS 包没有 Apple 开发者签名或 notarization。根据设备选择包，在存放包的目录先校验再解压：

```sh
shasum -a 256 -c vibeface-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-<version>-<target>.tar.gz
cd vibeface-<version>-<target>
./vibeface --version
./vibeface --help
```

包内有 Rust 程序、SDK sidecar 源码、浏览器密码学 JS/WASM、文档、用户级服务安装脚本和 `BUILD-INFO.json`，不含身份、配置、数据库、Provider 程序、模型凭证或 npm 依赖。干净 Git 提交的正常构建会记录版本、target 和源码提交；`--skip-build` 或未提交源码构建的 `source_commit` 为 `null`，不能作为提交身份的证明。

运行任务前须另装并登录相应 Provider。Codex SDK 管理路径还需 Node.js 18+，在包的 `sidecars/` 下执行 `npm ci --omit=dev`。Cursor 管理路径需另装 Cursor SDK Bridge 并配置凭证。初始化、配对、服务安装和公网 TLS 见 [Node 使用说明](node.md)。

## CI 验证范围

`crypto-web` 在固定版本的 wasm-bindgen 工具链生成一份浏览器密码学资源，运行 native Rust 与 WASM 的实际 Noise 互通和篡改、重放、身份错配负例。`web` 下载这份资源及其真实 TypeScript 声明，以锁定的 pnpm 依赖生成 TanStack Start 静态 SPA。检查和三个平台的打包任务下载同一份密码学资源和 SPA，再由 Rust 构建脚本内嵌。缺少资源时构建失败，不用空模块替代。

质量检查包含 Rust fmt、workspace tests、Clippy、声明的最低 Rust 版本编译检查、工作流和 shell 静态检查、JavaScript 语法、Web TypeScript 检查和静态构建，以及 Atlas 迁移文件校验。Atlas 不打开用户数据库。打包任务校验 SHA256、源码提交、版本、target、密码学资源和存储 epoch，再启动解包后的程序运行无账号网络验收。网页验收检查 SPA 路由、JavaScript、CSS、字体、每段内联脚本的 CSP hash、HEAD、缓存策略和未知资源的 404。Linux x64 还安装包内锁定的 SDK 依赖并导入模块，不开启模型会话。

源码打包前先生成密码学资源，再构建网页。安装 Node.js 22.12+、pnpm 11.9.0、Rust 的 `wasm32-unknown-unknown` target 和 `wasm-bindgen-cli 0.2.129`，然后执行：

```sh
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
bash scripts/package-node.sh
```

`build-web.sh` 校验生成资源、按 lockfile 安装依赖、生成路由、检查类型，再构建 `crates/node/web/dist/client`。Rust 只内嵌这个目录，不运行 Start 的 SSR 输出。页面和密码学资源使用 `no-store`，成功命中的 `/assets/` 内容使用 immutable 缓存。CSP 根据最终 HTML 生成脚本 hash，并允许 WASM、WebSocket 和 Radix 所需的动态样式。

验收失败时只上传报告和四个测试进程日志，不上传测试目录或数据库。正常制品保留 14 天，诊断保留 7 天。CI 不验证真实模型调用、实体手机、物理局域网、公网反向代理或用户后台服务安装。

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
3. 查看 tag 触发的 CI，确认质量检查和三个平台的包验收全部成功。
4. 发布任务创建相应渠道的 draft Release，上传三个包和三个 checksum，核对资产清单并重新下载逐字节比较，最后按渠道公开 Release。恢复已有 draft 时也会校正预发布和 Latest 属性。

只有发布任务拥有 `contents: write` 权限。失败留下的 draft 可以在修复原因后重跑；公开 Release 不会被覆盖。同名公开版本需要更正时，使用新的版本和 tag。合并 PR 不会自动推送 tag。

## 更新渠道

npm Nightly 的制品与发布流程见下面的 npm 章节。npm 安装由 npm 更新，不使用本节的 Rust 自更新器。

Rust 更新器从 `leaperone/vibeface` 的公开 GitHub Releases 发现版本，不使用 Actions 的临时制品。Stable 只选择 `vX.Y.Z` 正式版，Nightly 只选择 `vX.Y.Z-nightly.YYYYMMDD.N` 预发布；两者都不选择 rc。更新器比较数值版本，不按 Release 创建时间或字符串排序，不自动降级。尚无正式版时，Stable 报告没有匹配版本，不回退到 Nightly。

自动更新默认关闭，默认渠道为 Stable。策略保存在数据目录的私有 `update.json`，不进入共享会话或 DAG。配置、检查及手动应用命令见 [选择更新渠道](node.md#选择更新渠道)。首次建立受管安装可使用 [公开安装命令](install.md)，已安装的节点使用 `update apply`。

匿名 GitHub API 请求被限流时，更新器明确报错并保留现版，自动更新在下轮再次检查。可在更新服务的运行环境提供 `GITHUB_TOKEN` 提高额度；token 只发往 `api.github.com`，不发给下载 CDN，也不写入配置或更新策略。后台服务不保证继承终端的临时环境变量。

更新器只接受受管安装的 `current` 符号链接，下载对应平台的完整包，核对 SHA256、版本、target、源码身份和存储兼容声明后再安装。包和 checksum 都通过 GitHub HTTPS 下载，这依赖 GitHub 账户、仓库和 TLS 的可信性，不是独立发布签名。macOS 开发者签名和 notarization 仍未提供。

每小时的独立用户服务调用 `update apply --automatic`，不调用模型，不消耗模型 token。更新器先下载并校验，再请求 Node 进入维护状态。正在执行的操作、管理 Agent 调用或 `unknown` 结果会推迟升级。更新器不会为升级强行结束任务。更新只更换程序和随包 sidecar，不覆盖身份、历史、配置和凭证，也不更新 Codex、Cursor 或其他 Provider 程序。使用 Codex SDK 时，更新器在停服务前按新包的 lock 安装 sidecar 依赖，需要可用的 npm；安装失败保留现版，不复制旧 `node_modules`。Cursor SDK 不需要这一步。

`BUILD-INFO.json` 声明 `update_protocol=2` 和 `data_epoch=6`。包协议 2 要求同名 `vibeface` 程序和制品。设备授权和业务记录使用单一 `node.sqlite3`，数据库 `user_version=6`，Atlas HCL 为 `crates/node/schema.hcl`。本次部署需重新初始化全新数据目录，不提供旧 epoch 升级或旧节点混用。更新器只安装与本机存储 epoch 一致的包，不隐式转换或清空旧目录。安装后会重启用户服务并核验本机健康状态；失败时恢复上一份程序入口。更新器不自动恢复业务数据库，不能以程序回退代替数据恢复。网络、校验或安装错误通过命令退出状态和私有更新日志报告，不当作“没有更新”。

## 首次安装

公开安装入口为仓库根 `install.sh`。默认选择最新 Stable，显式 `--channel nightly` 选择最新 Nightly；按数值版本比较，忽略 rc 和 draft，不使用 `/releases/latest` 获取 Nightly。当前只有 Nightly，Stable 安装会明确失败。命令和后续 CLI 管理见 [安装说明](install.md)。

安装器只依赖 Bash、curl、tar、gzip、系统文本工具及 `sha256sum` 或 `shasum`，不需要 jq、Python、Node.js 或 Rust。它解析公开 Releases 和 tag 对应的源码提交，下载完整包与准确同名的 SHA256，校验版本、target、源码身份、协议和 data epoch，并拒绝危险包路径、符号链接和特殊文件。校验通过后建立 `releases/<version>`、`current` 与用户 bin 链接，不覆盖已有安装、身份或服务。

`bash install.sh --self-test` 使用临时目录、模拟 GitHub 响应和测试包验证渠道、数值排序、校验拒绝及已有安装保护，不改当前用户的正式安装或服务。CI 与 preflight 均运行它。SHA256 的信任边界与更新器一致，不是独立发布签名。

## 手动建立受管安装或更换程序

以下步骤用于从完整包手动建立受管安装。本版本使用 `data_epoch=6`，从旧数据格式部署时必须使用全新数据目录重新初始化，并重新配对同版本节点。只有新旧程序都使用 epoch 6 时，更换程序才可保留原数据目录和身份。

先停用旧更新定时服务，备份 `current` 目标、用户服务定义和私有启动脚本。确认维护状态空闲且无结果未知操作后，再停止 Node 用户服务并等待目录锁释放，然后备份完整数据目录。备份包括身份和历史，应保持私密。不要强行结束任务，也不要让新旧程序同时使用同一数据目录。

校验并解压新包到受管安装根目录下的独立目录，通过 `current` 符号链接选择程序。例如，先将解压后的包移动到 `$HOME/.local/share/vibeface/releases/<version>`，再创建指向它的 `$HOME/.local/share/vibeface/current`。保留整个包，不单独复制 Rust binary；更新器需要同包的脚本、sidecar 和 `BUILD-INFO.json`。

原子切换 `current` 后，建立 `~/.local/bin/vibeface` 到 `current/vibeface` 的链接。旧 CLI 链接只有在确认属于该受管安装时才可移除，不覆盖用户自有文件。使用全新或已确认属于 epoch 6 的数据目录前台启动，核对设备身份与状态，随后按 [后台运行说明](node.md#作为后台应用运行) 配置指向这一固定路径的用户服务。Linux 服务名是 `vibeface.service`，先卸载旧服务定义，避免双进程。停止前台进程后再启动用户服务。

已有私有凭证启动脚本时，保留内容和私有权限，仅将末端启动路径改为 `current/vibeface`。不要用 `service install` 覆盖凭证脚本。核对新服务的 PID、版本、设备身份、历史及本机健康状态，再恢复原更新渠道和自动更新策略。

确认新程序可用后，再执行 `update configure` 设置渠道和自动更新。普通 `cargo install` 或直接复制 binary 不构成受管安装，更新器会要求先建立完整包与 `current` 链接。

此版本不转换旧数据库，也不自动清空旧目录。需要恢复旧环境时，停止新程序，并使用旧程序及其对应的完整备份，不能让不同数据格式的程序共用目录。

## npm Nightly 制品与发布

npm 包名为 `@leaperone/vibeface`，Nightly 使用同名不可变版本和 `nightly` dist-tag。当前只配置分发与 CI，尚未公开发布，也未确认 npm scope 的发布权限。模板 `npm/package.json` 保持 private；打包脚本验证三个原生包后，为发布制品写入真实版本并移除 private 限制。

`scripts/package-npm.mjs` 接收三个平台的完整 `.tar.gz` 与同名 SHA256，要求版本、源码提交、平台和包协议一致。它拒绝危险路径、链接、特殊文件、校验错配和缺失资源，把完整包放入 `vendor/<target>`，再生成一个含全部平台的 `.tgz`。只有一个 npm 发布身份和一次发布操作，代价是下载包含其他平台的程序。没有 postinstall、运行期下载或新增 npm 依赖。

PR、main 和 Nightly tag 的 CI 都生成 `npm-package-<SHA>` artifact，包含 `.tgz` 和记录源码 SHA、SHA256、npm integrity 的 JSON。CI 在三个原生平台上以 `--ignore-scripts` 全局安装到隔离 prefix，然后验证版本、真实进程及随包网页。Stable 和 rc 延续现有 GitHub 发布流程，不生成 npm 包。

本地检查与打包命令如下。输入目录只能包含对应版本的三个包与三个 checksum，默认输出 `.artifacts/npm`，已有同名制品会被保留并报错：

```sh
node scripts/package-npm.mjs --self-test
node scripts/package-npm.mjs --input /absolute/path/to/verified-packages --source-commit <full-commit-sha>
node scripts/package-npm.mjs --verify /absolute/path/to/leaperone-vibeface-<version>.tgz --source-commit <full-commit-sha>
```

`npm-publish.yml` 只接受手动触发，不在 PR、merge 或 tag 时自动向 npm 发布。触发时必须选择与输入相同的 tag ref，并核对 checkout 提交等于工作流的 `GITHUB_SHA`，使 npm provenance 对应制品源码。默认 `publish=false`，从成功的对应 tag CI 下载已验收制品，重新核对身份并执行 `npm publish --dry-run`。发布阶段不重新构建另一份包。

未来首次公开发布前，先确认 `@leaperone/vibeface` 的 npm scope 权限，在 npm 自己的登录流程中完成认证，并对审核过的 tag 制品执行一次人工 bootstrap。不要把 npm token 发到聊天或写入仓库。首次发布命令是 `npm publish /absolute/path/to/leaperone-vibeface-<version>.tgz --tag nightly --access public --ignore-scripts`。

包存在后，在 npm 配置 GitHub trusted publisher，仓库为 `leaperone/vibeface`，workflow 文件为 `npm-publish.yml`，环境为 `npm-release`。GitHub 的同名 environment 应配置 required reviewers；该远端设置不由工作流文件自动保证。CI 使用 Node.js 24 内置的新版 npm 与 OIDC，不存长期 npm token。

配置好发布权限后，先运行手动 dry run：

```sh
gh workflow run npm-publish.yml --repo leaperone/vibeface --ref v<version> \
  -f tag=v<version> -F publish=false
```

明确决定公开该版本时，才把 `publish` 设为 true。工作流拒绝将 nightly 指向相同或更旧版本，使用 provenance 发布，再核对 registry 中的版本 integrity 与 nightly dist-tag。网络或结果未知时先查询 registry 对账，不直接重复发布。公开版本不能覆盖，不能通过复用 tag 更换内容。
