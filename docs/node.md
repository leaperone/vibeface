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
bash scripts/build-web.sh
./scripts/package-node.sh
```

`--target` 指定 Rust target，仍需本机具备相应工具链。此选项不保证跨平台编译成功。`--skip-build` 使用已有 release 程序，不能用于证明程序与当前源码一致。

验收制品时，密码学资源、网页、Rust 程序和 SDK sidecar 必须来自同一提交。使用干净 checkout 重新构建，核对包内 `BUILD-INFO.json` 的 `source_commit` 与验收 SHA 一致。`source_commit` 为 `null` 的包不能证明这一点，网页能打开也不证明真实 SDK 调用及后台回报闭环可用。

## 初始化首台设备

直接运行 `./vibeface` 即可启动本机节点。首次运行仅在不存在或为空的数据目录中初始化，默认目录是 `$HOME/.vibeface`，不授予执行目录或启用管理 Agent。根 `--data-dir` 可选择数据目录；有子命令时使用子命令自己的 `--data-dir`，两种位置不能混用。显式 `serve` 要求已初始化。

`--root` 指向本机 Agent 可选择的现有绝对项目目录，可重复指定。省略时没有项目目录。

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

在自己信任的浏览器打开链接，核对设备编号，为设备网络命名并保存加密恢复包。客户端独立生成个人根身份和自己的设备身份，签署首个成员目录。Node 验证一次性本机授权后安装该目录，日常连接使用客户端设备密钥。

不要公开接入链接。其中的授权材料用于初始化或加入当前设备。后续连接自动核验设备编号和成员授权，不输入账号密码。个人根私钥留在可信客户端及加密恢复包，不作为节点间的长期共享密码。

默认只监听本机。`http://127.0.0.1:4790`、`http://localhost:4790` 和本机字面地址 `http://0.0.0.0:4790` 可用于打开服务。保存密钥的网页客户端必须处于浏览器认可的安全上下文，推荐使用 `http://localhost:4790` 或可信 HTTPS 页面。

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含本机身份、配置、私有授权记录和 `node.sqlite3`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

### 本机自动访问

个人网络建立后，`local_auto_access` 默认开启。在本机运行 `vibeface access`，打开它生成的链接即可使用此 Node，无需再配对新的浏览器成员。启动器使用本机私有凭据取得一次性 token，token 放在 URL fragment 中，页面清除后兑换绑定浏览器 Noise 公钥的本机会话。已有有效会话可直接重连。Node 重启或会话失效后，重新运行 `access`。

本机会话可使用本机业务和通过正常授权路由发送任务，不能签署成员授权、登录其他 Node 或替代管理客户端。已有签名管理身份继续保留管理权限。

可在网页的设备设置关闭本机自动访问，也可把 `config.json` 的 `local_auto_access` 改为 `false` 后重启。初始化时加 `--no-local-auto-access` 可直接关闭。关闭会撤销已有本机会话和待兑换 token；普通配对身份仍可使用。重新开启由管理客户端或本机配置完成，不恢复旧凭据。

本机授权使用独立的随机回环端口。公开入口不提供本机会话兑换；来源 IP、`Host: localhost` 和代理头都不授予权限。裸 localhost 页面没有有效会话时仍需启动器链接或普通配对。反向代理应只转发公开 listener。

## 配置管理 Agent

协调 Agent 理解用户指令，使用宿主 SDK 的 Shell 和文件工具完成本机工作，也能直接访问配置的 Codex App Server Socket。跨设备时，它发送任务给目标设备的协调 Agent，由目标设备本机执行并回传结果。SDK 会话缓存与共享聊天分开保存。`manager.workspace` 是 SDK 的本机工作目录，项目选择来自 `roots`。

通过 `manager.model` 选择账户实际可用的快模型。`auto` 或默认模型不是速度保证。2026-10-18 的独立 SDK 两轮对话测得首个模型输出约 7 至 10 秒，只验证了对应环境的模型对话与本机原生会话延续，不代表整个 Node、网页和后台任务的响应时间。

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

Cursor 使用 SDK 原生自定义工具回调连接 Rust Node。产品链路不启动 MCP Server，也不要求 Bot 或浏览器支持 MCP。协调工具限定到当前个人网络及机器人聊天，本机工作使用 SDK 宿主工具。Agent 不能替本人回答编码审批、授权自动后续或确认目标完成。

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

