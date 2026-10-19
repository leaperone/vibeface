# Vibeface

在自己的 Mac 或 Linux 设备上运行 `vibeface`，创建有名称、职责和项目配置的机器人，直接在它的聊天中提出要求。协调 Agent 用宿主 SDK 的 Shell、文件工具和 Codex App Server 完成本机工作，也能向另一台设备的 Agent 发任务。机器人定义、聊天历史和任务结果在本人授权的节点间同步，换一个入口也能查询原来的工作。

```mermaid
flowchart LR
    U[本人的网页或手机浏览器]
    G[PVE Node<br>网页与公共中继]
    A[Mac mini Node<br>协调 Agent]
    B[Node B<br>协调 Agent 与本机工具]
    C[Node C<br>协调 Agent 与历史副本]
    U -->|HTTPS| G
    U -->|LAN 直连| A
    U -->|切换入口| C
    A <-->|出站 WebSocket| G
    B <-->|出站 WebSocket| G
    C <-->|出站 WebSocket| G
```

所有设备运行同一种 Node。公网 Server 是 Node 的一种部署方式，可以按需启用 Agent，也可以只提供网页和公共中继。纯中继不加入个人集群。每个人拥有独立的集群，程序为每次请求选择已授权的可用节点，并固定已投递任务的目标。

网页使用 React 和 TanStack Start 构建静态 SPA，随 Rust 程序内嵌分发，生产运行不需要 Node.js Web 服务。可信网页通过设备密钥和 Noise 通道连接指定 Node。

Rust 核心负责认证、路由、DAG 同步、SQLite 存储、任务调度、去重和通知。Cursor SDK 聊天 Agent 通过原生自定义工具回调协调 Node，不需要 MCP。宿主命令和文件操作直接使用 SDK 工具；Codex 使用薄原生客户端访问 App Server，不经过独立执行适配层。机器人定义和共享聊天历史独立于本机 SDK 缓存，创建机器人不会启动模型。

本版本使用新的机器人和数据库结构，部署时使用全新的数据目录重新初始化。旧数据库不自动升级，集群中的节点使用同一新版本。

## 安装与开始

主程序是 Rust 二进制 CLI `vibeface`，支持 macOS Apple Silicon、Linux x64 和 Linux ARM64（glibc）。npm 安装入口需要 Node.js 22+，网页仍由 Rust 程序提供。

npm Nightly 分发已配置，包尚未公开发布。首次发布后使用：

```sh
npm install -g @leaperone/vibeface@nightly
vibeface --version
```

npm 包包含完整平台程序、SDK sidecar 和服务脚本，不通过 postinstall 下载程序，不初始化身份或启动服务。发布前可从 PR 的 CI 下载 `.tgz`，在隔离 prefix 中安装验收。当前尚无正式版，GitHub 完整包安装入口见 [安装说明](docs/install.md)。

安装后直接启动：

```sh
vibeface
```

首次运行仅在不存在或为空的数据目录中初始化，默认使用 `~/.vibeface` 并监听 `127.0.0.1:4790`，不授予执行目录或启用管理 Agent。同一数据格式的节点重启保留原身份和配置。npm 安装由 npm 更新；后台节点先确认空闲并停止服务，再更新、重启和核对状态。GitHub 受管安装仍使用 `vibeface update apply`。安装、更新与 CLI 管理见 [安装说明](docs/install.md)。

## 从源码开始

从源码构建需要 Rust 1.89+、Node.js 22.12+、pnpm 11.9.0 和 wasm-bindgen-cli 0.2.129。运行任务前，安装并登录需要的 SDK 和 Codex CLI。

```sh
rustup target add wasm32-unknown-unknown
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
cargo build --release -p vibeface --locked
./target/release/vibeface init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects
./target/release/vibeface serve --data-dir "$HOME/.vibeface"
```

`init` 要求数据目录为空或不存在，默认是 `$HOME/.vibeface`。它写入 `identity.json`、`config.json`、一次性 `bootstrap.json`、`data-epoch.json` 和单一 `node.sqlite3`，设备授权与业务记录使用同一个 Store。数据格式 epoch 为 6，数据库 `user_version` 为 6。不注册网站账号，也不保存个人根私钥。`serve` 和 `pair` 用目录锁独占该目录。

