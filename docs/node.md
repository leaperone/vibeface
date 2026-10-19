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

`init` 创建本机独立密钥、配置及一次性本机接入材料，不注册网站账号，也不把个人根私钥放进 Node。在本机可信浏览器打开 `http://localhost:4790`，网页自动读取本机设置。填写浏览器名称及恢复包口令，下载并确认保存加密恢复包后创建网络，无需终端接入链接。

客户端独立生成个人根身份和自己的设备身份，签署首个成员目录。Node 验证一次性本机授权后安装该目录，日常连接使用客户端设备密钥。已有网络的节点提供当前入口；新浏览器可使用本机自动访问，永久成员授权仍需原有成员邀请或加密恢复包。本机恢复自动填入目标及所需初始化材料。已有个人网络时选择**使用节点邀请加入**，按 [添加自己的设备](#添加自己的设备) 完成双方确认。

`./vibeface access --data-dir "$HOME/.vibeface"` 保留为备用入口。不要公开它输出的初始化链接。后续连接自动核验设备编号和成员授权，不输入账号密码。个人根私钥留在可信客户端及加密恢复包，不作为节点间的长期共享密码。

默认只监听本机。`http://127.0.0.1:4790` 和 `http://localhost:4790` 可用于本机初始化。`http://0.0.0.0:4790` 可打开服务，但不能自动取得初始化授权。保存密钥的网页客户端必须处于浏览器认可的安全上下文，推荐固定使用 `http://localhost:4790` 或可信 HTTPS 页面。

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含本机身份、配置、私有授权记录和 `node.sqlite3`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

### 本机自动访问

个人网络建立后，`local_auto_access` 默认开启。在本机打开当前节点的 localhost 网页，先尝试原有签名身份。需要本机会话时，点击**打开本机访问**，网页重新取得短期入口并导航到独立回环页面，无需配对新的永久成员。一次性 token 放在 URL fragment 中，页面清除后兑换绑定浏览器 Noise 公钥的 HttpOnly 会话。已有有效会话可直接重连；Node 重启或会话失效后，重新打开本机网页并点击该入口。`vibeface access` 保留为备用方式。

本机会话可使用本机业务、配置本机 Agent 和项目目录，并通过正常授权路由发送任务；不能签署成员授权、登录其他 Node 或替代网络管理客户端。已有签名管理身份继续保留管理权限。

可在网页的设备设置关闭本机自动访问，也可把 `config.json` 的 `local_auto_access` 改为 `false` 后重启。初始化时加 `--no-local-auto-access` 可直接关闭。关闭会撤销已有本机会话和待兑换 token；普通配对身份仍可使用。重新开启由管理客户端或本机配置完成，不恢复旧凭据。

本机会话兑换使用固定回环端口 `local_listen`（默认 `127.0.0.1:4791`），重启后本机 origin 和浏览器存储不变；端口被占用时 `init` 先报错并提示用 `--local-listen` 更换，不写数据目录。固定 localhost 网页只有通过实际 loopback 来源、字面 Host、精确 Origin 且不含转发头的设置请求，才能取得短期启动材料。一般公开请求仅伪造 `Host: localhost` 或代理头不能取得本机访问权。自动访问关闭时仍使用签名成员邀请或恢复包。反向代理应只转发公开 listener。

## 配置管理 Agent

协调 Agent 理解用户指令，用宿主 SDK 的 Shell 和文件工具读取本机，通过 Node 的 `codex` 工具启动原生 Codex 工作来修改本机代码，不持有 Codex App Server Socket。跨设备时，它发送任务给目标设备的协调 Agent，由目标设备本机执行并回传结果。SDK 会话缓存与共享聊天分开保存。`manager.workspace` 是 SDK 的本机工作目录，项目选择来自 `roots`。

通过 `manager.model` 选择账户实际可用的快模型。`auto` 或默认模型不是速度保证。2026-10-18 的独立 SDK 两轮对话测得首个模型输出约 7 至 10 秒，只验证了对应环境的模型对话与本机原生会话延续，不代表整个 Node、网页和后台任务的响应时间。

在签名管理浏览器或可信本机会话的**设置**中选择管理 SDK、模型和允许 Agent 访问的项目目录。默认 Agent 工作目录为当前数据目录下的 `manager-workspace`，首次保存启用 Agent 时自动建立为私有目录。自定义工作目录须已存在，且与项目编辑目录分开。保存只修改当前节点的本机配置，新操作立即读取，已运行任务不会重启或自动取消。

网页列出所需程序和调度角色的准备状态。保存启用 Codex SDK 时可启动其私有 App Server，但不会安装 Provider、执行账户登录、调用模型或扩大节点角色。程序可用也不代表 Provider 已登录。确认安装和授权前提后，再通过真实对话核验。下面的初始化参数是可选备用方式，字段说明见 [本机配置](node-protocol.md#本机配置)。

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

Cursor 使用 SDK 原生自定义工具回调连接 Rust Node。产品链路不启动 MCP Server，也不要求 Bot 或浏览器支持 MCP。协调工具限定到当前个人网络及机器人聊天，本机工作使用 SDK 宿主工具。Agent 不能替本人回答编码审批或授权自动后续。

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

这个适配器通过本机 `vibeface tool` 命令调用限定到当前网络和聊天的协调工具。所有调用的宿主 Shell 都是只读文件系统，网络只放行 `127.0.0.1` 和 `localhost`，不注入也不放行 Codex Socket。Codex 子进程环境按变量名筛选，Provider 凭证不放进 Shell 环境。macOS 的 Codex 沙箱不拦截 Unix Socket 连接，同一用户的进程若自行找到 Socket 路径仍可连接，不能把它当作隔离不可信模型或插件的边界。Cursor SDK 与 Codex SDK 的工具运行方式不同，不承诺相同的系统隔离。

## 添加自己的设备

每个 Node 和每个浏览器有独立密钥，设备编号就是设备公钥。本人设备列表来自已签名的成员目录，设备名称、IP 和中继返回的地址不证明所有权。

在管理设备选择**添加设备**，得到一条 `https://<gateway>/join#vibeface1…` 邀请链接和二维码，可勾选授予管理权。邀请链接含一次性 secret，只交给自己的设备。新浏览器打开链接，名称默认取主机名，可以改。两端随后各显示一个 6 位短码：短码一致才在管理设备上批准，不一致或不认识的请求点拒绝，该请求随即作废。新设备伪造或替换随机数时请求也会作废。

没有任何可公网访问的 HTTPS 入口时，邀请界面不显示二维码，提示“只能本机打开”，见 [配置公网入口](#配置公网入口)。`localhost` 与 `127.0.0.1` 仅指向各自本机。

添加另一台 Node 时，在新机器上执行：

```sh
./vibeface join --data-dir "$HOME/.vibeface" "https://gateway.example/join#vibeface1…"
```

命令显示 6 位短码并等待批准；在管理设备核对短码一致后批准，命令完成加入，再启动新 Node。`--name` 可改设备名，默认取主机名。连接中断时用同一链接和目录重试，结果未知时先核对原请求状态，不重新创建请求。

任何管理设备都能撤销设备。Node 不代替本人签署授权，Agent 不能添加设备、扩大权限或撤销设备。浏览器设备私钥不可导出，清除站点数据后需要重新加入。

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

## 配置公网入口

手机或另一台电脑要从外网打开网页，需要一个 HTTPS 入口。做法是在有公网地址的机器上运行一台 Node 作为 Gateway，用反向代理提供 HTTPS：

1. 准备域名并解析到这台机器，例如 `vibe.example.com`。
2. 用 Caddy 或 nginx 签发证书，把 `https://vibe.example.com` 整站反代到本机 `127.0.0.1:4790`，并允许 WebSocket 升级。Caddy 只需一行：`vibe.example.com { reverse_proxy 127.0.0.1:4790 }`。
3. 在这台 Node 的 `config.json` 设置 `"public_url": "https://vibe.example.com"`，重启。其他要嵌入此网页的 origin 加入 `allowed_origins`。
4. 用 `vibeface join` 让这台 Node 加入个人网络，之后管理设备生成邀请时会使用它的公网地址并显示二维码。

反代只转发公开 listener，不要转发本机会话的 `local_listen`。浏览器只经 `/api/client` 跨源连接，其他路径仍要求同源。网页中的**配置公网入口**链接指向本节。

## 发现与公共中继

Node 可启用局域网发现，从候选地址找到已知 Peer ID。公网发现由可替换的会合节点提供。发现记录带设备签名和过期时间，只说明某个设备声明了路径，连接时仍需核验身份及个人成员授权。

公网或可达主机运行同一个 Node，也可启用调度或执行能力。启用公共中继时，其他人的设备可预约出站通道并通过它连通。中继只转发端到端密文，不加入对方个人网络，不获得会话、项目目录或执行权限。

用 `--bootstrap-url` 指定自己选择的发现或中继入口。以下是三种独立用法：

```sh
./vibeface --bootstrap-url https://relay.example
./vibeface serve --bootstrap-url wss://relay.example/api/network/ws
./vibeface init --data-dir /absolute/path/to/new-node --bootstrap-url https://relay.example
```

参数可重复，最多指定 8 个入口。HTTP(S) 地址自动转换成 WS(S) 的 `/api/network/ws` 端点。URL 不接受凭证、查询参数或片段。HTTP(S) 仅接受根路径或 `/api/network/ws`，WS(S) 保留显式的非根路径，程序没有内置的公共入口。

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

历史较多时，Node 分批保存和补齐内容，未完成项留待后续同步。某个副本缺少内容时，可从本人其他已授权副本补齐。已经完整且没有变化的历史再次同步不会重写业务投影。缺少必要历史或附件时，调度和执行仍会拒绝继续，导入旧历史不会重新执行操作。批读协议见[事件 DAG 协议](node-protocol.md#事件-dag)。

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

管理 Agent 忙时，追加消息进入持久队列。尚未开始的本人消息优先于自主后续，同一调度 Node 上的同一会话一次只运行一个管理轮。管理轮派发工作后即结束，不等待工作；工作结束时它的结果作为新的一轮入队，Agent 默认以只读权限汇报。关闭网页不会停止 Node 上的工作，但执行设备关机、休眠或断网会影响推进。

需要停止时，区分以下动作：

- **停止当前回复**只请求取消当前管理轮，并撤销该轮的 Node 工具权限。已输出的部分回复保留为一条标记“已停止”的回答。取消待确认时不能当作已经停止，也不停止已派发的工作。
- **停止工作**作用于选中的工作，中断它的原生 turn，并一并停止它委派出的工作。请求发出后仍需核对实际终态。
- **停止后续**在聊天输入框上方把“收到结果后”设为停止后续，作用于整个机器人聊天。撤销权限阻止新的自主派发，不证明当前外部进程已停止。

Cursor SDK 和 Codex SDK 都提供进度和取消请求，当前都没有管理轮的原生 steer。立即调整须先核对取消结果，再提交新一轮。Cursor 需要匹配的 Provider 终态或受管进程退出证据才能确认取消。Codex 在停止时记录 CLI 进程树中的全部子进程及其进程组，逐组终止并确认这些进程都已退出后才回报已停止；无法确认时该轮进入 `interrupted` 并列入待核对。停止回复不影响已派发的工作，停止工作是单独操作。

Agent 通过 Node 的 `dispatch_work` 工具派发工作，例如 `vibeface tool dispatch_work --json '{"prompt":"...","cwd":"..."}' --call-ref REF`。`cwd` 必须位于授权 `roots` 内，Node 固定 `approvalPolicy=on-request`、`approvalsReviewer=user`，并把沙箱限定为该目录的 workspace-write、关闭网络。同一轮用同一 `call_ref` 重试得到同一工作，不会启动第二个 turn；结果不明时用 `call_lookup` 查询。Agent 没有回答审批的入口，审批只能由本人在网页回答。第一期只能派发到本机。

Node 托管本机的 `codex app-server`：未设置 `codex_socket` 时，Node 用 `codex_bin` 启动它，Socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix Socket 路径长度限制。App Server 运行在独立进程组，进程组号写入数据目录的 `codex-app-server.pid`。Node 启动时先结束上次遗留的进程组，正常退出时一并停止它，异常退出则退避重启。要连接自己管理的实例，在 `config.json` 设置 `codex_socket`。启动网页成功不等于程序已安装、已登录或模型可用，应查看聊天中工作的实际状态。

App Server 重启或重连后，Node 只读复查未结束的工作，不重发旧指令，也不自动批准审批。重启前未回答的审批作废。读不到原 turn 的工作标为“待核对”，由本人附证据结清。

普通执行的终态结果自动送回原聊天，默认只汇报，模型总结使用只读权限，不能借结果回调创建或派发新工作。收到结果后继续工作须由本人在该聊天中选择继续工作，模型和聊天文字不能代授予。并发修改授权会要求刷新版本，已知撤销后旧权限不能继续写入；离线设备尚未收到撤销时不能宣称全网已经生效。

普通进度和通知不调用管理模型。`daily_decision_limit` 只限制当前 Node、当前个人网络在 UTC 当天的自主管理调用，本人主动聊天不消耗这个额度。失败的自主调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。没有模型汇报授权或额度耗尽时，原始结果和通知仍保留。

编码执行器结束一轮不等于交付目标已经达成，由本人核对产物后确认。审批必须由本人回答，并对应仍有效的原执行请求。停止后续不会自动停止已经运行的进程，仍需核对后台工作的实际状态。

未读提醒归属原机器人聊天。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与设置中的通知权限。关闭网页后没有独立的手机推送服务。

从另一台入口打开原会话，可只读观察原管理节点和执行节点的临时进度。重连后读取持久状态，缺失的临时文字不会触发重做。缓存状态会标明新鲜度，离线时不能据此确认远端仍在运行。长文本结果可在聊天中展开或下载；尚未同步的附件需等待原副本可达，只有哈希不能补回正文。

## 待核对

网络断开或进程中断后，工作可能已经执行，但回执未保存。Node 不自动重新调用 SDK 或向其他设备重复派发，而是把无法确认的工作列入聊天中的同一个待核对列表。

管理 SDK 已返回明确结果时，Node 先将最终结果保存到本机。历史依赖暂未补齐或最终事件暂时写入失败时，只补发最终记录，不重新调用 SDK。

管理轮重启或 SDK 进程退出无法确认时，原轮进入终态 `interrupted`，不重跑原触发，也不冻结会话。管理 SDK 的文件系统只读、网络仅限本机回环，但未确认退出的进程仍可能经回环访问本机服务；Node 工具权限已封存，原生工作经 `codex` 工具登记后单独观察。因此仍因此登记一条管理进程待核对，关联它的进程组、原生 turn 和 Agent 任务。原生 turn 结果未知时登记原生待核对。

每条待核对记录有两个动作：「复查原目标」只读查询原设备，管理进程项只确认进程组是否已退出，原生项补查原 turn；「确认已核对」由本人检查实际进程、项目产物和外部副作用后附证据提交。进程运行期间对本机服务的副作用只能由本人确认。模型不能结清待核对，确认也不重放原任务。失联不能证明远端进程已停止。

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

`check` 只查询与校验版本信息。`apply` 仅安装声明与当前数据格式兼容的包，需要安全的空闲窗口。待投递消息、排队与活动管理轮、活动任务和待核对记录都会阻止安装，先完成或核对原任务，不需要关闭网页。升级不会调用模型或修改 Provider 密钥。

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

存储使用 `rusqlite`，没有 ORM。设备授权、配对回执和业务记录共用单一 `node.sqlite3` 及同一个 Store 事务。Atlas HCL 为 `crates/node/schema.hcl`，唯一 initial SQL 和校验清单位于 `crates/node/migrations/`。数据格式为 epoch 6，数据库 `user_version` 为 6，直接初始化最终结构，不提供旧库迁移。部署此版本使用全新数据目录和同版本节点，不能把新程序直接接到旧库。程序不会自动清空旧目录。监控历史正文、附件和服务日志的磁盘占用。
