# 部署与使用 Node

本文的命令使用[部署配置](deployment.md)中显式设置的 `VIBEFACE_DATA_DIR`、`VIBEFACE_INSTALL_ROOT` 和 `VIBEFACE_BIN_DIR`。先设置所需变量，再执行对应步骤。

本文使用 `vibeface` 管理自己的设备。HTTP 与同步协议见 [协议参考](node-protocol.md)，出问题时见 [排障](troubleshooting.md)。

## 安装制品

选择对应操作系统与 CPU 架构的 `vibeface-<version>-<target>.tar.gz`，先校验同名 `.sha256` 文件，再解压。

```sh
shasum -a 256 -c vibeface-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-<version>-<target>.tar.gz
cd vibeface-<version>-<target>
./vibeface --help
```

包内包括 Rust 程序、文档和用户级服务安装脚本，不包含模型凭证或 Provider 程序。

从源码生成包时，先构建同一 Rust 密码学模块的浏览器资源。发布包已包含资源，安装发布包不需要这些构建工具。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
./scripts/package-node.sh
```

`--target` 指定 Rust target，仍需本机具备相应工具链。此选项不保证跨平台编译成功。打包始终重新构建原生程序，并校验 Web 与 WASM 的来源和完整文件清单。

验收制品时，密码学资源、网页和 Rust 程序必须来自同一提交。使用干净 checkout 重新构建，核对包内 `BUILD-INFO.json` 的 `source_commit` 与验收 SHA 一致。`source_commit` 为 `null` 的包不能证明这一点，网页能打开也不证明真实 SDK 调用及后台回报闭环可用。

## 初始化首台设备

先按[部署配置](deployment.md)明确设置 `VIBEFACE_DATA_DIR` 或 `--data-dir`，再启动节点。首次运行仅在指定的空数据目录初始化，不授予执行目录或启用 Provider。根 `--data-dir` 用于直接启动；有子命令时使用子命令自己的 `--data-dir`，两种位置不能混用。显式 `serve` 要求已初始化。

`--root` 指向本机 Agent 可选择的现有绝对项目目录，可重复指定。省略时没有项目目录。

```sh
./vibeface init \
	--data-dir "$VIBEFACE_DATA_DIR" \
	--root /absolute/path/to/projects
./vibeface serve --data-dir "$VIBEFACE_DATA_DIR"
```

`init` 创建本机独立密钥、配置及一次性本机接入材料，不注册网站账号，也不把个人根私钥放进 Node。在本机可信浏览器打开 `http://localhost:4790`，网页自动读取本机设置。填写浏览器名称及恢复包口令，下载并确认保存加密恢复包后创建网络，无需终端接入链接。