```sh
./target/release/vibeface access --data-dir "$HOME/.vibeface"
```

首次在自己信任的浏览器打开 `access` 打印的链接，填写设备名称，核对集群指纹，并保存加密恢复包。保存密钥的页面需要安全上下文。集群建立后，默认开启的本机自动访问会通过独立回环入口建立本机会话，不必重复配对；可在网页或 `config.json` 关闭。公网访问仍验证设备身份。创建机器人后进入它的长期聊天，设备、连接和恢复放在设置中。聊天调度需要另外配置 [管理 Agent](docs/node.md#配置管理-agent)。已启用 Agent 的 Node 可以启动私有 Codex App Server；网页可用不代表 Codex 已安装、登录或模型可用。

PVE 只提供网页和中继时，使用 `init --relay-only`，然后让 Mac mini 的 `network.bootstrap_urls` 指向它。在手机上打开可信网页，导入 Mac mini 的连接信息并完成设备配对，实际认证目标是 Mac mini。见 [纯 Web 与 Relay](docs/node.md#纯-web-与-relay)。

已配对的浏览器也可在连接页选择“按集群连接”，填写 Cluster ID 和已知发现服务。集群编号保持不变，浏览器会重新发现并认证可用的 Gateway。入口切换只恢复读取与订阅，未确认写操作仍绑定原入口，不会自动重发。

另一台 Node 使用独立的空数据目录先 `init`。管理客户端复制的 base64url 配对信息传给 `pair --invite`。`pair --invite-file` 读取解码后的 JSON 邀请，不是这段原文。不要用新建集群代替配对。操作步骤见 [添加自己的设备](docs/node.md#添加自己的设备)。

## 接续工作与通知

A 把任务派发到 B 后，B 在接单前取得所需上下文。A 离线时，可以在已同步的 C 查询会话，由 C 核对 B 的实际状态，再使用 C 自己的管理 Agent 回答。缺失且不可达的历史会阻止继续执行，系统不会用摘要猜测缺失内容。

普通进度和通知不调用管理模型。B 完成后保存结果和通知，通过可达节点同步到当前入口；客户端离线时保留未读通知，重连后补发。浏览器关闭后的系统推送不在当前范围内。

一次 Codex turn 结束不等于用户目标已经达成。审批仍由本人决定。写操作结果未知时，在原聊天中核查实际结果，不换一个 ID 或设备重新派发。

## 安装、协议与验证

- [首次安装与 CLI 管理](docs/install.md) 包括 npm Nightly、GitHub 完整包、初始化、状态和后台服务命令。
- [部署与使用 Node](docs/node.md) 包括局域网、公网中继、SDK、后台运行和制品安装。
- [Node 协议参考](docs/node-protocol.md) 包括 HTTP、事件 DAG、任务消息、原生状态和通知确认。
- [构建与发布](docs/releasing.md) 说明 Linux x64（`x86_64-unknown-linux-gnu`）、Linux ARM64（`aarch64-unknown-linux-gnu`）和 macOS Apple Silicon（`aarch64-apple-darwin`）三个包、SHA256、Stable 与 Nightly 更新渠道，以及 stable/nightly/rc tag 规则。PR 与 `main` 的 [CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml) 只生成 Actions 制品；公开版本见 [Releases](https://github.com/leaperone/vibeface/releases)，nightly 与 rc 为预发布。
- [选择更新渠道](docs/node.md#选择更新渠道) 适用于 GitHub 受管安装，可开启空闲时的后台升级；npm 安装由 npm 更新。
- `scripts/package-node.sh` 生成当前平台的压缩包和 SHA256 文件，默认输出到 `.artifacts/packages/`。

可在仓库根目录运行对应检查。命令本身不代表本次提交已经通过，实际结果以 PR 的验证记录为准。

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

当前没有原生手机 App。模型额度和计费由各 Provider 决定，每日决策次数限制不是 token 或费用上限。