这个适配器通过本机 `vibeface tool` 命令调用限定到当前网络和聊天的协调工具。普通工作开放宿主 Shell、文件写入和配置的 Codex Socket；只读结果汇报限制文件写入，并且不注入 Socket 地址。Codex 子进程环境按变量名筛选，Provider 凭证不放进 Shell 环境。macOS 的只读 profile 不隔离任意 Unix Socket，不能把它当作隔离不可信模型或插件的边界。Cursor SDK 与 Codex SDK 的工具运行方式不同，不承诺相同的系统隔离。

## 添加自己的设备

每个 Node 和每个客户端有独立密钥。本人设备列表来自已签名的成员目录，设备名称、IP 和中继返回的地址不证明所有权。

在已授权管理客户端选择**添加设备**，选择 Node 或客户端及其权限。将短期邀请通过可信途径交给新设备，核对个人网络和邀请端的完整设备编号。新设备独立生成密钥并申请加入。管理客户端核对待加入的名称和设备编号，再明确确认。

添加另一台 Node 时，先在新设备用独立数据目录运行 `init`，再把完整邀请保存为私有 JSON 文件。邀请文件包含接入地址、钉住的 Peer ID、签名邀请和一次性 capability，不公开分享。

```sh
./vibeface pair \
	--data-dir "$HOME/.vibeface" \
	--invite-file /private/path/to/invitation.json \
	--name "My Linux Node"
```

命令提交新设备持钥证明后等待管理客户端确认。核对名称、完整设备编号和角色后批准，命令显示配对完成，再启动新 Node。连接中断时使用原邀请、目录和名称重试，CLI 保留待确认请求身份，不另建授权。

Node 不代替客户端签署授权。Agent 不能添加设备、扩大权限或撤销设备。新设备加入后可同步本人已授权的上下文，在设备列表中显示名称、设备编号、角色和在线状态。

网页的**我的入口**可保存直连地址或公共中继地址，连接信息可通过链接、二维码或文件交给另一台设备。连接信息固定目标 Node 指纹，不代替设备配对授权。断线后查询同一配对申请，不重新创建授权。更换入口后使用同一客户端身份认证目标 Node，再补齐业务历史。跨 origin 的浏览器存储互相隔离，使用固定可信客户端来源可保留自己的设备密钥。

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

Node 可启用局域网发现，从候选地址找到已知 Peer ID。公网发现由可替换的会合节点提供。发现记录带设备签名和过期时间，只说明某个设备声明了路径，连接时仍需核验身份及个人成员授权。

公网或可达主机运行同一个 Node，也可启用调度或执行能力。启用公共中继时，其他人的设备可预约出站通道并通过它连通。中继只转发端到端密文，不加入对方个人网络，不获得会话、项目目录或执行权限。

用 `--bootstrap-url` 指定自己选择的发现或中继入口。以下是三种独立用法：

```sh
./vibeface --bootstrap-url https://relay.example
./vibeface serve --bootstrap-url wss://relay.example/api/network/ws
./vibeface init --data-dir /absolute/path/to/new-node --bootstrap-url https://relay.example
```

参数可重复，最多指定 8 个入口。HTTP(S) 地址自动转换成 WS(S) 的 `/api/network/ws` 端点。URL 不接受凭证、查询参数、片段或其他路径，程序没有内置的公共入口。

首次直接启动或显式 `init` 会保存传入的 URL。已有节点启动时，传入的列表仅覆盖本次运行的 `network.bootstrap_urls`，不改写原配置与身份；省略参数时沿用配置。`serve` 始终要求已初始化的数据目录。使用子命令时，将 `--bootstrap-url` 写在子命令后。入口提供路由和中继预约，设备仍须获得个人网络授权。

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

各入口分别发布地址和维持中继预约。某个入口拒绝预约或暂时无响应时，其他有效入口仍能继续。无效或过期的目录记录只影响对应来源，连接仍须核验目标身份和成员授权。

局域网发现按服务实例维护地址，移除一个实例后仍可使用同一设备的其他有效来源。固定 IP 监听只宣告该地址，回环监听不宣告自身；wildcard 监听只发现和宣告对应地址族。接口变化会撤销旧候选，但不会自动重绑固定监听地址。地址发现不保证设备已经获得授权或当前可以连接。