客户端独立生成个人根身份和自己的设备身份，签署首个成员目录。Node 验证一次性本机授权后安装该目录，日常连接使用客户端设备密钥。已有网络的节点提供当前入口；新浏览器可使用本机自动访问，永久成员授权仍需原有成员邀请或加密恢复包。本机恢复自动填入目标及所需初始化材料。要加入已有个人网络的 Node 不在网页上建网，用 `vibeface join` 加入，见 [添加自己的设备](#添加自己的设备)。

`./vibeface access --data-dir "$VIBEFACE_DATA_DIR"` 保留为备用入口。不要公开它输出的初始化链接。后续连接自动核验设备编号和成员授权，不输入账号密码。个人根私钥留在可信客户端及加密恢复包，不作为节点间的长期共享密码。

默认只监听本机。`http://127.0.0.1:4790` 和 `http://localhost:4790` 可用于本机初始化。`http://0.0.0.0:4790` 可打开服务，但不能自动取得初始化授权。保存密钥的网页客户端必须处于浏览器认可的安全上下文，推荐固定使用 `http://localhost:4790` 或可信 HTTPS 页面。

`init` 不覆盖现有身份和配置。数据目录由 `--data-dir` 或 `VIBEFACE_DATA_DIR` 明确指定，包含本机身份、配置、私有授权记录和 `node.sqlite3`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

### 本机自动访问

个人网络建立后，`local_auto_access` 默认开启。在本机打开当前节点的 localhost 网页，先尝试原有签名身份。需要本机会话时，点击**打开本机访问**，网页重新取得短期入口并导航到独立回环页面，无需配对新的永久成员。一次性 token 放在 URL fragment 中，页面清除后兑换绑定浏览器 Noise 公钥的 HttpOnly 会话。会话保存在数据目录的 `local-sessions.json`（只存 cookie 摘要），Node 重启后刷新页面即可继续，无需重新兑换；会话失效或本机访问被关闭后，重新打开本机网页并点击该入口。本机会话在网页顶部显示为“本机管理”。

`vibeface access` 保留为备用方式，它输出的链接先打开 `http://localhost:<listen 端口>`。这个浏览器已在该地址加入时直接沿用已加入的身份，不开本机会话；没加入时才转到回环页面开本机会话。两个地址是不同 origin，浏览器存储各自独立。

本机会话可使用本机业务、配置本机 Agent 和项目目录，并通过正常授权路由发送任务；不能签署成员授权、登录其他 Node 或替代网络管理客户端。已有签名管理身份继续保留管理权限。

可在网页的设备设置关闭本机自动访问，也可在本机执行 `vibeface access --disable`。`vibeface access --enable` 重新开启并输出本机入口。命令经同一用户才能访问的维护 socket 交给运行中的 Node，立即生效，无需改配置或重启；Node 未运行时只写入配置，启动后生效。初始化时加 `--no-local-auto-access` 可直接关闭。关闭会撤销已有本机会话和待兑换 token；普通配对身份仍可使用。重新开启不恢复旧凭据。

本机会话兑换使用固定回环端口 `local_listen`（默认 `127.0.0.1:4791`），重启后本机 origin 和浏览器存储不变；端口被占用时 `init` 先报错并提示用 `--local-listen` 更换，不写数据目录。固定 localhost 网页只有通过实际 loopback 来源、字面 Host、精确 Origin 且不含转发头的设置请求，才能取得短期启动材料。一般公开请求仅伪造 `Host: localhost` 或代理头不能取得本机访问权。自动访问关闭时仍使用签名成员邀请或恢复包。反向代理应只转发公开 listener。

## 配置管理 Agent

协调 Agent 理解用户指令，用宿主 SDK 的 Shell 和文件工具读取本机，通过 Node 的 `dispatch_work` 工具派发工作来修改代码：交给本机或另一台设备上打开了执行任务的 Agent，或把任务交给另一台设备的协调 Agent，由它在那台设备上执行并回传结果。SDK 会话缓存与共享聊天分开保存。`manager.workspace` 是 SDK 的本机工作目录，项目选择来自 `roots`。

通过 `manager.model` 选择账户实际可用的模型。`auto` 或默认模型不保证响应速度，实际延迟取决于 Provider、模型和网络。

在签名管理浏览器或可信本机会话打开设备页的 **AI**。Codex、Cursor、Claude Code、pi 和 opencode 各占一行：点某一行的 Bot 图标，由它运行 Bot，再点一次关闭 Bot；除 Cursor 外，各行的任务图标开关本机执行任务。模型、Bot 工作文件夹和组件路径在 Bot 所在服务的**设置**里，允许 Agent 访问的项目目录在**可用文件夹**里。默认 Agent 工作目录为当前数据目录下的 `manager-workspace`，首次选择 Bot 时自动建立为私有目录。自定义工作目录须已存在，且与项目编辑目录分开。修改只影响当前节点的本机配置，新操作立即读取，已运行任务不会重启或自动取消。

网页在每个服务那一行列出所需程序和准备状态。Codex 打开执行任务且选择由 Vibeface 启动时，Node 启动私有 App Server，但不会安装 Provider、执行账户登录、调用模型或扩大节点角色。程序可用也不代表 Provider 已登录。确认安装和授权前提后，再通过真实对话核验。下面的初始化参数是可选备用方式，字段说明见 [本机配置](node-protocol.md#本机配置)。

### 使用 Cursor SDK

从 [Cursor SDK Bridge v1.0.37 发布页](https://github.com/cursor/sdk-bridge/releases/tag/v1.0.37) 下载对应平台的 standalone 包，用发布页的校验文件核对后解压。可执行文件位于 `bin/cursor-sdk-bridge`。例如，Apple Silicon Mac 的包名是 `cursor-sdk-bridge-standalone-darwin-arm64.tar.gz`。

在运行 Node 的环境中配置 `CURSOR_API_KEY`。不要把密钥写进命令行、URL、Git 或聊天记录。模型使用账户实际可用的名称。

```sh
./vibeface init \
	--data-dir "$VIBEFACE_DATA_DIR" \
	--root /absolute/path/to/projects \
	--manager cursor \
	--workspace /absolute/path/to/manager-workspace \
	--model auto \
	--bin cursor=/absolute/path/to/bin/cursor-sdk-bridge
```

此命令用于尚未初始化的 Node。默认使用 `auto` 模型。SDK 程序在本机运行，模型请求仍发往 Cursor 服务，消耗 Cursor 额度。

Cursor 使用 SDK 原生自定义工具回调连接 Rust Node。产品链路不启动 MCP Server，也不要求 Bot 或浏览器支持 MCP。协调工具限定到当前个人网络及机器人聊天，本机工作使用 SDK 宿主工具。Agent 不能替本人授权自动后续。

Node 只常驻一个 Cursor SDK Bridge 进程供各轮回复复用。已设置 `CURSOR_API_KEY` 时，Node 启动或保存 Cursor 设置后立即预热。Bridge 退出或无响应时，下一轮按 1 至 30 秒退避重启；某一轮结果未知时，Bridge 等其余轮次结束后退出，下一轮换新进程。启动超过 60 秒时这一轮不会开始，稍后再发即可。

### 使用 Codex SDK

Codex Bot 的回合在 Codex App Server 上运行。装好并登录 Codex 后，`init` 从本机找出 Codex 目录、App Server socket 和会话目录并写进配置，找法见[部署配置](deployment.md#配置-sdk-与-app-server)；`--codex-socket` 可直接指定。

```sh
./vibeface init \
	--data-dir "$VIBEFACE_DATA_DIR" \
	--root /absolute/path/to/projects \
	--manager codex \
	--codex-socket '<App Server socket 的绝对路径>' \
	--workspace /absolute/path/to/manager-workspace
```

可用 `--model` 指定模型，省略时使用 Codex 配置中的默认模型。

Bot 通过本机 `vibeface tool` 命令调用限定到当前网络和聊天的协调工具，工具令牌和地址经线程的 Shell 环境传入，Provider 凭证不放进 Shell 环境。Cursor SDK 与 Codex 的工具运行方式不同，不承诺相同的系统隔离。

### 使用 Claude Code、pi 或 opencode

三者都像 Codex 一样经 `vibeface tool` 调用协调工具，Bot 和执行任务都能用。Node 在本机运行它们，不安装、不登录。先在同一用户下装好并登录，或在该服务的**设置**里填模型密钥变量。

| 产品 | 程序 | 登录 | Node 怎么运行 |
|---|---|---|---|
| Claude Code | `claude` | `claude` 里的 `/login`，或变量 `ANTHROPIC_API_KEY` | 每轮和每个任务一个 `claude -p` 进程，`bypassPermissions` |
| pi | npm 包 `@earendil-works/pi-coding-agent` 的 `pi` | pi 里的 `/login`，或所用服务的密钥变量 | 每轮和每个任务一个 `pi --mode rpc --no-extensions` 进程 |
| opencode | `opencode` | `opencode auth login`，或所用服务的密钥变量 | 回环地址上的一个 `opencode serve`；Node 建的会话允许全部工具、拒绝提问，规则排在本人配置之后。子 Agent 的会话不带这些规则 |

程序先在 Node 的 PATH 里找，再找 `~/.local/bin`、`~/.claude/local`（Claude Code）、`~/.opencode/bin`（opencode）、`/opt/homebrew/bin` 和 `/usr/local/bin`。后台服务没有交互式 shell 的 PATH：nvm 或自定义 npm 前缀装的 pi 要在设置里填绝对路径，并在 pi 的变量里设含 `node` 所在目录的 `PATH`。

模型留空时用各自的默认模型；opencode 写成 `provider/model`。Claude Code 的进度按整条消息更新，Bot 回复中收到的新消息等这轮结束再处理。

`dispatch_work` 的 `thread` 只接受本机任务跑过的 Claude Code、pi 或 opencode 会话；本人自己开的会话不接任务。opencode 会话里还有未结束的工作（在跑、发送结果未知或待确认）时，新工作以 `writer_busy` 被拒，不发送；能确定是哪项工作占用时，错误里点名它。不给 `agent` 时沿用该会话里工作用过的产品。

### 回答本人自己开的会话

`vibeface hooks install --home <home>` 写入各产品的监听入口，已打开的会话重启后加载：

| 产品 | 写入 | Bot 能回答 |
|---|---|---|
| Codex | `.codex/hooks.json` | 只上报；App Server 上的请求由 Bot 直接回答 |
| Claude Code | `.claude/settings.json` 的 hooks | 权限请求和提问 |
| pi | `.pi/agent/extensions/vibeface.ts` | 一轮结束后替本人发下一条消息；本人已接着输入时丢弃 |
| opencode | `.config/opencode/plugins/vibeface.ts` | 权限请求和提问 |

Bot 须在 30 秒内回答。超时后请求交回原程序，由本人在终端处理。

## 添加自己的设备

每个 Node 和每个浏览器有独立密钥，设备编号就是设备公钥。本人设备列表来自已签名的成员目录，设备名称、IP 和中继返回的地址不证明所有权。

在管理设备选择**添加设备**并点**生成链接**，得到一条 `https://<gateway>/join#vibeface1…` 加入链接和一个二维码。入口有公网 HTTPS 地址时，二维码就是这条链接，手机相机扫码即可打开；只有本机或局域网地址时，二维码是 `vibeface://join?link=vibeface1…`，下方提示“用手机 App 扫码”，用 Vibeface App 扫码加入。链接含一次性 secret，只交给自己的设备。新浏览器打开链接，确认名称后显示 6 位安全码和“等待批准”。管理设备上出现“X 想加入”和同一个安全码：一致就点**一致，允许**，浏览器可同时勾选**设为管理员**；不一致或不认识的请求点**拒绝**，该请求随即作废。只有发出链接的面板会自动开始比对，其他管理页面需先点**显示安全码**。新设备伪造或替换随机数时请求也会作废。

App 扫这种二维码后，按邀请里写的局域网地址直接连发出邀请的 Node，手机要和这台电脑在同一局域网，见 [手机 App 局域网直连](#手机-app-局域网直连)。手机浏览器打不开本机或局域网地址；要在外网用手机浏览器加入，见 [配置公共入口](#配置公共入口)。`localhost` 与 `127.0.0.1` 仅指向各自本机。

添加另一台 Node 时，在新机器上先用 `vibeface init` 初始化（不要在它的本机网页上建网），再执行：

```sh
./vibeface join --data-dir "$VIBEFACE_DATA_DIR" --name "<设备显示名称>" "https://gateway.example/join#vibeface1…"
```

命令显示 6 位安全码并等待批准；在管理设备上比对安全码一致后点**一致，允许**，命令完成加入，再启动新 Node。`--name` 必须明确指定设备名，不读取系统主机名。连接中断时用同一链接和目录重试，结果未知时先核对原请求状态，不重新创建请求。

任何管理设备都能撤销设备，也能在设备详情页把已加入的浏览器或手机设为管理设备或取消，至少保留一台。浏览器或手机在设置里断开时会撤销自己并删除本机设备密钥，之后要用加入链接作为新设备加入；唯一的管理设备不能这样断开，需先把另一台设备设为管理设备。Node 不代替本人签署授权，Agent 不能添加设备、扩大权限或撤销设备。浏览器设备私钥不可导出，清除站点数据后需要重新加入。

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

## 手机 App 局域网直连

手机 App 和 Node 在同一局域网时，不经网页入口，直接用 iroh QUIC 连这台 Node，再在这条连接上做和网页入口相同的 Noise 握手，由设备密钥证明对方就是要连的 Node。App 每次启动换一个临时的 QUIC 密钥，不用 relay。网页没有这条路径。

- App 在前台时收听 Node 的 mDNS 广播（见 [Node 之间的连接](#node-之间的连接) 的“局域网发现”），用个人网络编号和已知 Node 的设备编号比对标签，认出本人的 Node 和它当前的地址；退到后台就停止。
- 重连顺序：当前 Node 的局域网直连、当前网页入口、局域网里看到的其他本人 Node、其他网页入口。局域网直连每次最多等 2 秒，只剩一种可试时等 10 秒。拨号会在 TLS 里明文带上对方的设备编号，所以 App 不盲拨没看到的地址；只有没有任何网页入口可试时，才按上次连上或 Node 签名记录里的地址试一次。
- 已经经网页入口在线时，一发现本人的 Node 出现在局域网，就先试连一次。成功后最多等 30 秒让进行中的请求返回，再切到直连；仍有请求未返回、正在做需要确认的操作或试连失败时，保持当前连接。
- 扫码加入时，App 先按邀请里 Node 的直连地址（局域网地址在前）连，连不上再试邀请里的网页入口。
- 直连时连接状态显示“局域网直连”。

Node 不需要额外配置，只要已加入个人网络、不是公共入口、`bind` 不是具体地址，防火墙放行局域网内 UDP 5353 组播和 iroh 的 UDP 端口。

iOS 第一次在局域网里查找或连接电脑时，会询问是否允许 Vibeface 访问本地网络。不允许时 App 只能走网页入口；之后可在 iOS **设置 > 隐私与安全性 > 本地网络**里打开。

## 配置公共入口

手机或另一台电脑要从外网打开网页，需要一个 HTTPS 入口。有两种做法：

- **公共入口**：在有公网地址的机器上运行一个不加入个人网络的 `vibeface`，它默认只提供网页、relay 和密文转发，不保存个人网络的事件或附件。个人设备不需要入站端口。推荐用这种方式。
- **成员 Gateway**：让公网机器上的 Node 作为普通成员加入个人网络，同时提供网页，见本节末尾。

### 公共入口：Linux 服务器加 nginx

下面以一台 Linux 服务器为公共入口，域名为 `vb.example.com`。个人电脑运行加入个人网络的 Node。

1. 域名解析到服务器的公网地址，用 certbot 或其他方式为它签发证书。

2. 在服务器上初始化公共入口并启动。网页监听 `127.0.0.1:4790`，relay 监听 `127.0.0.1:3340`，两者都只对 nginx 开放：

   ```sh
   export VIBEFACE_ENTRY_DATA_DIR='<入口 Node 数据目录的绝对路径>'
   vibeface init --data-dir "$VIBEFACE_ENTRY_DATA_DIR" \
   	--public-entry \
   	--public-url https://vb.example.com \
       --relay-listen 127.0.0.1:3340 \
       --relay-url https://vb.example.com
   vibeface serve --data-dir "$VIBEFACE_ENTRY_DATA_DIR"
   ```

   `init` 在数据目录生成运营者令牌 `entry-token`（权限 600）并打印它的路径。令牌决定谁能把设备挂到这台 relay 上，不要放进配置、聊天或代码仓库。确认能用后按 [作为后台应用运行](#作为后台应用运行) 安装服务。

3. nginx 把 relay 的三个路径转给 relay 端口，其余转给网页端口，两者都允许 WebSocket 升级：

   ```nginx
   map $http_upgrade $connection_upgrade {
   	default upgrade;
   	''      close;
   }

   server {
   	listen 443 ssl http2;
   	server_name vb.example.com;
   	ssl_certificate     /etc/letsencrypt/live/vb.example.com/fullchain.pem;
   	ssl_certificate_key /etc/letsencrypt/live/vb.example.com/privkey.pem;

   	location ~ ^/(relay|ping|generate_204)$ {
   		proxy_pass http://127.0.0.1:3340;
   		proxy_http_version 1.1;
   		proxy_set_header Upgrade $http_upgrade;
   		proxy_set_header Connection $connection_upgrade;
   		proxy_set_header Host $host;
   		proxy_read_timeout 1h;
   	}

   	location / {
   		proxy_pass http://127.0.0.1:4790;
   		proxy_http_version 1.1;
   		proxy_set_header Upgrade $http_upgrade;
   		proxy_set_header Connection $connection_upgrade;
   		proxy_set_header Host $host;
   		proxy_read_timeout 1h;
   	}
   }
   ```

   `Host` 必须是 `public_url` 的主机名，Node 会拒绝其他 Host。

4. 在个人电脑上指定服务器的 SSH 地址、步骤 2 选择的数据目录，以及本机用于接收令牌的临时文件，再复制 `entry-token`：

   ```sh
   export VIBEFACE_ENTRY_SSH='<SSH 用户>@<入口域名>'
   export VIBEFACE_ENTRY_DATA_DIR='<步骤 2 的服务器数据目录绝对路径>'
   export VIBEFACE_RELAY_TOKEN_FILE='<本机临时令牌文件的绝对路径>'
   scp "${VIBEFACE_ENTRY_SSH}:${VIBEFACE_ENTRY_DATA_DIR}/entry-token" "$VIBEFACE_RELAY_TOKEN_FILE"
   chmod 600 "$VIBEFACE_RELAY_TOKEN_FILE"
   ```

5. 在个人电脑上初始化 Node 时指向公共入口，然后启动：

   ```sh
   vibeface init --data-dir "$VIBEFACE_DATA_DIR" \
   	--root /absolute/path/to/projects \
   	--entry https://vb.example.com \
       --relay-token-file "$VIBEFACE_RELAY_TOKEN_FILE"
   vibeface serve --data-dir "$VIBEFACE_DATA_DIR"
   ```

   `init` 把令牌复制进 Node 的数据目录，之后可以删掉 `$VIBEFACE_RELAY_TOKEN_FILE` 指向的临时文件。relay 不在入口同一个 origin 上时，用 `--entry-relay URL` 另外指定。

6. 在个人电脑上打开 `http://localhost:4790` 建网。之后**添加设备**生成的链接和二维码都指向 `https://vb.example.com/join#…`。手机扫码打开后确认名字，在个人电脑上核对 6 位安全码并点**一致，允许**。也可以在手机上打开 `https://vb.example.com`，点**扫码**扫描**添加设备**里的二维码。

默认公共入口只搬运浏览器与个人电脑上的 Node 之间的 Noise 密文，看不到对话内容，也不保存个人网络事件或附件。另行启用文件交付服务时，它还会独立暂存交付密文，见[可选文件传输](#启用可选文件传输)。它的 relay 只接纳持有令牌的设备，持错误令牌的 Node 连不上。一台公共入口可以同时服务多台带令牌的 Node，各人的个人网络互不相通。令牌泄露时，停掉入口和使用它的 Node，用同一份新令牌（`openssl rand -hex 32` 生成的 64 个十六进制字符）替换它们数据目录里的 `entry-token`，再依次启动。

已经初始化的 Node 要改用公共入口，停掉它，在 `config.json` 的 `network` 中加入 `"entry": {"url": "https://vb.example.com", "relay": "https://vb.example.com"}`，把令牌放到数据目录的 `entry-token`（权限 600），再启动。

### 成员 Gateway

公网机器上的 Node 也可以作为普通成员提供网页：

1. 准备域名并解析到这台机器，例如 `vibe.example.com`。
2. 用 Caddy 或 nginx 签发证书，把 `https://vibe.example.com` 整站反代到本机 `127.0.0.1:4790`，并允许 WebSocket 升级。Caddy 只需一行：`vibe.example.com { reverse_proxy 127.0.0.1:4790 }`。
3. 在这台 Node 的 `config.json` 设置 `"public_url": "https://vibe.example.com"`，重启。
4. 用 `vibeface join` 让这台 Node 加入个人网络，之后管理设备生成邀请时会使用它的公网地址并显示二维码。

成员 Gateway 保存个人网络的完整副本。反代只转发公开 listener，不要转发本机会话的 `local_listen`。浏览器只经 `/api/client` 跨源连接，其他路径仍要求同源。网页中的**怎么设置**链接指向本节。

## 不提供网页

默认启动网页。只运行节点传输、同步、Agent 与本地工具时，执行 `vibeface serve --data-dir /absolute/node-data --no-web`。直接启动同样支持 `vibeface --data-dir /absolute/node-data --no-web`。

`--no-web` 不绑定浏览器公开 listener，不提供网页、浏览器会话或浏览器转发。本地 hook、分诊、日志、SDK 工具和维护接口继续运行。其他设备仍可经个人网络连接这个 Node。关闭后不发布本机网页入口；已配置的远端公共入口仍可访问这个 Node。

后台服务要保持关闭网页时，在 `init` 时传入 `--no-web`，或把 `config.json` 的 `web_enabled` 设为 `false` 后重启。直接启动和 `serve` 的开关只影响本次运行。

## Node 之间的连接

Node 之间经 [iroh](https://www.iroh.computer/) QUIC 互连，设备编号就是 iroh 的 `EndpointId`，连接由 QUIC TLS 证明对端身份，再按成员目录授权。非成员的连接被拒绝。不使用 n0 的 DNS 和 pkarr 地址发布，成员 Node 的持久地址来自邀请和个人网络内部交换的签名地址记录。同一局域网内的 Node 还经 mDNS 互相找到当前地址，见下文“局域网发现”。显式配置发现服务后，还可查询临时直连地址。

每台 Node 定期为自己签一条地址记录，内容是 iroh 地址（relay URL 和直连 IP）以及网页入口 `gateway_url`。同步时双方交换记录，地址变化后几秒内传到其他成员。浏览器从当前入口读取这些记录，自己验签后作为可切换的访问地址，见下文“多个访问地址”。

**局域网发现**。路由器给 Node 换了 IP 后，其他 Node 手里的地址记录要等下一次同步才更新，没有 relay 时双方可能再也连不上。所以成员 Node 在局域网用 mDNS 广播 `_vibeface._udp` 服务，同时收听其他 Node 的广播。广播的实例名、主机名和 TXT 记录 `t` 都是同一个标签，由个人网络编号、设备编号和当前小时算出，每小时更换，不含设备名称、设备编号或网络编号。成员 Node 用自己的成员目录比对标签，前后一小时的标签也算数，以容忍时钟误差。比对上的广播地址只作为拨号提示，和地址簿里的地址一起交给 iroh，并让这台 Node 立即和对方同步一次。对端身份仍由 QUIC TLS 证明。旁人可以重放标签，所以提示只收私网地址，同一 Node 只用最新一条广播的最多 4 个地址，广播表满时先删不属于成员的，再删最早收到的。本机网卡地址变化后几秒内重新广播；对方发出 mDNS 告别或约两分钟不再应答后，提示随即删除。

公共入口和把 `bind` 设为具体地址（例如 `127.0.0.1`）的 Node 不启用局域网发现，尚未加入或已被移出个人网络的 Node 不广播。mDNS 无法启动时只记一条警告，Node 照常用其他地址连接。防火墙需放行局域网内 UDP 5353 端口的组播（IPv4 `224.0.0.251`、IPv6 `ff02::fb`）和 iroh 的 UDP 端口。

两台 Node 不能直连时经 relay 转发。relay 只转发 QUIC 密文，只接纳当前成员 Node 和持有本 Node 所发邀请的加入者，默认不使用 n0 公共 relay。网络设置只在 `init` 或首次直接启动时写入 `config.json`，修改后重启生效：

| 参数 | `config.json` 中的 `network` 字段 | 作用 |
|---|---|---|
| `--relay <URL>` | `relays` | 个人网络中另一台 Node 的 relay，可重复，最多 16 个 |
| `--relay-listen <ADDR>` | `relay_listen` | 普通模式运行 TCP relay，`discovery-only` 模式提供 UDP 发现服务 |
| `--relay-mode <MODE>` | `relay_mode` | 默认 `forward`；`discovery-only` 只提供连接协助，需要 `--public-entry` |
| `--discovery <URL>` | `discovery` | 钉住身份的发现服务，格式为 `vibeface://<device-id>@<host>:<UDP-port>`，最多 4 个 |
| `--relay-url <URL>` | `relay_url` | 其他 Node 访问本 relay 的 URL，必须明确提供其他 Node 可访问的 URL |
| 无 | `bind` | iroh 的 UDP 监听地址，默认在 `listen` 的端口上监听全部 IPv4 和 IPv6 网卡 |
| 无 | `n0_relays` | 同时使用 n0 公共 relay，默认关闭 |

公网 Node 承担 relay 角色。例如在服务器上用反向代理把 `https://relay.example` 转到本机 `127.0.0.1:3340`，并允许 WebSocket 升级：

```sh
vibeface init --data-dir "$VIBEFACE_DATA_DIR" \
	--relay-listen 127.0.0.1:3340 --relay-url https://relay.example \
	--public-url https://vibe.example
vibeface join '<管理设备给出的加入链接>' --name '<设备显示名称>' --data-dir "$VIBEFACE_DATA_DIR"
```

个人设备上的 Node 用 `--relay https://relay.example` 初始化，无需入站端口。邀请链接写明发出邀请的 Node 自己的 relay。relay 只认本 Node 登记的邀请，所以只能经 relay 连通的新 Node 要用运行 relay 的那台 Node 发出的邀请加入。

公网 Node 也是普通成员，可同时运行调度或执行。relay 不读取会话内容，也不给其他设备增加任何权限。不想让公网机器保存个人网络数据时，改用 [公共入口](#配置公共入口)。

历史按每台设备一条签名日志同步：双方交换版本向量，只拉对方缺的区间，入库前逐条验签并去重，中断后从已入库的位置续拉。对端重启后，它发来的同步请求带新的启动编号，本机随即关闭旧连接并重新同步，不等 QUIC 空闲超时。同步只写入历史和投影，不会重新执行历史中的操作；调度和执行只发生在被指定的 Node 上，见[事件 DAG 协议](node-protocol.md#事件-dag)。

网页按 1 MiB 分块把附件上传到当前入口 Node，入口重启后从已落盘的位置继续，消息只引用附件的 hash 和大小。附件下载的已收部分保存在本机私有暂存文件。断线或重启后，Node 重新核验授权并从已保存的位置继续；只有最终大小和 hash 校验通过的完整附件才能使用。暂存最多占用 256 MiB、64 个对象。连接恢复只继续传输，不重发业务操作。

## 启用可选文件传输

文件交付默认关闭，不影响已有会话同步和附件。节点发现免费，公益转发不需要钱包。付费交付只供本机 `31337` 测试链使用，不要转入真实资产。验收条件、费用和恢复边界见[文件交付实验参考](paid-delivery.md)。

S 是发送 Node，R 是接收 Node，P 是服务 Node。

1. 在 S、R 各自的可信管理客户端打开**设置 → 发送和接收文件**，在**传输设置**开启**启用文件传输**和**公益转发**。本机自动访问会话只能查看，须使用已加入个人网络且有管理权限的客户端操作。
2. 在 P 上启用相同服务。P 可以是不入个人网络的公共入口。公共入口没有管理网页，通过本机所有者命令应用以下 JSON 文件。命令要求对应 Node 正在运行。

   ```json
   {
       "enabled": true,
       "charity_enabled": true,
       "paid_enabled": false
   }
   ```

   ```sh
   vibeface wallet --data-dir /path/to/node-data configure /path/to/delivery-charity.json
   ```

3. 确保 S、R 和 P 能按 PeerID 建立连接。沿用已有地址和 relay，或在停掉 Node 后为 `config.json` 的 `network` 增加明确选择的 pkarr 发现目录，再重启。以下设置发布 relay 地址并解析 PeerID，默认不发布直连 IP。

   ```json
   "pkarr": {
       "publish": true,
       "resolve": true,
       "urls": ["https://discovery.example/pkarr"],
       "publish_direct": false
   }
   ```

   发现目录只返回签名地址，不转发文件，也不收取协议服务费。启用发布会公开这台 Node 的 PeerID 和所发布的地址，请使用自己选择和维护的目录。

4. 在 S 的传输页面选择文件，在**接收方的连接 ID**和**中转设备的连接 ID**分别填写 R、P 的 PeerID，选择**公益转发**，点击**下一步**，核对后点击**确认发送**。当前原文件上限为 130816 字节。
5. 在 R 的传输页面选择该订单，点击**接收文件**，完成后点击**保存文件**。Web 把已核对文件交给浏览器保存，Mobile 写入并再次核对后提供分享副本。

断线或进程重启后，保留原订单并点击**检查进度**。不要通过另建订单处理未知结果。公益收据不表示链上解密证明，也不表示真人阅读。

付费实验还需按[proof 构建说明](../proofs/delivery/README.md)配置 Linux prover，并按[合约说明](../contracts/delivery/README.md)固定本机测试部署。仅切换网页开关不能创建这些配置。完整进程验收见[e2e 的付费模式](e2e.md#验证付费交付和实际结算)。

在已启用服务的本机 Node 上管理独立钱包，口令由 TTY 关闭回显输入。以下命令不接受网页远程口令。

```sh
vibeface wallet --data-dir /path/to/node-data create
vibeface wallet --data-dir /path/to/node-data unlock
vibeface wallet --data-dir /path/to/node-data status
vibeface wallet --data-dir /path/to/node-data lock
vibeface wallet --data-dir /path/to/node-data backup /path/to/new-wallet-backup.json
```

自动化可用 `--passphrase-fd` 从所有者私有文件或管道读取口令。不要把口令写进命令参数或环境变量。钱包每次重启都保持锁定，损坏时显示 `unavailable`，只能在本机从独立备份恢复，不能覆盖创建。

钱包备份和个人恢复文件都不包含订单接收私钥。停掉 Node 后备份完整数据目录，尤其是整个 `delivery/`。注册、轮换和撤销接收密钥的命令及原意图重试规则见[接收密钥和恢复材料](paid-delivery.md#接收密钥和恢复材料)。
## 运行不转发业务数据的公共发现服务

运营者可只提供发现和连接协助，不加入用户的个人网络，也不承担业务流量。初始化时选择 `discovery-only`：

```sh
vibeface init --data-dir /srv/vibeface-discovery \
	--public-entry --relay-mode discovery-only \
	--relay-listen 0.0.0.0:4792 --no-web
vibeface serve --data-dir /srv/vibeface-discovery
```

`init` 输出发现服务的设备编号。UDP 4792 必须能从公网直接访问，不能用 HTTP 反向代理替代。监听地址的端口必须非零；使用 `0.0.0.0` 或 `[::]` 时同时尝试监听 IPv4 和 IPv6。此模式不使用 `--relay-url`，也不生成 `entry-token`。省略 `--no-web` 时仍提供静态网页，但不提供 `/api/client`、`/api/decision` 或建网入口。

用户在自己的 Node 上明确配置这个发现服务的编号和公网地址：

```sh
vibeface init --data-dir /absolute/personal-node \
	--discovery 'vibeface://<发现服务设备编号>@discovery.example:4792'
```

已有 Node 可在 `config.json` 的 `network.discovery` 数组里添加同一 URL 后重启。发现服务的编号通过 QUIC TLS 校验，域名解析结果不能冒充运营者。不要把这个地址传给 `--relay` 或 `--entry`，它没有业务转发能力。

Node 用自身的业务 UDP 端口登记。发现服务只交换它从已认证连接上观察到的地址，并通知双方发起直连探测。服务端没有转发客户端数据报的实现，也不接受个人网络的业务 ALPN。加入设备仍需有效邀请，读取、同步和执行仍按原有成员权限校验。

发现服务不会列出所有用户；知道设备编号的人可以查询它是否在线及其公网地址。运营者能看到设备编号、UDP 地址和查询关系。地址登记只保存在内存中，连接租约到期或断开后移除，不保存会话、附件、事件或个人网络。每个服务最多接纳 512 条控制连接，每个来源 IP 最多 8 条；单连接最多处理 60 个固定请求，租约为 60 秒，单帧最多 512 字节。连接通知队列和并发探测也有上限。

仅发现不保证所有 NAT 类型都能直连。对称 NAT、UDP 被封锁或双方没有可用直连路径时，仅发现会报告不可达。需要这类网络仍可自动连接时，用户可另行配置普通 `--relay`。Node 自动选择直连或普通 relay；发现节点始终不承载业务流量。本项目不默认连接任何官方发现服务。

## 多个访问地址

每台启用 Gateway 的 Node 都是同一个人网络的一个访问地址。浏览器连上任意一台后，会经它连到目标 Node：Noise 握手仍在浏览器和目标之间完成，中间的 Gateway 只转发密文；目标不是本网络成员时，Gateway 回“目标不在我的设备里”。

左下角的连接状态显示在线、切换中或离线，点开可看到当前访问地址和其他可切换的地址，每个地址标明“公网”或“仅本机或局域网”。只有带公网 HTTPS `gateway_url` 的地址算公网，邀请的二维码只用公网地址。

当前访问地址断开时，页面先重试它，再按最近成功的顺序试其他地址，不需要重新加入或刷新。还没确认送达的消息保存在浏览器 IndexedDB 中，切换后用原来的签名原样重发，Node 按消息编号去重，调度 Node 只执行一次。同一浏览器的多个标签页由其中一个负责重发和探测入口，其他标签页跟随它切换。页面所在 origin 的 Gateway 不可达时，刷新后应用外壳从 service worker 缓存打开，再经其他访问地址恢复会话。

成员 Gateway 的 origin 自动被其他成员的 `/api/client` 接受，无需逐个写进 `allowed_origins`。撤销一台 Node 后，其他 Gateway 不再转发到它，浏览器也不再把它列为访问地址。

## 创建机器人并处理结果

连接自己的设备后，创建一个有名称的机器人，按需填写职责和项目。进入机器人即可直接聊天，每个机器人保留自己的长期历史。创建机器人只保存定义，不启动模型；首次发消息时，程序选择已授权且可用的聊天 Agent。

机器人定义、聊天历史和本机 SDK 会话分开保存。机器人项目配置只是执行偏好，不授予目录或设备权限；执行时仍检查 Node 真实授权。多个机器人可以使用同一 Node 的 SDK 配置，但不共用聊天历史或本机 SDK 缓存。设备、连接和恢复在设置中。

普通查询可以直接回答，本机工作使用 SDK 的宿主工具，远端工作通过 Agent 任务消息委派。日常操作不需要创建任务单、打开执行会话或选择调度器。进展、补充输入和结果都回到原机器人的聊天。

会话的调度设备由本人在建会话时签名指定，每次更换都写一条带本人签名、epoch 加一的负责人记录（`set_owner`）。调度设备离线而当前连接的设备能运行管理 Agent 时，聊天里出现“X 离线了，这个对话在等它”，点**交给这台设备处理**即可换过来，排队的消息在新设备上继续回复。新调度设备只按共享记录和上下文检查点重建上下文，不重放已经执行的工作。旧调度设备看到更换后会停下正在进行的回复，并拒绝它之后的工具调用。每个定时时刻由当时的调度设备在共享记录里领取，全网只运行一次；错过太久而跳过的时刻也会领取，换了设备也不会补跑；换过来之前已经到点的时刻留给旧设备，新设备只跑之后的。

发送后先看状态。`pending_delivery` 表示入口已保存消息，正在投递给原调度设备；只有目标持久入队后才是 `queued`。排队不表示模型已开始，`running` 才表示已跨过 SDK 启动边界。目标离线、能力不满足或回执丢失时，保留原目标与消息身份核对，不自动改派另一台设备。

管理 Agent 忙时，追加消息进入持久队列。尚未开始的本人消息优先于工作结果等后续轮次，同一调度 Node 上的同一会话一次只运行一个管理轮。管理轮派发工作后即结束，不等待工作；工作结束时它的结果作为新的一轮入队，Agent 汇报并可继续派发。关闭网页不会停止 Node 上的工作，但执行设备关机、休眠或断网会影响推进。

需要停止时，区分以下动作：

- **停止当前回复**只请求取消当前管理轮，并撤销该轮的 Node 工具权限。已输出的部分回复保留为一条标记“已停止”的回答。取消待确认时不能当作已经停止，也不停止已派发的工作。
- **停止工作**作用于选中的工作，中断它的原生 turn，并一并停止它委派出的工作。任何入口都能停止其他设备上的工作。停止请求先存在本机再发送，直到工作结束都不会放弃：目标离线时入口以 3 秒起、最长 1 分钟的间隔持续投递，设备上线后照样收到并停下；执行设备在 `turn/interrupt` 失败或重启后先读回合再重发，回合已结束就不再中断。页面显示“停止中”，30 秒仍未确认显示“还在尝试停止”。执行器接受中断后 30 秒回合仍未结束时再读一次：仍在运行就再发，读不到则进入待核对。

Cursor SDK 和 Codex 都提供进度和取消请求。Codex Bot 的回合在 App Server 上运行，回复进行中再发的消息直接并入当前回复（`turn/steer`），网页显示“Bot 正在看这条补充”，并入后显示“已补充给 Bot”，该回复未正常完成时显示“补充后回复没完成”，无法确认时显示“不确定 Bot 看到了没有”；Cursor 没有原生 steer，新消息排在当前回复之后。Codex 线程创建时把本轮的工具令牌和本机工具地址写进 Shell 环境，之后续接同一线程的回合沿用这个令牌，令牌在该线程每轮都有效（接受的取舍）；Node 重启时沿用上次的工具端口（记在数据目录的 `tool-port`），端口被占用而换了地址时 Bot 开新线程。Cursor 需要匹配的 Provider 终态或受管进程退出证据才能确认取消。Codex 停止时发 `turn/interrupt`，确认回合已结束才回报已停止；无法确认时该轮进入 `interrupted` 并列入待核对。停止回复不影响已派发的工作，停止工作是单独操作。

Agent 通过 Node 的 `dispatch_work` 工具派发工作，例如 `vibeface tool dispatch_work --json '{"title":"跑测试","prompt":"...","cwd":"...","call_ref":"REF","target_node":"<设备编号>"}'`。`title` 是给本人看的几个字，任务卡片以它为名，`prompt` 放在详细信息里。省略 `target_node` 时派发到本机，由 `agent` 指定的产品执行（本机只开了一个执行产品时可省略）：Codex 工作由 CLI 先向 Node 登记，再直接连本机 App Server 开线程、以完全访问（`approvalPolicy=never`、`danger-full-access`）发一个 turn，Node 记录并观察它。`target_node` 是另一台设备时，任务交给那台设备的协调 Agent，作为它的一轮执行，由它在本机派发，不能再向其他设备派发。`continues` 填原工作编号时，在原工作的 thread 上继续；`thread` 填本机已有线程时，消息进入该线程，与 `continues` 互斥。目标 Node 核对派发方仍是成员且处于进行中的调度轮次，并要求 `cwd` 位于目标自己 `config.json` 的 `roots` 内，不满足时拒绝，页面显示“未接收”。同一轮用同一 `call_ref` 重试得到同一工作，不会启动第二个 turn；结果不明时用 `call_lookup` 查询。

协调 Agent 的 Shell 完全访问本机，可以改文件、跑 git 和 gh。它用 `vibeface codex call <method> [json]` 调用 App Server 的读取和线程管理方法，开始、引导或停止回合的方法（`turn/start`、`turn/steer`、`turn/interrupt`、`thread/start`、`thread/fork`、`thread/compact/start`、`thread/shellCommand`、`thread/revert`、`thread/inject_items`、`review/start`）一律拒绝，改用 `dispatch_work`、`codex send` 和 `interrupt_work`，让每个回合都是 Node 登记过的工作；`vibeface codex send <thread> <text> --id <id>` 给已有会话插话（回合进行中时 steer，否则开新回合）。这条消息是本轮的一项工作，与 `dispatch_work` 相同：发送前向 Node 登记，`--id` 即 `call_ref`，Node 观察它所在的回合，结束后结果作为 `work_result` 轮回到 Bot；同一轮同一 `--id` 再次执行只回读、绝不重发，换了内容或会话则拒绝。会话的目录取自线程，不受 `roots` 限制。它只能在 Bot 的一轮里运行，因为它要用这一轮的工具令牌；Cursor 的 Shell 没有这个令牌，Cursor 回合用带 `thread` 的 `dispatch_work` 工具做同样的事。`vibeface codex watch <thread>` 查看事件，`vibeface log <level> <message> --conversation <id>` 把它遇到的问题写进 Node 日志（经本机监听器的 `/api/log`，边界与 `/api/hooks` 相同）。Socket 路径在 `VIBEFACE_CODEX_SOCKET`。

Codex 默认 bypass，正常不会发起请求。仍发起的请求（审批、提问、登录等）由 Node 挂起并立即唤醒协调 Agent：它用 `vibeface requests` 查看，`vibeface respond <request_id> '<result-json>'` 回答，同一请求只回答一次。需要本人拍板时，它用 `vibeface triage raise --event-id <ID> --request-id <请求> --question <问题> --choice 批准 --choice 拒绝` 在拥有该会话的 Bot 聊天里提问，按项目设置也经 webhook 转发 Grok Bot。提问挂住请求直到有回答；回答作为一轮交给该 Bot，它在 120 秒内用 `vibeface respond <请求> '<result-json>' --node <设备>` 应答。没有协调 Agent，或它 120 秒内既没回答也没提问时，Node 按方法拒绝并记录。等待回答的 hook 挂起的请求同样经 `vibeface requests` 和 `vibeface respond` 处理，30 秒内没有回答时交回会话自己决定。没有协调 Agent 时分诊事件直接丢弃。不是 Bot 发起的交互会话由运行它的设备归给主 Bot；Bot 每轮能看到名下的会话，自己判断后用 `hand_over_session` 转给其他 Bot 或不再关注；`codex exec`、子代理等后台会话不处理，见 [聊天里的提问与回答](design/questions-in-chat.md)。

Node 默认不启用 Codex。启用 Codex Bot 或任务执行时，缺的路径由 Node 从本机找出并写进配置，设备设置里显示并可修改；Bot 也能用 `propose_provider_config` 提议路径、打开执行或添加可用文件夹（`roots`），经聊天里的「允许 / 不允许」卡片同意后由 Node 写入。首次设置时打开 Codex 运行 Bot 会同时打开执行任务，并可一键添加家目录下常见的项目文件夹（`node_settings.suggested_roots`）。`providers.codex_app_server.mode` 为 `system` 时只连接配置的 socket，不启动也不结束服务，见 [Agent 直连 App Server](design/agent-direct-app-server.md#21a-用哪个-app-server)。设为 `spawn` 时 Node 托管自己的 `codex app-server`：用 `bins.codex` 启动它，Socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix Socket 路径长度限制。App Server 运行在独立进程组，进程组号写入数据目录的 `codex-app-server.pid`。Node 启动时先结束上次遗留的进程组，正常退出时一并停止它，异常退出则退避重启。要指定系统 App Server 的 socket，设置 `providers.codex_app_server.socket`。启动网页成功不等于程序已安装、已登录或模型可用，应查看聊天中工作的实际状态。

App Server 重启或重连后，Node 只读复查未结束的工作，不重发旧指令。读不到原 turn 的工作标为“待核对”，由本人附证据结清。

普通执行的终态结果自动送回原聊天，协调 Agent 以完整权限汇报，可以继续派发工作。

普通进度和通知不调用管理模型。`daily_decision_limit` 只限制当前 Node、当前个人网络在 UTC 当天非本人触发的管理调用，本人主动聊天不消耗这个额度。失败的调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。额度耗尽时，原始结果和通知仍保留。

Codex 结束一轮不等于交付目标已经达成，由本人核对产物后确认。停止回复不会自动停止已经运行的进程，仍需核对后台工作的实际状态。

未读提醒归属原机器人聊天。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与设置中的通知权限。关闭网页后没有独立的手机推送服务。

从另一台入口打开原会话，可只读观察原管理节点和执行节点的临时进度。重连后读取持久状态，缺失的临时文字不会触发重做。缓存状态会标明新鲜度，离线时不能据此确认远端仍在运行。长文本结果可在聊天中展开或下载；尚未同步的附件需等待原副本可达，只有哈希不能补回正文。

## 待核对

网络断开或进程中断后，工作可能已经执行，但回执未保存。Node 不自动重新调用 SDK 或向其他设备重复派发，而是把无法确认的工作列入聊天中的同一个待核对列表。

管理 SDK 已返回明确结果时，Node 先将最终结果保存到本机。历史依赖暂未补齐或最终事件暂时写入失败时，只补发最终记录，不重新调用 SDK。

管理轮重启或 SDK 进程退出无法确认时，原轮进入终态 `interrupted`，不重跑原触发，也不冻结会话。管理 SDK 的文件系统只读、网络仅限本机回环，但未确认退出的进程仍可能经回环访问本机服务；Node 工具权限已封存，原生工作经 `dispatch_work` 登记后单独观察。因此仍登记一条管理进程待核对，关联它的进程组、原生 turn 和 Agent 任务。原生 turn 结果未知时登记原生待核对。同一工作只显示一条待核对：另一台设备代为执行的任务结果未知时，只显示该工作的一条，结清它时一并结清那一轮管理进程。

每条待核对记录有三个选择：「不确定，再查一次」只读查询原设备，管理进程项只确认进程组是否已退出，原生项补查原 turn；「做完了」和「没做完」由本人检查实际进程、项目产物和外部副作用后提交，备注可选。工作的选择带本人签名，成为该工作的结果，其他入口看到同一结果。原 turn 的结果可读时直接显示。进程运行期间对本机服务的副作用只能由本人确认。模型不能结清待核对，确认也不重放原任务。失联不能证明远端进程已停止。

A 已派发 B 后掉线时，从 C 打开原业务会话，等待 DAG 补齐，再核对 B 的实际执行状态。缺失历史、App Server 不可达和结果未知分别显示，不能通过创建同名新任务代替恢复。

同一调度 Node 对同一会话串行领取管理轮，不提供旧执行层的项目目录锁或跨设备全局文件锁。多个 Agent 需要修改同一项目时，使用独立 worktree 或独立根目录隔离。

浏览器在 IndexedDB 中保留尚未确认的消息和它的设备签名，入口把消息写进自己的签名日志并回执后删除。入口回执 `pending_delivery` 时，由入口负责继续投递给调度 Node，浏览器不再重发。回执丢失时，浏览器经同一或另一个访问地址原样重发同一请求，Node 按消息编号返回原来的结果。刷新后重试须使用原内容和汇报意图；改写正文会被拒绝，避免把新内容误当成旧操作重试。

## 作为后台应用运行

`vibeface` 是节点主进程，也是本机管理 CLI。先用前台 `serve` 确认配置，再按 Ctrl-C 停止前台进程，安装用户级服务：

```sh
vibeface service install --data-dir "$VIBEFACE_DATA_DIR"
vibeface status --data-dir "$VIBEFACE_DATA_DIR"
vibeface service status --data-dir "$VIBEFACE_DATA_DIR"
```

服务管理命令需要完整的受管安装，启动路径始终为安装根目录下的 `current/vibeface`。通过 `$VIBEFACE_BIN_DIR/vibeface` 链接调用也保留该路径，升级后不会固定在旧 release。直接运行 `cargo build` 的程序不安装服务。先使用 `update configure --install-root "$VIBEFACE_INSTALL_ROOT"` 保存部署时选择的安装根。

每个系统用户管理一个 Node 服务。各服务命令核对 `--data-dir` 与已安装服务一致，再操作该服务。省略参数时要求已设置 `VIBEFACE_DATA_DIR`。管理其他数据目录时显式传入绝对路径。

`status` 通过私有维护 socket 检查运行中的 Node，并核对设备编号，不要求安装服务。它输出设备编号、监听地址、运行版本、写入状态（`Writes`），以及每台其他成员 Node 最近一次同步是否连上、多久之前完成（`Peer`）。`status --json` 输出同样内容，不输出接入授权材料。已停止或无法连接时返回非零退出码。`service status` 则展示 launchd 或 systemd 的 Node 和更新服务状态。各项含义见 [排障](troubleshooting.md)。

```sh
vibeface status --json --data-dir "$VIBEFACE_DATA_DIR"
vibeface service stop --data-dir "$VIBEFACE_DATA_DIR"
vibeface service start --data-dir "$VIBEFACE_DATA_DIR"
vibeface service restart --data-dir "$VIBEFACE_DATA_DIR"
```

macOS 使用当前登录用户的 launchd agent，Linux 使用 `systemd --user`。Node 无论前台还是后台都把 info 级日志写到数据目录的 `node.log`（JSON lines，用 `vibeface logs` 查看）；后台运行时 macOS 的 `service.stderr.log` 和 Linux 的用户 journal 只剩崩溃输出。退出登录后仍需运行的 Linux 主机，由管理员按主机策略启用该用户的 lingering。

后台服务不保证继承终端的 PATH 或临时环境变量。在 `config.json` 使用 Provider 的绝对路径，并通过对应服务管理器配置模型凭证。安装脚本不保存或复制密钥。

卸载同时移除 Node 服务和更新定时服务，不删除身份、配置和历史：

```sh
vibeface service uninstall --data-dir "$VIBEFACE_DATA_DIR"
```

## 选择更新渠道

自动更新需要完整发布包与受管安装目录。固定的 `current` 符号链接指向当前包，Node 用户服务也通过该路径启动。首次安装方式见 [构建与发布](releasing.md#首次安装)。

选择 Stable 并开启自动更新：

```sh
"$VIBEFACE_INSTALL_ROOT/current/vibeface" update configure \
	--data-dir "$VIBEFACE_DATA_DIR" \
	--install-root "$VIBEFACE_INSTALL_ROOT" \
	--channel stable \
	--automatic true
```

将 `stable` 改为 `nightly` 可跟随开发预发布。Stable 不会安装 Nightly；尚无正式版时不会自动更换版本。切换渠道不触发降级。更新渠道和安装目录必须先配置；已有 Nightly 安装应显式选择 `nightly`。

查看策略和最近结果、主动检查或立即尝试升级：

```sh
"$VIBEFACE_INSTALL_ROOT/current/vibeface" update status --data-dir "$VIBEFACE_DATA_DIR"
"$VIBEFACE_INSTALL_ROOT/current/vibeface" update check --data-dir "$VIBEFACE_DATA_DIR"
"$VIBEFACE_INSTALL_ROOT/current/vibeface" update apply --data-dir "$VIBEFACE_DATA_DIR"
```

`check` 只查询与校验版本信息。`apply` 仅安装声明与当前数据格式兼容的包，需要安全的空闲窗口。待投递消息、排队与活动管理轮、活动任务和待核对记录都会阻止安装，先完成或核对原任务，不需要关闭网页。升级不会调用模型或修改 Provider 密钥。

开启自动更新会安装独立的用户定时服务。macOS 的 `com.leaperone.vibeface.update` 在载入时和每小时尝试更新；Linux 的 `vibeface-update.timer` 每小时启动一次独立 oneshot 服务。Linux 关机期间不能更新，用户服务能否在退出登录后运行取决于 lingering。更新日志位于数据目录的 `update.stdout.log` 与 `update.stderr.log`，文件权限为 600。`vibeface service status` 同时展示 Node 与更新服务的真实状态。

关闭自动更新时，保留所选渠道与受管路径，重新执行 `configure`：

```sh
"$VIBEFACE_INSTALL_ROOT/current/vibeface" update configure \
	--data-dir "$VIBEFACE_DATA_DIR" \
	--install-root "$VIBEFACE_INSTALL_ROOT" \
	--channel stable \
	--automatic false
```

这会移除更新定时服务，手动 `check` 和 `apply` 仍可使用。`vibeface service install-updater` 和 `uninstall-updater` 可单独安装或移除定时服务，不改变已保存的更新策略。策略的 `automatic` 为 `false` 时，即使手动安装定时服务也不会自动更新。重新配置自动更新会重新安装或移除定时服务。

更新器先核对包和存储兼容声明，再停止 Node 并切换 `current`。新进程健康检查失败时只恢复上一份程序入口，不自动还原数据库；会话、身份、配置和凭证仍保存在原数据目录。校验、网络和安装失败可通过 `update status` 与更新日志查看。

新进程通过健康检查并开放接单后才消费队列。存在新版本未处理的调度记录时，不支持任意手动复制旧程序降级；受管回退限于尚未开放接单或已经安全排空的状态。DAG 同步和恢复只补历史、重算视图，不从历史重新生成执行。

## 备份与恢复

在可信管理客户端导出加密恢复包，选择足够强且独立保存的密码。密码只保护恢复材料，不发送给 Node，不作为节点连接密码。

恢复包用于恢复个人根身份、必要的解密材料及建立新的管理授权。客户端恢复时生成新的设备密钥，不复制丢失设备的日常身份。恢复记录使用更高的授权 epoch，获知恢复记录的 Node 拒绝过期授权。

恢复根身份不会自动找回聊天正文、工程文件或 Codex 会话的隐藏状态。业务历史仍需可达的授权副本或独立数据备份。缺少材料时明确停止，不创建看似相同的新身份替代原网络。

从备份、快照或旧磁盘恢复的 Node 启动后先与其他成员 Node 对齐，每台其他 Node 都确认它没有漏掉以前写过的内容才开始写入；还没确认的 Node 都连不上时保持只读，由本人在网页上确认“不是从备份恢复的”后才写。同一数据目录的两份副本都写了内容时，两份都停止写入，分叉后的内容在移除这台设备前都不计入；移除时保留当前连接的电脑上的那份，再用新数据目录重新加入。处理方法见 [排障](troubleshooting.md#写入状态)。

在**我的设备**中移除丢失设备。收到签名撤销记录的端点拒绝后续操作，已建立会话也重新验权。离线分区收到记录前不能立即生效，撤销不能抹除对方已经读过的数据，也不强制停止已运行的外部进程。被移除设备上还没有结果的工作由会话的调度设备记为待确认（“设备已移除，无法核对”），由你确认做完没有，不会一直挂起，也不会当作没执行；还在等它接收的派发显示“结果未知”，Bot 会收到一次通知。

停掉 Node 后备份完整数据目录，保留 SQLite 数据库及未合并的 WAL。模型 key、本机私钥和 SDK session 留在本机，不进入共享 DAG。完整目录备份含敏感信息，按本机凭证保管。不要复制运行中的 SQLite 文件来实现节点同步。

存储使用 `rusqlite`，没有 ORM。设备授权、配对回执和业务记录共用单一 `node.sqlite3` 及同一个 Store 事务。Atlas HCL 为 `crates/node/schema.hcl`，唯一 initial SQL 和校验清单位于 `crates/node/migrations/`。数据格式为 epoch 7，数据库 `user_version` 为 7，直接初始化最终结构，不提供库迁移。不同 epoch 的程序和数据目录不能混用。程序不会自动清空旧目录。监控历史正文、附件和服务日志的磁盘占用。
