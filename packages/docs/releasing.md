---
title: 构建与发布
description: 了解 Vibeface 的 CI、制品和 Nightly 发布流程。
---

# 构建与发布

[CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml) 按改动范围选择 `main` push 的核心检查；开发分支 PR 不触发 Actions，Draft 与 Ready 都不例外。merge queue 执行全工作区核心检查，定时和手动执行扩展回归。`main` 的常规 CI 只上传 Actions 制品，不创建公开版本。门户仅在 www 正式版 Tag 时由 `deploy-www.yml` 请求部署，网页 App 在产品正式版 Tag 时由 `deploy-app.yml` 请求部署。版本号含 `-` 时不自动发站。公开版本在 [Releases](https://github.com/leaperone/vibeface/releases) 下载。具体选择规则见 [按改动选择 CI](ci.md)。

## 按产品组合发布

Core、CLI、Web App 和 Mobile 共享一个版本、一个 `v<version>` Tag 与一个 GitHub Release。`www-v<version>` 只发布独立官网与文档。产品版本以 `apps/cli/Cargo.toml` 为准，Core、Web App 和 Mobile 的发行版本必须与之相等。

| 发布组 | Tag | 交付物 |
| --- | --- | --- |
| product | `v<version>` | 三个平台的完整电脑包与 Core、Web App 归档、Android APK，以及各自校验信息 |
| www | `www-v<version>` | 静态官网与文档归档 |

首个正式版为 `0.0.1`。Nightly 使用 `0.0.1-nightly.YYYYMMDD.N`，日期取 UTC 准备日期，序号按发布组递增。Mobile 的 `expo.version` 保持 `0.0.1`，Android versionCode 独立递增。当前 Mobile 发行目标是 Android APK，不代表 TestFlight、App Store 或 OTA 交付。

身份检查后，Web、macOS 原生程序、Linux x64 原生程序、Linux ARM64 原生程序和 Android 同时构建。原生任务一次编译本平台 CLI 与 Core。每个平台的组包只等自己的原生输入和 Web，不等待其他平台，也不读取另一个公开 Release。

流水线按不可变 artifact ID 传递输入，核对源码、版本、目标平台、完整文件集合及 SHA-256。所有目标通过后汇总 17 个载荷文件与一个 `release.json`，由唯一发布任务上传、回读验证，再公开 Release。缺失、错源、损坏或失败目标都会阻止公开。

`COMPONENT-INFO.json` 与 `release.json` 的 schema 保持 1。`BUILD-INFO.json` 仍包含九个既有字段，CLI、Core、App 的版本和源码提交必须完全一致。数据 epoch 与更新协议保持 1。旧公开 Tag 与字节保持不可变，不创建旧应用前缀别名。

## 每 4 小时 Nightly 的发布约束

协调入口仍是 `ci.yml` 的 `daily-nightly`。定时扩展检查使用 `23 */4 * * *`，每日 UTC 00:23、04:23、08:23、12:23、16:23、20:23 发起，只在 main 的 `ci-gate` 成功后执行协调。手动 CI 和普通 main push 不触发自动 Nightly。产品四应用分配一个共同版本和 `v` Tag，官网独立分配版本和 `www-v` Tag；即使业务源码没有变化，每个四小时批次仍递增版本。

启用需要明确授权未来每四小时机械版本 PR 自动 squash，并配置单仓库 fine-grained `NIGHTLY_RELEASE_TOKEN`。实现或合并代码不代表凭证已配置或自动发布已启用。PAT 只处理受信版本 PR 创建与普通 squash；版本分支、只创建的 Tag 和精确 dispatch 使用 job 级 `GITHUB_TOKEN`。缺少凭证时明确失败，不挪用部署 token 或修改保护规则。

原 schedule run 的 `created_at` 转为 UTC 后，向下取整到 00、04、08、12、16、20 时的批次。批次分支为 `bot/daily-nightly/YYYYMMDDHH`，版本日期仍为原 run 的 `YYYYMMDD`。同日不同批次各建一个版本 PR，并继续递增 Nightly 序号与 Android versionCode；协调器全局串行。延迟启动或跨日重跑仍恢复原批次唯一 PR、版本、实际 squash SHA 和留存制品，校验完整机械 tree、作者、base、head 和 Android versionCode。旧批次若尚无分支与 PR，而 main 的产品或官网 Nightly 日期已晚于该批次，则停止初始化，避免版本倒退。已有批次 PR 格式不符合时停止，不另建替代 PR 或重新分配版本。

只有实际合并 SHA 的 main push CI success 放行。产品与官网可并行启动独立发布运行，产品内部按依赖并行构建；不再先公开 Core、App 再启动 CLI。由 `GITHUB_TOKEN` 创建 Tag 后显式 dispatch，并指定 `publish=true`。

网络响应未知时先查询 PR、ref 或原 run 对账。发布恢复复用原运行的完整产品 artifact，禁止重建后覆盖公开字节。npm 失败单独报告失败；核对 registry 后恢复原 npm artifact，不把 GitHub Release 成功当作 npm 成功。缺失、过期或字节冲突的原制品阻止恢复。

## Stable App 的站点部署

仅 Stable `vX.Y.Z` 的完整产品发布成功后，`deploy-app.yml` 使用 `VIBEFACE_RELEASE_DISPATCH_TOKEN` 向 `leaperone/leaperone-releases` 发送 `deploy-vibeface-app`，携带确切 tag 和源码 SHA。接收端校验 tag、main 祖先关系、release 清单和归档内容后部署到现有站点，并检查 `/`、`/connect`、`/join` 及首页引用的静态资源。源站首页必须与发布产物一致；公网入口和静态资源必须返回 200，页面须保留原启动脚本，静态资源须与产物一致。公网检查允许现有 Cloudflare 外部统计脚本，循环跳转使验收失败。派发成功仅表示接收了请求，仍须核对发布仓库的部署与冒烟结果。

## 可下载的包

每个平台的 Actions artifact 包含 `vibeface-<version>-<target>.tar.gz` 和同名 `.sha256`。`main` 的核心检查按需生成 Linux x64 包；扩展检查在三种架构的原生 runner 构建，解包后分别启动网页进程。多 Node、调度、跨设备协作和公共入口的多进程验收只在 Linux x64 运行一次，聊天 Agent 与写入保护验收只在 macOS ARM64 运行一次，都不调用模型。macOS 两项独立进程验收并行执行，保留各自状态和完整观察。

| 平台 | Rust target | 构建和验收环境 |
| --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | Ubuntu 24.04 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 ARM64 |

Linux 包依赖 glibc，未在旧版发行版验收。扩展检查和产品 Tag 产出的 macOS 包，`vibeface` 与 `vibeface-core` 程序使用 Developer ID 签名，启用 hardened runtime，identifier 为 `cn.leaperone.vibeface`，并通过 Apple notarization。应用防火墙保持默认的“自动允许下载的已签名软件”设置时据此放行，安装和更新后不再询问是否接受传入连接。`main` 的核心检查不生成 macOS 包。本地运行 `scripts/package-node.sh` 时，只有设置 `VIBEFACE_CODESIGN_IDENTITY` 才签名；同时设置 `VIBEFACE_NOTARY_KEY`、`VIBEFACE_NOTARY_KEY_ID` 和 `VIBEFACE_NOTARY_ISSUER` 才提交 notarization，三者须同时设置，状态不是 `Accepted` 时打包失败。裸 Mach-O 无法 staple；带隔离属性的文件首次运行时由 Gatekeeper 在线核对公证记录。Apple 接受公证后，在线记录可能数分钟后才能查到，CI 因此按退避间隔重试 Gatekeeper 评估，最长约 14 分钟，只接受 `source=Notarized Developer ID`。产品发布入口验证签名和公证 `Accepted`，Gatekeeper 在线验收另行记录。

macOS 打包需要以下仓库 secrets，缺任何一项时打包失败，不回退为未签名包：

- `MACOS_CERTIFICATE_P12_BASE64`：含私钥的 "Developer ID Application" 证书 `.p12`，base64 编码。
- `MACOS_CERTIFICATE_PASSWORD`：该 `.p12` 的导出密码。
- `APPLE_API_KEY_P8_BASE64`：App Store Connect API 密钥 `.p8`，base64 编码。
- `APPLE_API_KEY_ID`：该 API 密钥的 Key ID。
- `APPLE_API_ISSUER_ID`：App Store Connect 的 Issuer ID。

根据设备选择包，在存放包的目录先校验再解压：

```sh
shasum -a 256 -c vibeface-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-<version>-<target>.tar.gz
cd vibeface-<version>-<target>
./vibeface --version
./vibeface --help
```

包内有 Rust 程序、浏览器密码学 JS/WASM、文档、用户级服务安装脚本和 `BUILD-INFO.json`，不含身份、配置、数据库、Provider 程序或模型凭证。打包要求干净 Git 提交，记录版本、target、源码提交和各组件身份；存在未提交更改时拒绝生成发行包。

发布构建脚本把本机源码、用户目录和工具缓存路径替换成通用路径；压缩包清除构建账号和扩展属性。打包拒绝过期生成物和额外文件。默认重新编译原生程序；CI 可显式消费通过同 run、同源码、target 和完整摘要校验的原生输入，不接受任意二进制或无校验的跳过构建。

自定义 Rust 编译参数通过 `RUSTFLAGS` 或 `CARGO_ENCODED_RUSTFLAGS` 传入，后者优先。发布脚本保留这些参数并追加路径映射；按 Cargo 的优先级，此时不使用配置文件中的 `rustflags`。

运行任务前须另装并登录相应 Provider。Cursor 管理路径需另装 Cursor SDK Bridge 并配置凭证。初始化、配对、服务安装和公网 TLS 见 [Node 使用说明](node.md)。

产品发布的 static 任务复用 pnpm 与 Rust 工具缓存。`build-web.sh` 自身执行冻结的按需依赖安装，因此 Product 不再提前安装整个 workspace；www 仍独立安装。工具版本和生成物来源校验照常执行。

## CI 验证范围

`crypto-web` 在固定版本的 wasm-bindgen 工具链生成一份浏览器密码学资源，运行 native Rust 与 WASM 的实际 Noise 互通和篡改、重放、身份错配负例。`web` 下载这份资源及其真实 TypeScript 声明，以锁定的 pnpm 依赖生成 TanStack Start 静态 SPA。制品任务下载同一份密码学资源和 SPA；Rust 检查不依赖这些生成产物。Core 从 `VIBEFACE_APP_DIR` 或发行包同级 `web/` 读取完整静态目录，不在编译时内嵌网页或 WASM。Core 可独立构建和运行，未提供 App 时网页返回 404；App 构建和完整发行包缺少资源时失败。

质量检查按计划选择受影响的 crate，包含 Rust fmt、快慢测试、Clippy、声明的最低 Rust 版本编译检查、工作流和 shell 静态检查、JavaScript 语法、Web TypeScript 检查和静态构建，以及 Atlas 迁移文件校验。Atlas 不打开用户数据库。README 和项目指引不触发构建，`packages/docs/` 触发门户构建；`main` 的纯 App 改动只构建 Linux x64 包并验收网页资源，不运行 Core 库业务测试。`main` 保留相关快速测试、Clippy 和 Linux x64 制品。慢测试、最低 Rust 版本和三平台制品在扩展检查执行。`ci-gate` 要求本次计划中的任务全部成功。

打包任务校验 SHA256、源码提交、版本、target、密码学资源和存储 epoch。扩展检查在 Linux x64 启动解包后的程序运行无账号网络验收；每个选中的平台独立启动网页进程，检查 SPA 路由、JavaScript、CSS、字体、每段内联脚本的 CSP hash、HEAD、缓存策略和未知资源的 404。三平台 npm 冒烟检查安装已验证的 CLI 包并实际启动 Node 和网页；Provider 进程专项使用受控协议夹具，不调用真实模型。

源码打包前先生成密码学资源，再构建网页。安装 Node.js 22.12+、pnpm 11.9.0、Rust 的 `wasm32-unknown-unknown` target 和 `wasm-bindgen-cli 0.2.129`，然后执行：

```sh
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
bash scripts/package-node.sh
```

`build-web.sh` 校验生成资源、按 lockfile 安装依赖、生成路由、检查类型，再构建 `apps/app/dist/client`。Core 从这个目录读取静态资源，不运行 Start 的 SSR 输出，也不把网页编译进 Core。页面和密码学资源使用 `no-store`，成功命中的 `/assets/` 内容使用 immutable 缓存。CSP 根据最终 HTML 生成脚本 hash，并允许 WASM、WebSocket 和 Radix 所需的动态样式。

验收失败时只上传各模式的报告、进程日志和 `node.log`，不上传测试目录或数据库。正常制品保留 14 天，诊断保留 7 天。CI 不验证真实模型调用、实体手机、物理局域网、公网反向代理或用户后台服务安装。

## 门户静态制品

`apps/www` 是面向 `www.vibeface.dev` 的独立门户，使用根 `packages/docs/` 的公开文档。它不编译进 Rust，也不需要运行 Node 服务端。使用 Node.js 24，在仓库根目录执行：

```sh
pnpm install --frozen-lockfile
pnpm --filter vibeface-www run typecheck
pnpm --filter vibeface-www run build
pnpm --filter vibeface-www run check:static
```

产物在 `apps/www/dist/client`。CI 的 `www-<SHA>` artifact 包含相同目录，可用于审核或后续静态部署；成功发布 www 正式版 tag 后才调用 `deploy-www.yml`，向 `leaperone/leaperone-releases` 发送部署请求。请求成功不代表站点已经上线，仍需检查目标仓库的部署结果和线上页面。门户配置与验收命令见 [`apps/www/README.md`](../../apps/www/README.md)。

## 显式发布版本

| 渠道 | 产品 Tag | GitHub Release |
| --- | --- | --- |
| stable | `vX.Y.Z` | 正式版本，不强制占用官网的 Latest |
| nightly | `vX.Y.Z-nightly.YYYYMMDD.N` | 预发布，不标记为 Latest |
| rc | `vX.Y.Z-rc.N` | 预发布，不标记为 Latest |

数字不带前导零，日期必须有效。Tag 一经创建不可移动、删除重用或覆盖，不创建可变 `nightly` Tag，不追加 build metadata。

1. 在同一 PR 更新四个产品版本及必要的 Cargo.lock，完成检查并按授权合并。
2. 核对该完整提交的 main push CI success，再创建并推送 `v<version>`。一个产品 Tag 构建全部产品目标。
3. 核对流水线全部生产者、组包和完整汇总成功，确认唯一 publisher 公开 Release。
4. 下载并验证完整清单、包内来源、SHA-256 和实际运行结果。npm 与部署分别核对。

手动验证默认只构建，既不创建 Tag，也不公开 Release：

```sh
gh workflow run release.yml --ref <branch> -f app=product -F publish=false
```

真实发布的手动入口只能引用现存精确 Tag，例如 `--ref v<version> -f app=product -f tag=v<version> -F publish=true`。网站使用 `app=www` 与 `www-v<version>`。先查询同一 Tag、SHA 的原 run，不为一次未知响应重复派发。失败的 publisher 只消费原完整 artifact，不能为同一 Tag 重建替换字节。

## 更新渠道

npm Nightly 的制品与发布流程见下面的 npm 章节。npm 安装由 npm 更新，不使用本节的 Rust 自更新器。

Rust 更新器从 `leaperone/vibeface` 的公开 GitHub Releases 发现版本，不使用 Actions 的临时制品。Stable 只选择 `vX.Y.Z` 正式版，Nightly 只选择 `vX.Y.Z-nightly.YYYYMMDD.N` 预发布；两者都不选择 rc。更新器比较数值版本，不按 Release 创建时间或字符串排序，不自动降级。尚无正式版时，Stable 报告没有匹配版本，不回退到 Nightly。

更新器没有默认策略；先用 `update configure` 明确指定渠道、是否自动更新及安装根目录，再检查或应用更新。策略保存在数据目录的私有 `update.json`，不进入共享会话或 DAG。命令见 [选择更新渠道](node.md#选择更新渠道)。首次建立受管安装可使用 [公开安装命令](install.md)，已安装的节点使用 `update apply`。

匿名 GitHub API 请求被限流时，更新器明确报错并保留现版，自动更新在下轮再次检查。可在更新服务的运行环境提供 `GITHUB_TOKEN` 提高额度；token 只发往 `api.github.com`，不发给下载 CDN，也不写入配置或更新策略。后台服务不保证继承终端的临时环境变量。

更新器只接受受管安装的 `current` 符号链接，下载对应平台的完整包，核对 SHA256、版本、target、源码身份和存储兼容声明后再安装。包和 checksum 都通过 GitHub HTTPS 下载，这依赖 GitHub 账户、仓库和 TLS 的可信性，不是独立发布签名。macOS 程序另有 Developer ID 签名和 notarization，供系统放行，更新器不校验它们。

每小时的独立用户服务调用 `update apply --automatic`，不调用模型，不消耗模型 token。更新器先下载并校验，再请求 Node 进入维护状态。正在执行的操作、管理 Agent 调用或 `unknown` 结果会推迟升级。更新器不会为升级强行结束任务。更新只更换程序，不覆盖身份、历史、配置和凭证，也不更新 Codex、Cursor 或其他 Provider 程序。

`BUILD-INFO.json` 声明 `update_protocol=1` 和 `data_epoch=1`，两者取自共享控制 crate `crates/control/src/lib.rs` 的 `UPDATE_PROTOCOL` 和 `DATA_EPOCH`。包协议 1 的 BUILD-INFO 只接受 `version`、`target`、`source_commit`、`update_protocol`、`data_epoch`、`core_version`、`core_source_commit`、`app_version` 和 `app_source_commit` 这 9 个字段。校验涵盖同目录 CLI、Core 和完整 `web/` 的版本、源码及文件摘要。设备授权和业务记录使用单一 `node.sqlite3`，数据库 `user_version=1`，Atlas HCL 为 `apps/core/schema.hcl`。不同 epoch 的数据目录和节点不能混用。更新器只安装与本机存储 epoch 一致的包，不隐式转换或清空数据目录。安装后会重启用户服务并核验本机健康状态；失败时恢复上一份程序入口。更新器不自动恢复业务数据库，不能以程序回退代替数据恢复。网络、校验或安装错误通过命令退出状态和私有更新日志报告，不当作“没有更新”。

## 首次安装

公开安装入口为仓库根 `install.sh`。默认选择最新 Stable，显式 `--channel nightly` 选择最新 Nightly；按数值版本比较，忽略 rc 和 draft，不使用 `/releases/latest` 获取 Nightly。如果尚无正式版，Stable 安装会明确失败。命令和后续 CLI 管理见 [安装说明](install.md)。

安装器只依赖 Bash、curl、tar、gzip、系统文本工具及 `sha256sum` 或 `shasum`，不需要 jq、Python、Node.js 或 Rust。它解析公开 Releases 和 tag 对应的源码提交，下载完整包与准确同名的 SHA256，校验版本、target、源码身份、协议和 data epoch，并拒绝危险包路径、符号链接和特殊文件。校验通过后建立 `releases/<version>`、`current` 与用户 bin 链接，不覆盖已有安装、身份或服务。

`bash install.sh --self-test` 使用临时目录、模拟 GitHub 响应和测试包验证渠道、数值排序、校验拒绝及已有安装保护，不改当前用户的正式安装或服务。CI 与 preflight 均运行它。SHA256 的信任边界与更新器一致，不是独立发布签名。

## 手动建立受管安装或更换程序

以下步骤用于从完整包手动建立受管安装。数据目录的 epoch 或共享结构与程序不一致时，必须使用全新数据目录重新初始化，并重新加入同版本节点。只有新旧程序都使用最终 epoch 1 结构时，更换程序才可保留原数据目录和身份。程序不兼容或迁移旧的开发格式。

先停用旧更新定时服务，备份 `current` 目标、用户服务定义和私有启动脚本。确认维护状态空闲且无结果未知操作后，再停止 Node 用户服务并等待目录锁释放，然后备份完整数据目录。备份包括身份和历史，应保持私密。不要强行结束任务，也不要让新旧程序同时使用同一数据目录。

校验并解压新包到受管安装根目录下的独立目录，通过 `current` 符号链接选择程序。例如，先将解压后的包移动到 `$VIBEFACE_INSTALL_ROOT/releases/<version>`，再创建指向它的 `$VIBEFACE_INSTALL_ROOT/current`。保留整个包，不单独复制 Rust binary；更新器需要同包的脚本和 `BUILD-INFO.json`。

原子切换 `current` 后，建立 `$VIBEFACE_BIN_DIR/vibeface` 到 `current/vibeface` 的链接。Core 通过同一 `current` 目录发现 `vibeface-core` 和 `web/`，不能只替换 CLI。旧 CLI 链接只有在确认属于该受管安装时才可移除，不覆盖用户自有文件。使用全新或已确认属于 epoch 1 的数据目录前台启动，核对设备身份和状态，随后按 [后台运行说明](node.md#作为后台应用运行) 配置指向这一固定路径的用户服务。Linux 服务名是 `vibeface.service`，先卸载旧服务定义，避免双进程。停止前台进程后再启动用户服务。

已有私有凭证启动脚本时，保留内容和私有权限，仅将末端启动路径改为 `current/vibeface`。不要用 `service install` 覆盖凭证脚本。核对新服务的 PID、版本、设备身份、历史及本机健康状态，再恢复原更新渠道和自动更新策略。

确认新程序可用后，再执行 `update configure` 设置渠道和自动更新。普通 `cargo install` 或直接复制 binary 不构成受管安装，更新器会要求先建立完整包与 `current` 链接。

此版本不转换旧数据库，也不自动清空旧目录。需要恢复旧环境时，停止新程序，并使用旧程序及其对应的完整备份，不能让不同数据格式的程序共用目录。

## npm Nightly 制品与发布

npm 包名为 `@leaperone/vibeface`，已公开首版 `0.0.1-nightly.20261106.1`。Nightly 使用同名不可变版本和 `nightly` dist-tag。模板 `apps/cli/npm/package.json` 保持 private；打包脚本验证三个原生包后，为发布制品写入真实版本并移除 private 限制。

`scripts/package-npm.mjs` 接收三个平台的完整 `.tar.gz` 与同名 SHA256，要求三个平台的 CLI 版本、CLI 源码、锁定组件身份及包协议一致，平台字段分别匹配各自 target。它拒绝危险路径、链接、特殊文件、校验错配和缺失资源，把完整包放入 `vendor/<target>`，再生成一个含全部平台的 `.tgz`。只有一个 npm 发布身份和一次发布操作，代价是下载包含其他平台的程序。没有 postinstall、运行期下载或新增 npm 依赖。

定时和手动扩展 CI 生成 `npm-package-<SHA>-<producer_attempt>` artifact。产品 Nightly 汇总生成 `npm-package-<SHA>-<producer_attempt>`，由 `collect` 输出不可变 artifact ID。npm job 使用该 id 下载，失败后单独重跑仍使用原 producer 的制品，不按当前 run attempt 拼接名称。两种 artifact 都包含 `.tgz` 和记录源码 SHA、SHA256、npm integrity 的 JSON。CI 在三个原生平台上以 `--ignore-scripts` 全局安装到隔离 prefix，然后验证版本，并以 `--web-only` 验证启动和随包网页，不重复完整多节点验收。Stable 和 rc 延续现有 GitHub 发布流程，不生成 npm 包。

本地检查与打包命令如下。输入目录只能包含对应版本的三个包与三个 checksum，默认输出 `.artifacts/npm`，已有同名制品会被保留并报错：

```sh
node scripts/package-npm.mjs --self-test
node scripts/package-npm.mjs --input /absolute/path/to/verified-packages --source-commit <full-commit-sha>
node scripts/package-npm.mjs --verify /absolute/path/to/leaperone-vibeface-<version>.tgz --source-commit <full-commit-sha>
```

`release.yml` 在产品 Nightly 的 `publish` 成功后自动调用可复用的 `npm-publish.yml` 并公开发布。两个工作流保留同一 tag 的 `GITHUB_SHA` 和 `GITHUB_REF`，npm provenance 对应制品源码。发布前核对来源仓库、run id、源码 SHA、tag、artifact id、有效期、名称和 producer attempt，再执行 `package-npm.mjs --verify`；发布阶段不重新构建。

发布使用 GitHub-hosted Ubuntu、Node.js 24 和 npm 12.2.2。调用方与被调用方都授予 `id-token: write`，发布 job 使用 `npm-release` environment，不配置 `NODE_AUTH_TOKEN` 或长期 npm token。

[npm Trusted Publishing](https://docs.npmjs.com/trusted-publishers) 对可复用工作流验证调用方的文件名，因此自动发布须信任 `release.yml`，手动入口须另信任 `npm-publish.yml`。在 npm 自己的登录流程完成认证后，用 npm 12.2.2 为包配置两条允许发布的 trust：

```sh
npm trust github @leaperone/vibeface --repo leaperone/vibeface --file release.yml --env npm-release --allow-publish --yes
npm trust github @leaperone/vibeface --repo leaperone/vibeface --file npm-publish.yml --env npm-release --allow-publish --yes
```

GitHub 的同名 environment 及 tag 访问策略由仓库远端设置控制。仓库代码不会自动创建 npm trust，也不能证明设置已生效。保存 trust 后仍需一次真实 tag 发布确认 OIDC、provenance 和公开 registry 结果；本地自检和 dry run 不证明真实发布权限。不要把登录凭证发到聊天或写入仓库。

手动入口默认 `publish=false`，只接受与输入相同的 tag ref。它要求该 tag 和源码 SHA 只有一个成功的 `release.yml` run，以及一个有效 npm artifact；无法唯一定位时停止。该 tag 必须已经包含本工作流与脚本。dry run 命令如下：

```sh
gh workflow run npm-publish.yml --repo leaperone/vibeface --ref v<version> \
  -f tag=v<version> -F publish=false
```

需要手动公开发布时，把 `publish` 设为 true。若原发布 run 的 npm job 失败，协调器只在原产品与 npm 制品完整时尝试重跑原 run 的失败 publisher。手动恢复时，先对账后使用 `gh run rerun <run-id> --failed`，复用原 artifact id，不重建归档。

发布器先读取公开 registry，只把 HTTP 404 当作包不存在，传输失败、其他 HTTP 错误和无效响应都停止发布。已有目标版本的 integrity 必须相同，且 `nightly` 等于目标或更高时直接成功，不降级。已有版本但 `nightly` 缺失或较旧时停止并要求对账，不自动修改 dist-tag。仅目标版本不存在且比当前 Nightly 更新时执行一次 `npm publish --provenance`。随后最多进行 40 次只读查询，等待 registry 确认 integrity 和渠道；包括发布响应丢失的情况，也不自动重发。未核实公开结果始终失败。公开版本不能覆盖，不能通过复用 tag 更换内容。
