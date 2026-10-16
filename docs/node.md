# 部署与使用 Node

本文使用 `vibeface-node` 管理自己的设备。HTTP 与同步协议见 [协议参考](node-protocol.md)。

## 安装制品

选择对应操作系统与 CPU 架构的 `vibeface-node-<version>-<target>.tar.gz`，先校验同名 `.sha256` 文件，再解压。

```sh
shasum -a 256 -c vibeface-node-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-node-<version>-<target>.tar.gz
cd vibeface-node-<version>-<target>
./vibeface-node --help
```

包内包括 Rust 程序、SDK sidecar 源码、文档和用户级服务安装脚本，不包含模型凭证、Provider 程序或 npm 依赖。若使用 Codex SDK 管理 Agent，先安装 Node.js 18+，然后在 `sidecars/` 运行 `npm ci --omit=dev`。仅使用 Cursor SDK 管理 Agent 时，无需 Node.js。

从源码生成包：

```sh
./scripts/package-node.sh
```

`--target` 指定 Rust target，仍需本机具备相应工具链。此选项不保证跨平台编译成功。`--skip-build` 使用已有 release 程序，不能用于证明程序与当前源码一致。

## 初始化首台设备

`--root` 指向允许编码执行器使用的现有绝对目录，可重复指定。省略时没有允许的执行目录。

```sh
./vibeface-node init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects
./vibeface-node serve --data-dir "$HOME/.vibeface"
```

打开 `http://127.0.0.1:4790`，选择“创建账户”，随后登录。用户名使用 3 至 64 个 ASCII 字母、数字、点、短横线或下划线，大小写不区分。密码为 12 至 1024 字节。

注册创建一个新的个人网络。只有尚无账户、监听地址与 `public_url` 都是回环地址时，首个账户才获得本机已配置的调度和执行角色。先在本机完成这一步，再开放 LAN 或公网；其他注册账户只获得接入、中继和副本角色，不能使用服务器的项目目录或执行器。

首台注册设备保管该网络的根签名私钥，配对邀请和设备撤销只能在这台设备发起。这是设备信任的签发位置，不是会话数据的永久主节点，也不是调度负责人。

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含 `identity.json`、`config.json`、`auth.sqlite3`、`node.sqlite3` 和运行状态。使用独立目录运行不同 Node。同一目录同时只允许运行一个进程。

## 配置管理 Agent

管理 Agent 与执行器分开配置。管理 Agent 理解用户指令、使用 Node 工具创建工单和派发任务；执行器在目标设备的项目目录中工作。管理工作目录应单独建立，不作为项目编辑入口。

