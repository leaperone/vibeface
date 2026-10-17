# 部署与使用 Node

本文使用 `vibeface` 管理自己的设备。HTTP 与同步协议见 [协议参考](node-protocol.md)。

## 安装制品

选择对应操作系统与 CPU 架构的 `vibeface-<version>-<target>.tar.gz`，先校验同名 `.sha256` 文件，再解压。

```sh
shasum -a 256 -c vibeface-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-<version>-<target>.tar.gz
cd vibeface-<version>-<target>
./vibeface --help
```

包内包括 Rust 程序、SDK sidecar 源码、文档和用户级服务安装脚本，不包含模型凭证、Provider 程序或 npm 依赖。若使用 Codex SDK 管理 Agent，先安装 Node.js 18+，然后在 `sidecars/` 运行 `npm ci --omit=dev`。仅使用 Cursor SDK 管理 Agent 时，无需 Node.js。

从源码生成包时，先构建同一 Rust 密码学模块的浏览器资源。发布包已包含资源，安装发布包不需要这些构建工具。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
bash scripts/build-web-crypto.sh
./scripts/package-node.sh
```

`--target` 指定 Rust target，仍需本机具备相应工具链。此选项不保证跨平台编译成功。`--skip-build` 使用已有 release 程序，不能用于证明程序与当前源码一致。

## 初始化首台设备

`--root` 指向允许编码执行器使用的现有绝对目录，可重复指定。省略时没有允许的执行目录。

```sh
./vibeface init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects
./vibeface serve --data-dir "$HOME/.vibeface"
```

`init` 创建本机独立密钥、配置及一次性本机接入材料，不注册网站账号，也不把个人根私钥放进 Node。用本机终端取得可信接入链接：

```sh
./vibeface access --data-dir "$HOME/.vibeface"
```

在自己信任的浏览器打开链接，核对设备指纹，为设备网络命名并保存加密恢复包。客户端独立生成个人根身份和自己的设备身份，签署首个成员目录。Node 验证一次性本机授权后安装该目录，日常连接使用客户端设备密钥。

不要公开接入链接。其中的授权材料用于初始化或加入当前设备。后续连接自动核验设备指纹和成员授权，不输入账号密码。个人根私钥留在可信客户端及加密恢复包，不作为节点间的长期共享密码。

默认只监听本机。`http://127.0.0.1:4790`、`http://localhost:4790` 和本机字面地址 `http://0.0.0.0:4790` 可用于打开服务。保存密钥的网页客户端必须处于浏览器认可的安全上下文，推荐使用 `http://localhost:4790` 或可信 HTTPS 页面。

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含本机身份、配置、私有授权记录和 `node.sqlite3`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

## 配置管理 Agent

管理 Agent 与执行器分开配置。管理 Agent 理解用户指令、使用 Node 工具创建工单和派发任务；执行器在目标设备的项目目录中工作。管理工作目录应单独建立，不作为项目编辑入口。