中继按实际 TCP 来源限制连接容量和请求频率。经同一反向代理接入的连接共用来源额度，`X-Forwarded-For` 和 `Forwarded` 不改变归属。容量不足只拒绝本次新工作，已建立的预约和电路继续受各自限制保护。具体额度见[协议参考](node-protocol.md#发现对等连接与公共中继)。

公网网页来源使用 HTTPS，WebSocket 使用 WSS。Node 本身可绑定回环 HTTP，由反向代理终止 TLS。代理保留公网 Host 与原始 Origin，并支持 `/api/network/ws` 的 WebSocket upgrade。中继不是可信网页发行者，使用本人信任的客户端代码来源。

历史较多时，Node 分批保存和补齐内容，未完成项留待后续同步。某个副本缺少内容时，可从本人其他已授权副本补齐。已经完整且没有变化的历史再次同步不会重写业务投影。缺少必要历史或附件时，调度和执行仍会拒绝继续，导入旧历史不会重新执行操作。批读兼容边界见[事件 DAG 协议](node-protocol.md#事件-dag)。

附件下载的已收部分保存在本机私有暂存文件。断线或重启后，Node 重新核验授权并从已保存的位置继续；只有最终大小和 hash 校验通过的完整附件才能使用。暂存最多占用 256 MiB、64 个对象，清理失败时保留进度和占用，后续维护继续重试。连接恢复只继续传输，不重发业务操作。

### 纯 Web 与 Relay

PVE 仅提供网页和公共中继时，在独立空目录初始化：

```sh
vibeface init \
	--data-dir "$HOME/.vibeface-relay" \
	--relay-only \
	--listen 127.0.0.1:4790 \
	--public-url https://relay.example
vibeface serve --data-dir "$HOME/.vibeface-relay"
```

该模式启用公共发现和中继，不初始化个人网络、不配置管理 Agent，也不启动 Codex。为 `https://relay.example` 配置 TLS 反向代理，并支持 `/api/network/ws` 的 WebSocket upgrade。

在 Mac mini 的 `config.json` 中把 `network.bootstrap_urls` 设为 `["wss://relay.example/api/network/ws"]`，重启 Mac mini Node。Mac mini 主动登记签名路径并维持中继预约，无需入站公网端口。PVE 不需要加入 Mac mini 的成员目录。

手机打开可信的 PVE 网页，导入含 Mac mini 完整指纹的连接信息。首次访问仍在 Mac mini 完成配对和持钥验证，之后使用手机自己的设备凭据重连。公共目录只按已知 Peer ID 查路由，不枚举私人设备，也不能仅凭设备名称证明它属于你。公开发现和密文转发不要求个人网络成员身份，业务权限由 Mac mini 验证。

## 创建机器人并处理结果

连接自己的设备后，创建一个有名称的机器人，按需填写职责和项目。进入机器人即可直接聊天，每个机器人保留自己的长期历史。创建机器人只保存定义，不启动模型；首次发消息时，程序选择已授权且可用的聊天 Agent。

机器人定义、聊天历史和本机 SDK 会话分开保存。机器人项目配置只是执行偏好，不授予目录或设备权限；执行时仍检查 Node 真实授权。多个机器人可以使用同一 Node 的 SDK 配置，但不共用聊天历史或本机 SDK 缓存。设备、连接和恢复在设置中。

普通查询可以直接回答，本机工作使用 SDK 的宿主工具，远端工作通过 Agent 任务消息委派。日常操作不需要创建任务单、打开执行会话或选择调度器。进展、本人审批、补充输入和结果都回到原机器人的聊天。

发送后先看状态。`pending_delivery` 表示入口已保存消息，正在投递给原调度设备；只有目标持久入队后才是 `queued`。排队不表示模型已开始，`running` 才表示已跨过 SDK 启动边界。目标离线、能力不满足或回执丢失时，保留原目标与消息身份核对，不自动改派另一台设备。

管理 Agent 忙时，追加消息进入持久队列。尚未开始的本人消息优先于自主后续，同一调度 Node 上的同一会话一次只运行一个管理轮。本轮已登记的原生工作和子任务结束后，Agent 以只读权限收尾，再消费后续消息。关闭网页不会停止 Node 上的工作，但执行设备关机、休眠或断网会影响推进。

需要停止时，区分以下动作：

- **停止当前回复**只请求取消当前管理轮，并撤销该轮的 Node 工具权限。取消待确认时不能当作已经停止，也不停止已派发的后台任务。
- **停止后台工作**作用于选中的 Agent 任务或已登记的原生 turn，按原任务、设备和会话关联发出取消或中断。请求发出后仍需核对实际终态。
- 明确长期目标的自主继续权限仍由本人决定。撤销权限阻止新的自主派发，不证明当前外部进程已停止。

Cursor SDK 和 Codex SDK 都提供进度和取消请求，当前都没有管理轮的原生 steer。立即调整须先核对取消结果，再提交新一轮。Cursor 需要匹配的 Provider 终态或受管进程退出证据才能确认取消。Codex CLI 退出仍不能证明已分离的工具进程全部退出，当前真实适配器将这种取消保留为 `unknown`。

Agent 可通过 `vibeface codex METHOD --json 'JSON_OBJECT' --socket /absolute/codex.sock` 调用 App Server 原生方法。省略 `--socket` 时使用 SDK 环境提供的 `VIBEFACE_CODEX_SOCKET`。`--watch` 输出原生事件，并接受本人明确提供的请求响应。连接中断后不会自动重发写方法；关闭观察连接不会停止后台工作。

启用 Agent 的 Node 可以启动与本机身份关联的私有 `codex app-server`，Socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix Socket 路径长度限制。要连接已有实例，在 `config.json` 设置 `codex_socket`，`codex_bin` 指向 Codex 程序。启动网页成功不等于程序已安装、已登录或模型可用，应查看聊天中后台工作的实际状态。

Agent 启动原生工作后，使用 `observe_session` 登记准确的 thread 与 turn。Node 观察进度、审批及终态，断线后只读补查，不要求修改用户全局 Codex hooks，也不重发旧指令或自动批准审批。配置的 Socket 未加载的线程不视为可控制；另一个 stdio 进程的历史记录不能证明其当前状态。

普通执行的终态结果自动送回原聊天，模型总结使用只读权限，不能借结果回调创建或派发新工作。明确长期目标的自主继续仍须本人授权，模型不能代授予。并发修改授权会要求刷新版本，已知撤销后旧权限不能继续写入；离线设备尚未收到撤销时不能宣称全网已经生效。

普通进度和通知不调用管理模型。`daily_decision_limit` 只限制当前 Node、当前个人网络在 UTC 当天的自主管理调用，本人主动聊天不消耗这个额度。失败的自主调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。没有模型汇报授权或额度耗尽时，原始结果和通知仍保留。

编码执行器结束一轮不等于交付目标已经达成。已有明确长期目标的历史保留目标和验收记录，本人核对产物后确认。审批必须由本人回答，并对应仍有效的原执行请求。

已确认完成或取消的长期目标不能由 Agent 自行恢复。需要继续时，在机器人聊天中明确提出新要求。取消目标不会自动停止已经运行的进程，仍需核对后台工作的实际状态。

未读提醒归属原机器人聊天。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与设置中的通知权限。关闭网页后没有独立的手机推送服务。

从另一台入口打开原会话，可只读观察原管理节点和执行节点的临时进度。重连后读取持久状态，缺失的临时文字不会触发重做。缓存状态会标明新鲜度，离线时不能据此确认远端仍在运行。长文本结果可在聊天中展开或下载；尚未同步的附件需等待原副本可达，只有哈希不能补回正文。

## 核对未知结果

网络断开或进程中断后，工作可能已经执行，但回执未保存。Node 将这种情况保留为 `unknown`，不自动重新调用 SDK 或向其他设备重复派发。

管理 SDK 已返回明确结果时，Node 先将最终结果保存到本机。历史依赖暂未补齐或最终事件暂时写入失败时，只补发最终记录，不重新调用 SDK。

管理轮重启后也可能变为 `unknown`。原轮保留未知，不重跑原触发。Node 隔离旧工具权限并核对在途请求后，新的本人消息可进入独立的只读会话查询状态。恢复写入须由本人核对原操作并附证据确认；管理未知确认不会把执行未知改成成功，旧只读权限也不会自动升级。

在原机器人的聊天中查看原任务和原生会话的状态。若仍无法判断，检查实际进程、项目产物和外部副作用，再为未知管理轮提供核对证据。确认只解除对应管理写冻结，不把未确认的原生工作改成成功，也不重放原任务。失联不能证明远端进程已停止。

A 已派发 B 后掉线时，从 C 打开原业务会话，等待 DAG 补齐，再核对 B 的实际执行状态。缺失历史、执行器不可达和结果未知分别显示，不能通过创建同名新任务代替恢复。

同一调度 Node 对同一会话串行领取管理轮，不提供旧执行层的项目目录锁或跨设备全局文件锁。多个 Agent 需要修改同一项目时，使用独立 worktree 或独立根目录隔离。

浏览器在当前入口的本地存储中保留尚未确认的消息正文、消息编号和原调度节点，收到目标持久入队确认后删除。入口仅保存待投递消息不算这个确认。刷新后重试须使用原内容、汇报意图和原目标；改写正文会被拒绝，避免把新目标误当成旧操作重试。

## 作为后台应用运行

`vibeface` 是节点主进程，也是本机管理 CLI。先用前台 `serve` 确认配置，再按 Ctrl-C 停止前台进程，安装用户级服务：

```sh
vibeface service install --data-dir "$HOME/.vibeface"
vibeface status --data-dir "$HOME/.vibeface"
vibeface service status --data-dir "$HOME/.vibeface"
```

服务管理命令需要完整的受管安装，启动路径始终为安装根目录下的 `current/vibeface`。通过 `~/.local/bin/vibeface` 链接调用也保留该路径，升级后不会固定在旧 release。直接运行 `cargo build` 的程序不安装服务。自定义安装根先使用 `update configure --install-root /absolute/path` 保存路径。

每个系统用户管理一个 Node 服务。各服务命令核对 `--data-dir` 与已安装服务一致，再操作该服务。省略时使用 `$HOME/.vibeface`。管理其他数据目录时显式传入绝对路径。

`status` 通过私有维护 socket 检查运行中的 Node，并核对 Peer ID，不要求安装服务。`status --json` 输出状态、Peer ID、监听地址、角色及运行版本，不输出接入授权材料。已停止或无法连接时返回非零退出码。`service status` 则展示 launchd 或 systemd 的 Node 和更新服务状态。

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

`check` 只查询与校验版本信息。`apply` 仅安装声明与当前数据格式兼容的包，需要安全的空闲窗口。待投递消息、排队与活动管理轮、活动任务和结果未知的记录都会阻止安装，先完成或核对原任务，不需要关闭网页。升级不会调用模型或修改 Provider 密钥。

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

新进程通过健康检查并开放接单后才消费队列。存在新版本未处理的调度记录时，不支持任意手动复制旧程序降级；受管回退限于尚未开放接单或已经安全排空的状态。DAG 同步和恢复只补历史、重算视图，不从历史重新生成执行。

## 备份与恢复

在可信管理客户端导出加密恢复包，选择足够强且独立保存的密码。密码只保护恢复材料，不发送给 Node，不作为节点连接密码。

恢复包用于恢复个人根身份、必要的解密材料及建立新的管理授权。客户端恢复时生成新的设备密钥，不复制丢失设备的日常身份。恢复记录使用更高的授权 epoch，获知恢复记录的 Node 拒绝过期授权。

恢复根身份不会自动找回聊天正文、工程文件或执行器隐藏状态。业务历史仍需可达的授权副本或独立数据备份。缺少材料时明确停止，不创建看似相同的新身份替代原网络。

在**我的设备**中移除丢失设备。收到签名撤销记录的端点拒绝后续操作，已建立会话也重新验权。离线分区收到记录前不能立即生效，撤销不能抹除对方已经读过的数据，也不强制停止已运行的外部进程。

停掉 Node 后备份完整数据目录，保留 SQLite 数据库及未合并的 WAL。模型 key、本机私钥和 SDK session 留在本机，不进入共享 DAG。完整目录备份含敏感信息，按本机凭证保管。不要复制运行中的 SQLite 文件来实现节点同步。

存储使用 `rusqlite`，没有 ORM。设备授权、配对回执和业务记录共用单一 `node.sqlite3` 及同一个 Store 事务。Atlas HCL 为 `crates/node/schema.hcl`，唯一 initial SQL 和校验清单位于 `crates/node/migrations/`。数据格式为 epoch 5，数据库 `user_version` 为 5，直接初始化最终结构，不提供旧库迁移。部署此版本使用全新数据目录和同版本节点，不能把新程序直接接到旧库。程序不会自动清空旧目录。监控历史正文、附件和服务日志的磁盘占用。