初始化时选择一种管理 SDK。已有 Node 先停止进程，再修改 `config.json` 的 `manager` 并重启，保留原身份和数据库。字段说明见 [本机配置](node-protocol.md#本机配置)。

### 使用 Cursor SDK

从 [Cursor SDK Bridge v1.0.37 发布页](https://github.com/cursor/sdk-bridge/releases/tag/v1.0.37) 下载对应平台的 standalone 包，用发布页的校验文件核对后解压。可执行文件位于 `bin/cursor-sdk-bridge`。例如，Apple Silicon Mac 的包名是 `cursor-sdk-bridge-standalone-darwin-arm64.tar.gz`。

在运行 Node 的环境中配置 `CURSOR_API_KEY`。不要把密钥写进命令行、URL、Git 或聊天记录。模型使用账户实际可用的名称。

```sh
./vibeface-node init \
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
./vibeface-node init \
	--data-dir "$HOME/.vibeface" \
	--root /absolute/path/to/projects \
	--manager codex_sdk \
	--workspace /absolute/path/to/manager-workspace \
	--node-bin /absolute/path/to/node \
	--codex-sdk-script /absolute/path/to/sidecars/codex-manager.mjs
```

可用 `--model` 指定模型，省略时使用 Codex 配置中的默认模型。源码运行时先在 `crates/node/sdk/` 安装依赖，将 `--codex-sdk-script` 指向该目录的入口。

这个适配器通过本机 `vibeface-node tool` 命令调用受限工具，使用只读文件系统权限，保留 shell 和网络访问。Codex 子进程环境按变量名筛选，shell 只获得运行及当前会话工具所需的变量；Provider 凭证不放进 shell 环境。它没有 shell 命令白名单或已验证的外部域名隔离，不能当作隔离不可信模型或不可信插件的安全边界。Cursor SDK 与 Codex SDK 的工具运行方式不同，不承诺完全相同的权限隔离。

## 连接自己的设备

先保证两台设备之间有可达地址。局域网设置见下一节。第二台设备先运行 `init`，此时不要在网页重新注册网络。

1. 登录首台注册设备，在“设备”中选择新设备角色并生成配对码。
2. 核对邀请中的完整 `network_id` 指纹，通过可信途径交给第二台设备。
3. 在第二台设备执行网页给出的 `pair` 命令。`--username` 和标准输入中的密码用于安装第二台设备的本地登录账户，密码不会交给远端节点。

```sh
./vibeface-node pair \
	--data-dir "$HOME/.vibeface" \
	--url https://your-node.example \
	--network-id NETWORK_FINGERPRINT \
	--code INVITE_CODE \
	--username your_name \
	--password-stdin
```

通过密码管理器把本地密码送入标准输入，再执行这条命令。直接在终端输入标准输入不会自动隐藏字符。不要把真实密码填进命令参数或 shell 历史。配对码有效期为 10 分钟，用于授权本人的新设备，不能公开分享。

`pair` 校验网络指纹、根签名和新设备证书，安装本地账户，并添加指向邀请入口的 `/peer` WebSocket 连接。完成后重启第二台 Node，登录第二台入口即可查看同步的数据。此入口的密码可以与首台入口不同。

网页“我的入口”保存此浏览器使用的多个地址。切换地址会携带非秘密的业务会话 ID 和已知历史头部；目标入口仍需独立登录，不共享跨 origin cookie。配对设备后应登录已安装的账户，重新注册会建立另一个个人网络。

## 开放可信局域网入口

先在回环地址初始化并注册本人账户，再停止 Node，修改 `config.json` 的以下字段，保留其他配置：

```json
{
	"listen": "0.0.0.0:4790",
	"public_url": "http://192.168.1.10:4790",
	"allow_insecure_lan": true
}
```

将示例 IP 替换为本机 LAN 地址，保存后重启，不重新初始化。防火墙需要允许入站连接。通过明文 LAN 配对的新设备，也需在自己的配置中开启 `allow_insecure_lan`；该设备仍可只监听回环地址，通过出站 WebSocket 接收请求。

Node 校验请求的 `Host` 和 `Origin` 是否匹配 `public_url`。`0.0.0.0` 是监听地址，不是浏览器入口。通过其他 IP 或域名访问同一监听端口会被拒绝。

HTTP 没有传输加密。密码、会话 cookie 和配对信息可能被监听；bearer token 也不能替代加密。此模式只用于明确受信任的局域网，不发布到公网。

## 通过公网节点中继

公网入口也运行同一个 `vibeface-node`。若该主机要为本人运行管理 Agent 或执行器，先在回环入口完成首个账户注册，再开放公网。公网注册的新账户不能调用这台主机的 Agent 和项目目录，需要配对本人的调度及执行设备。

在公网主机设置 `public_url` 为 HTTPS origin，通过反向代理提供 TLS。Node 本身监听 HTTP，推荐绑定回环地址，让反向代理访问。

反向代理保留公网 `Host`，转发原始 `Origin`，支持 `/peer` 的 WebSocket upgrade 和 `/api/events` 的 SSE 长连接。不能把 `Host` 改成内部 `127.0.0.1:4790`，也不能为 SSE 启用响应缓冲。`public_url` 不包含路径、查询参数或凭证。

NAT 后的设备通过出站 WebSocket 连接公网 Node，连接建立后双方都可发请求，不需要为每台执行设备开放入站端口。Node 可经多个已授权中继到达目标设备。通信网络允许有环，消息带稳定身份与去重信息。

配对会添加一个 peer。要添加另一条已授权路径，在 `config.json` 的 `peers` 中填写同一网络的入口：

```json
{
	"peers": [
		{
			"network_id": "NETWORK_FINGERPRINT",
			"url": "wss://your-node.example/peer"
		}
	]
}
```

这是配置片段，保留其他字段，修改后重启。更换地址不会改变 Node ID。中继和副本必须是本人明确授权的可信节点。签名校验来源与完整性，不能阻止被授权副本读取它持有的内容。

公网账号注册会在当前 Node 上创建新的个人网络。把现有网络接到公网 Node 应使用配对，不通过再次注册合并网络。

## 提交任务与处理结果

登录网页后，新建会话，选择可用的调度设备，发送目标。也可先在“工单”中填写目标和验收条件，再让管理 Agent 安排工作。在“执行会话”中打开指定 Node 上的执行器，查看原始进度、审批与操作记录。

Codex 执行器支持打开、读取、发送、调整当前 turn、中断和回答原请求。Claude Code 与 Cursor CLI 只支持打开和发送，其他操作明确拒绝。CLI 遇到需要交互审批的工作不会自动放行；需要完整审批往返时使用 Codex。

Node 默认尝试启动与本机身份关联的私有 `codex app-server`，socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix socket 路径长度限制。要连接已有实例，在 `config.json` 设置 `codex_socket`。`codex_bin`、`claude_bin`、`cursor_bin` 指向对应程序。启动网页成功不等于程序已安装、已登录或模型可用，应查看执行会话的实际状态。

勾选“完成执行后，让调度 Agent 继续检查”会授予该工单自动后续权限。普通内容片段和进度不调用管理模型；需要自动决策时才调用。`daily_decision_limit` 按个人网络限制当前 Node 的 UTC 每日管理调用次数，失败调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。

编码执行器结束一轮后，管理 Agent 可将工单置为 `needs_review`，本人核对产物及验收条件后才设为 `accepted`。审批也必须本人回答，并对应仍有效的原执行请求。

“通知”保存每个客户端尚未确认的消息。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与权限。关闭网页后没有独立的手机推送服务。

## 核对未知结果

网络断开或进程中断后，写操作可能已经执行，但回执未保存。Node 将这种情况保留为 `unknown`，不自动重发，也不释放该操作占用的执行资源。

先在“执行会话”查看操作记录，使用“核对执行结果”查询原执行 Node。若仍无法判断，检查实际进程、项目产物和外部副作用，再附证据确认成功或失败。人工确认只解决原账本记录，不重放原操作。失联不能证明远端进程已停止。

A 已派发 B 后掉线时，从 C 打开原业务会话，等待 DAG 补齐，再核对 B 的实际执行状态。缺失历史、执行器不可达和结果未知分别显示，不能通过创建同名新任务代替恢复。

同一 Node 检查会话版本，并对规范化工作目录及父子目录冲突进行本机互斥。不同 Node 之间没有全局文件锁。若两台设备挂载同一可写目录，使用独立 worktree 或独立根目录隔离。

浏览器在当前入口的本地存储中保留尚未确认的消息正文、消息编号和原调度节点，确认后删除。刷新后重试须使用原内容和原目标；改写正文会被拒绝，避免把新目标误当成旧操作重试。

## 作为后台应用运行

先用前台 `serve` 确认配置，再安装用户级服务。路径必须为绝对路径：

```sh
./scripts/install-node-service.sh install \
	--binary /absolute/path/to/vibeface-node \
	--data-dir "$HOME/.vibeface"
./scripts/install-node-service.sh status
```

macOS 使用当前登录用户的 launchd agent，日志位于数据目录的 `service.stdout.log` 和 `service.stderr.log`。Linux 使用 `systemd --user`。退出登录后仍需运行的 Linux 主机，由管理员按主机策略启用该用户的 lingering。

后台服务不保证继承终端的 PATH 或临时环境变量。在 `config.json` 使用 Provider 和 sidecar 的绝对路径，并通过对应服务管理器配置模型凭证。安装脚本不保存或复制密钥。

卸载只移除服务，不删除账户、身份和历史：

```sh
./scripts/install-node-service.sh uninstall
```

## 备份与维护

数据目录默认使用私有文件权限。模型 key、Node 私钥、账号密码哈希和 SDK session 留在本机，不进入共享 DAG。备份包含敏感信息，按本机凭证保管。

停掉 Node 后备份整个数据目录，保留 SQLite 数据库及尚未合并的 WAL。不要通过复制运行中的 SQLite 文件实现节点同步，也不要直接修改数据库来修复状态。

业务存储使用 `rusqlite`，没有 ORM。初始 schema 和 Atlas 校验清单位于 `crates/node/migrations/`，首次启动嵌入应用初始 SQL。当前按全新应用处理，不读取旧版 Bridge 数据。Atlas 管理 SQL 文件，不负责节点同步。

首版没有自动清理被历史引用的正文或附件。监控数据目录和服务日志的磁盘占用。根私钥只在首台注册设备；丢失该设备及其备份会失去新增配对和撤销能力，不能从普通同步副本恢复该私钥。

撤销信息传播后，节点拒绝该设备的新操作。离线设备收到更新前无法立即知道撤销，撤销也不强制停止已经运行的外部进程。