初始化时选择一种管理 SDK。已有 Node 先停止进程，再修改 `config.json` 的 `manager` 并重启，保留原身份和数据库。字段说明见 [本机配置](node-protocol.md#本机配置)。

### 使用 Cursor SDK

从 [Cursor SDK Bridge v1.0.37 发布页](https://github.com/cursor/sdk-bridge/releases/tag/v1.0.37) 下载对应平台的 standalone 包，用发布页的校验文件核对后解压。可执行文件位于 `bin/cursor-sdk-bridge`。例如，Apple Silicon Mac 的包名是 `cursor-sdk-bridge-standalone-darwin-arm64.tar.gz`。

在运行 Node 的环境中配置 `CURSOR_API_KEY`。不要把密钥写进命令行、URL、Git 或聊天记录。模型使用账户实际可用的名称。

```sh
./vibeface init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects \
	--manager cursor_sdk \
	--workspace /absolute/path/to/manager-workspace \
	--model auto \
	--cursor-sdk-bin /absolute/path/to/bin/cursor-sdk-bridge
```

此命令用于尚未初始化的 Node。默认使用 `auto` 模型。SDK 程序在本机运行，模型请求仍发往 Cursor 服务，消耗 Cursor 额度。

Cursor 使用 SDK 原生自定义工具回调连接 Rust Node。产品链路不启动 MCP Server，也不要求 Bot 或浏览器支持 MCP。管理 Agent 只能使用限定到当前个人网络及业务会话的工具，不能替本人回答编码审批、授权自动后续或验收工单。

### 使用 Codex SDK

Codex 官方 SDK 是 TypeScript 库。Rust Node 通过随包 sidecar 调用它，需要 Node.js 18+、已安装的锁定 npm 依赖和已登录的 Codex CLI。

```sh
./vibeface init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects \
	--manager codex_sdk \
	--workspace /absolute/path/to/manager-workspace \
	--node-bin /absolute/path/to/node \
	--codex-sdk-script /absolute/path/to/sidecars/codex-manager.mjs
```

可用 `--model` 指定模型，省略时使用 Codex 配置中的默认模型。源码运行时先在 `crates/node/sdk/` 安装依赖，将 `--codex-sdk-script` 指向该目录的入口。

这个适配器通过本机 `vibeface tool` 命令调用受限工具，使用只读文件系统权限，保留 shell 和网络访问。Codex 子进程环境按变量名筛选，shell 只获得运行及当前会话工具所需的变量；Provider 凭证不放进 shell 环境。它没有 shell 命令白名单或已验证的外部域名隔离，不能当作隔离不可信模型或不可信插件的安全边界。Cursor SDK 与 Codex SDK 的工具运行方式不同，不承诺完全相同的权限隔离。

## 添加自己的设备

每个 Node 和每个客户端有独立密钥。本人设备列表来自已签名的成员目录，设备名称、IP 和中继返回的地址不证明所有权。

在已授权管理客户端选择**添加设备**，选择 Node 或客户端及其权限。将短期邀请通过可信途径交给新设备，核对个人网络和邀请端的完整指纹。新设备独立生成密钥并申请加入。管理客户端核对待加入的名称和指纹，再明确确认。

添加另一台 Node 时，先在新设备用独立数据目录运行 `init`，再把完整邀请保存为私有 JSON 文件。邀请文件包含接入地址、钉住的 NodeID、签名邀请和一次性 capability，不公开分享。

```sh
./vibeface pair \
	--data-dir "$HOME/.vibeface" \
	--invite-file /private/path/to/invitation.json \
	--name "My Linux Node"
```

命令提交新设备持钥证明后等待管理客户端确认。核对名称、完整指纹和角色后批准，命令显示配对完成，再启动新 Node。连接中断时使用原邀请、目录和名称重试，CLI 保留待确认请求身份，不另建授权。

Node 不代替客户端签署授权。Agent 不能添加设备、扩大权限或撤销设备。新设备加入后可同步本人已授权的上下文，在设备列表中显示名称、指纹、角色和在线状态。

网页的**我的入口**可保存直连地址或公共中继地址。更换入口后使用同一客户端身份认证目标 Node，再补齐业务历史。跨 origin 的浏览器存储互相隔离，使用固定可信客户端来源可保留自己的设备密钥。

## 开放局域网入口

`listen` 决定实际监听网卡，`public_url` 是展示使用的主入口，`allowed_origins` 是额外允许的网页入口。修改以下字段并保留其他配置，然后重启：

```json
{
	"listen": "0.0.0.0:4790",
	"public_url": "http://192.168.1.10:4790",
	"allowed_origins": [],
	"allow_insecure_lan": true
}
```

将示例 IP 替换为本机 LAN 地址，并在防火墙允许入站连接。只在本机使用时，保留默认回环监听，不必开放 LAN 或公网。

手机使用实际 LAN IP，不能用 `0.0.0.0` 作为远程地址。本机别名仍可访问。额外 IP 或域名加入 `allowed_origins`。初始化时也可重复传入 `--allowed-origin`。

所有 origin 必须是完整的 HTTP 或 HTTPS 地址，不含路径、查询参数或凭证。同一主机和端口不能同时配置 HTTP 与 HTTPS。Node 检查请求 Host 和 Origin，允许一个地址不会自动开放端口或防火墙。

局域网 HTTP 页面能打开，不代表它能安全保存客户端私钥。浏览器不把普通 LAN HTTP 当作安全上下文。使用自己的可信 HTTPS 客户端或本机 localhost 页面连接 Node，不降低设备认证。HTTPS 页面访问远端时需要 HTTPS/WSS 入口，可通过可信的 HTTPS 中继建立该路径。端到端 Noise 保护业务通道，HTTPS 还保护网页代码的下载来源。

## 发现与公共中继

Node 可启用局域网发现，从候选地址找到已知 NodeID。公网发现由可替换的会合节点提供。发现记录带设备签名和过期时间，只说明某个设备声明了路径，连接时仍需核验身份及个人成员授权。

公网或可达主机运行同一个 Node，也可启用调度或执行能力。启用公共中继时，其他人的设备可预约出站通道并通过它连通。中继只转发端到端密文，不加入对方个人网络，不获得会话、项目目录或执行权限。

网络配置如下，保留其他字段：

```json
{
	"network": {
		"bootstrap_urls": ["wss://relay.example/api/network/ws"],
		"advertise_urls": ["wss://my-node.example/api/network/ws"],
		"mdns": true,
		"relay_enabled": false
	}
}
```

`bootstrap_urls` 可以配置多个已知会合入口。没有公网直连地址的设备可保持该列表，省略对外直连地址。提供公共中继时明确开启 `relay_enabled`，并按协议限制容量和流量。发现或中继服务不可达时换路径，不重做业务操作。

公网网页来源使用 HTTPS，WebSocket 使用 WSS。Node 本身可绑定回环 HTTP，由反向代理终止 TLS。代理保留公网 Host 与原始 Origin，并支持 `/api/network/ws` 的 WebSocket upgrade。中继不是可信网页发行者，使用本人信任的客户端代码来源。

## 提交任务与处理结果

连接自己的设备后，新建会话，选择可用的调度设备，发送目标。也可先在“工单”中填写目标和验收条件，再让管理 Agent 安排工作。在“执行会话”中打开指定 Node 上的执行器，查看原始进度、审批与操作记录。

Codex 执行器支持打开、读取、发送、调整当前 turn、中断和回答原请求。Claude Code 与 Cursor CLI 只支持打开和发送，其他操作明确拒绝。CLI 遇到需要交互审批的工作不会自动放行；需要完整审批往返时使用 Codex。

Node 默认尝试启动与本机身份关联的私有 `codex app-server`，socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix socket 路径长度限制。要连接已有实例，在 `config.json` 设置 `codex_socket`。`codex_bin`、`claude_bin`、`cursor_bin` 指向对应程序。启动网页成功不等于程序已安装、已登录或模型可用，应查看执行会话的实际状态。

勾选“完成执行后，让调度 Agent 继续检查”会授予该工单自动后续权限。普通内容片段和进度不调用管理模型；需要自动决策时才调用。`daily_decision_limit` 按个人网络限制当前 Node 的 UTC 每日管理调用次数，失败调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。

编码执行器结束一轮后，管理 Agent 可将工单置为 `needs_review`，本人核对产物及验收条件后才设为 `accepted`。审批也必须本人回答，并对应仍有效的原执行请求。

已验收或取消的工单不能由 Agent 重新打开，也不能省略工单关联后继续派发。本人可以明确重新打开工单，或把同一执行会话用于新的活动工单。这不会自动停止已经运行的进程。

“通知”保存每个客户端尚未确认的消息。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与权限。关闭网页后没有独立的手机推送服务。

## 核对未知结果

网络断开或进程中断后，写操作可能已经执行，但回执未保存。Node 将这种情况保留为 `unknown`，不自动重发，也不释放该操作占用的执行资源。

先在“执行会话”查看操作记录，使用“核对执行结果”查询原执行 Node。若仍无法判断，检查实际进程、项目产物和外部副作用，再附证据确认成功或失败。人工确认只解决原账本记录，不重放原操作。失联不能证明远端进程已停止。

A 已派发 B 后掉线时，从 C 打开原业务会话，等待 DAG 补齐，再核对 B 的实际执行状态。缺失历史、执行器不可达和结果未知分别显示，不能通过创建同名新任务代替恢复。

同一 Node 检查会话版本，并对规范化工作目录及父子目录冲突进行本机互斥。不同 Node 之间没有全局文件锁。若两台设备挂载同一可写目录，使用独立 worktree 或独立根目录隔离。

浏览器在当前入口的本地存储中保留尚未确认的消息正文、消息编号和原调度节点，确认后删除。刷新后重试须使用原内容和原目标；改写正文会被拒绝，避免把新目标误当成旧操作重试。

## 作为后台应用运行

`vibeface` 是节点主进程，也是本机管理 CLI。先用前台 `serve` 确认配置，再按 Ctrl-C 停止前台进程，安装用户级服务：

```sh
vibeface service install --data-dir "$HOME/.vibeface"
vibeface status --data-dir "$HOME/.vibeface"
vibeface service status --data-dir "$HOME/.vibeface"
```

服务管理命令需要完整的受管安装，启动路径始终为安装根目录下的 `current/vibeface`。通过 `~/.local/bin/vibeface` 链接调用也保留该路径，升级后不会固定在旧 release。直接运行 `cargo build` 的程序不安装服务。自定义安装根先使用 `update configure --install-root /absolute/path` 保存路径。

每个系统用户管理一个 Node 服务。各服务命令核对 `--data-dir` 与已安装服务一致，再操作该服务。省略时使用 `$HOME/.vibeface`。管理其他数据目录时显式传入绝对路径。

`status` 通过私有维护 socket 检查运行中的 Node，并核对 NodeID，不要求安装服务。`status --json` 输出状态、NodeID、监听地址、角色及运行版本，不输出接入授权材料。已停止或无法连接时返回非零退出码。`service status` 则展示 launchd 或 systemd 的 Node 和更新服务状态。

```sh
vibeface status --json --data-dir "$HOME/.vibeface"
vibeface service stop --data-dir "$HOME/.vibeface"
vibeface service start --data-dir "$HOME/.vibeface"
vibeface service restart --data-dir "$HOME/.vibeface"
```

macOS 使用当前登录用户的 launchd agent，日志位于数据目录的 `service.stdout.log` 和 `service.stderr.log`。Linux 使用 `systemd --user`，Node 日志由用户 journal 保存。退出登录后仍需运行的 Linux 主机，由管理员按主机策略启用该用户的 lingering。

后台服务不保证继承终端的 PATH 或临时环境变量。在 `config.json` 使用 Provider 和 sidecar 的绝对路径，并通过对应服务管理器配置模型凭证。安装脚本不保存或复制密钥。

卸载同时移除 Node 服务和更新定时服务，不删除身份、配置和历史：

```sh
vibeface service uninstall --data-dir "$HOME/.vibeface"
```

## 选择更新渠道

自动更新需要完整发布包与受管安装目录。固定的 `current` 符号链接指向当前包，Node 用户服务也通过该路径启动。首次安装方式见 [构建与发布](releasing.md#首次安装更新器或手动更换程序)。

选择 Stable 并开启自动更新：

```sh
"$HOME/.local/share/vibeface/current/vibeface" update configure \
	--data-dir "$HOME/.vibeface" \
	--install-root "$HOME/.local/share/vibeface" \
	--channel stable \
	--automatic true
```

将 `stable` 改为 `nightly` 可跟随开发预发布。Stable 不会安装 Nightly；尚无正式版时不会自动更换版本。切换渠道不触发降级。默认渠道为 Stable，未配置时不开启自动更新；已有 Nightly 安装应显式选择 `nightly`。

查看策略和最近结果、主动检查或立即尝试升级：

```sh
"$HOME/.local/share/vibeface/current/vibeface" update status --data-dir "$HOME/.vibeface"
"$HOME/.local/share/vibeface/current/vibeface" update check --data-dir "$HOME/.vibeface"
"$HOME/.local/share/vibeface/current/vibeface" update apply --data-dir "$HOME/.vibeface"
```

`check` 只查询与校验版本信息。`apply` 仅安装声明与当前数据格式兼容的包，需要安全的空闲窗口。活动任务、管理 Agent 调用和结果未知的操作都会阻止安装，先完成或核对原任务，不需要关闭网页。升级不会调用模型或修改 Provider 密钥。

开启自动更新会安装独立的用户定时服务。macOS 的 `com.leaperone.vibeface.update` 在载入时和每小时尝试更新；Linux 的 `vibeface-update.timer` 每小时启动一次独立 oneshot 服务。Linux 关机期间不能更新，用户服务能否在退出登录后运行取决于 lingering。更新日志位于数据目录的 `update.stdout.log` 与 `update.stderr.log`，文件权限为 600。`vibeface service status` 同时展示 Node 与更新服务的真实状态。

关闭自动更新时，保留所选渠道与受管路径，重新执行 `configure`：

```sh
"$HOME/.local/share/vibeface/current/vibeface" update configure \
	--data-dir "$HOME/.vibeface" \
	--install-root "$HOME/.local/share/vibeface" \
	--channel stable \
	--automatic false
```

这会移除更新定时服务，手动 `check` 和 `apply` 仍可使用。`vibeface service install-updater` 和 `uninstall-updater` 可单独安装或移除定时服务，不改变已保存的更新策略。策略的 `automatic` 为 `false` 时，即使手动安装定时服务也不会自动更新。重新配置自动更新会重新安装或移除定时服务。

更新器先核对包和存储兼容声明，再停止 Node 并切换 `current`。新进程健康检查失败时只恢复上一份程序入口，不自动还原数据库；会话、身份、配置和凭证仍保存在原数据目录。校验、网络和安装失败可通过 `update status` 与更新日志查看。

## 备份与恢复

在可信管理客户端导出加密恢复包，选择足够强且独立保存的密码。密码只保护恢复材料，不发送给 Node，不作为节点连接密码。

恢复包用于恢复个人根身份、必要的解密材料及建立新的管理授权。客户端恢复时生成新的设备密钥，不复制丢失设备的日常身份。恢复记录使用更高的授权 epoch，获知恢复记录的 Node 拒绝过期授权。

恢复根身份不会自动找回聊天正文、工程文件或执行器隐藏状态。业务历史仍需可达的授权副本或独立数据备份。缺少材料时明确停止，不创建看似相同的新身份替代原网络。

在**我的设备**中移除丢失设备。收到签名撤销记录的端点拒绝后续操作，已建立会话也重新验权。离线分区收到记录前不能立即生效，撤销不能抹除对方已经读过的数据，也不强制停止已运行的外部进程。

停掉 Node 后备份完整数据目录，保留 SQLite 数据库及未合并的 WAL。模型 key、本机私钥和 SDK session 留在本机，不进入共享 DAG。完整目录备份含敏感信息，按本机凭证保管。不要复制运行中的 SQLite 文件来实现节点同步。

业务存储使用 `rusqlite`，没有 ORM。schema 和 Atlas 校验清单位于 `crates/node/migrations/`。Atlas 只管理 SQL 文件，不负责节点同步。数据格式 epoch 为 `2`，启动和更新都不得隐式转换或清空不兼容的数据目录。监控历史正文、附件和服务日志的磁盘占用。
