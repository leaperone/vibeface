# 部署与使用 Node

本文使用 `vibeface` 管理自己的设备。HTTP 与同步协议见 [协议参考](node-protocol.md)，出问题时见 [排障](troubleshooting.md)。

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

客户端独立生成个人根身份和自己的设备身份，签署首个成员目录。Node 验证一次性本机授权后安装该目录，日常连接使用客户端设备密钥。已有网络的节点提供当前入口；新浏览器可使用本机自动访问，永久成员授权仍需原有成员邀请或加密恢复包。本机恢复自动填入目标及所需初始化材料。要加入已有个人网络的 Node 不在网页上建网，用 `vibeface join` 加入，见 [添加自己的设备](#添加自己的设备)。

`./vibeface access --data-dir "$HOME/.vibeface"` 保留为备用入口。不要公开它输出的初始化链接。后续连接自动核验设备编号和成员授权，不输入账号密码。个人根私钥留在可信客户端及加密恢复包，不作为节点间的长期共享密码。

默认只监听本机。`http://127.0.0.1:4790` 和 `http://localhost:4790` 可用于本机初始化。`http://0.0.0.0:4790` 可打开服务，但不能自动取得初始化授权。保存密钥的网页客户端必须处于浏览器认可的安全上下文，推荐固定使用 `http://localhost:4790` 或可信 HTTPS 页面。

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含本机身份、配置、私有授权记录和 `node.sqlite3`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

### 本机自动访问

个人网络建立后，`local_auto_access` 默认开启。在本机打开当前节点的 localhost 网页，先尝试原有签名身份。需要本机会话时，点击**打开本机访问**，网页重新取得短期入口并导航到独立回环页面，无需配对新的永久成员。一次性 token 放在 URL fragment 中，页面清除后兑换绑定浏览器 Noise 公钥的 HttpOnly 会话。会话保存在数据目录的 `local-sessions.json`（只存 cookie 摘要），Node 重启后刷新页面即可继续，无需重新兑换；会话失效或本机访问被关闭后，重新打开本机网页并点击该入口。`vibeface access` 保留为备用方式。

本机会话可使用本机业务、配置本机 Agent 和项目目录，并通过正常授权路由发送任务；不能签署成员授权、登录其他 Node 或替代网络管理客户端。已有签名管理身份继续保留管理权限。

可在网页的设备设置关闭本机自动访问，也可把 `config.json` 的 `local_auto_access` 改为 `false` 后重启。初始化时加 `--no-local-auto-access` 可直接关闭。关闭会撤销已有本机会话和待兑换 token；普通配对身份仍可使用。重新开启由管理客户端或本机配置完成，不恢复旧凭据。

本机会话兑换使用固定回环端口 `local_listen`（默认 `127.0.0.1:4791`），重启后本机 origin 和浏览器存储不变；端口被占用时 `init` 先报错并提示用 `--local-listen` 更换，不写数据目录。固定 localhost 网页只有通过实际 loopback 来源、字面 Host、精确 Origin 且不含转发头的设置请求，才能取得短期启动材料。一般公开请求仅伪造 `Host: localhost` 或代理头不能取得本机访问权。自动访问关闭时仍使用签名成员邀请或恢复包。反向代理应只转发公开 listener。

## 配置管理 Agent

协调 Agent 理解用户指令，用宿主 SDK 的 Shell 和文件工具读取本机，通过 Node 的 `dispatch_work` 工具派发工作来修改代码：在本机或另一台设备的 Codex App Server 上开一个 turn，或把任务交给另一台设备的协调 Agent，由它在那台设备上执行并回传结果。SDK 会话缓存与共享聊天分开保存。`manager.workspace` 是 SDK 的本机工作目录，项目选择来自 `roots`。

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

Cursor 使用 SDK 原生自定义工具回调连接 Rust Node。产品链路不启动 MCP Server，也不要求 Bot 或浏览器支持 MCP。协调工具限定到当前个人网络及机器人聊天，本机工作使用 SDK 宿主工具。Agent 不能替本人授权自动后续。

Node 只常驻一个 Cursor SDK Bridge 进程供各轮回复复用。已设置 `CURSOR_API_KEY` 时，Node 启动或保存 Cursor 设置后立即预热。Bridge 退出或无响应时，下一轮按 1 至 30 秒退避重启；某一轮结果未知时，Bridge 等其余轮次结束后退出，下一轮换新进程。启动超过 60 秒时这一轮不会开始，稍后再发即可。

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

在管理设备选择**添加设备**并点**生成链接**，得到一条 `https://<gateway>/join#vibeface1…` 加入链接。入口有公网 HTTPS 地址时同时显示二维码；只有本机或局域网地址时不出二维码，提示“只能本机打开”，按下文“配置公共入口”设置。链接含一次性 secret，只交给自己的设备。新浏览器打开链接，确认名称后显示 6 位安全码和“等待批准”。管理设备上出现“X 想加入”和同一个安全码：一致就点**一致，允许**，浏览器可同时勾选**设为管理员**；不一致或不认识的请求点**拒绝**，该请求随即作废。只有发出链接的面板会自动开始比对，其他管理页面需先点**显示安全码**。新设备伪造或替换随机数时请求也会作废。

没有任何可公网访问的 HTTPS 入口时，邀请界面不显示二维码，提示“只能本机打开”，见 [配置公共入口](#配置公共入口)。`localhost` 与 `127.0.0.1` 仅指向各自本机。

添加另一台 Node 时，在新机器上先用 `vibeface init` 初始化（不要在它的本机网页上建网），再执行：

```sh
./vibeface join --data-dir "$HOME/.vibeface" "https://gateway.example/join#vibeface1…"
```

命令显示 6 位安全码并等待批准；在管理设备上比对安全码一致后点**一致，允许**，命令完成加入，再启动新 Node。`--name` 可改设备名，默认取主机名。连接中断时用同一链接和目录重试，结果未知时先核对原请求状态，不重新创建请求。

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

## 配置公共入口

手机或另一台电脑要从外网打开网页，需要一个 HTTPS 入口。有两种做法：

- **公共入口**：在有公网地址的机器上运行一个不加入个人网络的 `vibeface`，它只提供网页、relay 和密文转发，不保存你的任何数据。家里的设备不需要入站端口。推荐用这种方式。
- **成员 Gateway**：让公网机器上的 Node 作为普通成员加入个人网络，同时提供网页，见本节末尾。

### 公共入口：PVE 加 nginx

下面以 PVE 上的一台 Linux 虚拟机为公共入口，域名 `vb.example.com`，家里的 Mac 是个人网络里的 Node。

1. 域名解析到 PVE 的公网地址，用 certbot 或其他方式为它签发证书。

2. 在 PVE 上初始化公共入口并启动。网页监听 `127.0.0.1:4790`，relay 监听 `127.0.0.1:3340`，两者都只对 nginx 开放：

   ```sh
   vibeface init --data-dir "$HOME/.vibeface-entry" \
   	--public-entry \
   	--public-url https://vb.example.com \
   	--relay-listen 127.0.0.1:3340
   vibeface serve --data-dir "$HOME/.vibeface-entry"
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

4. 把 PVE 上的 `entry-token` 复制到 Mac，例如 `scp pve:.vibeface-entry/entry-token ~/vb-entry-token && chmod 600 ~/vb-entry-token`。

5. 在 Mac 上初始化 Node 时指向公共入口，然后启动：

   ```sh
   vibeface init --data-dir "$HOME/.vibeface" \
   	--root /absolute/path/to/projects \
   	--entry https://vb.example.com \
   	--relay-token-file "$HOME/vb-entry-token"
   vibeface
   ```

   `init` 把令牌复制进 Mac 的数据目录，之后可以删掉 `~/vb-entry-token`。relay 不在入口同一个 origin 上时，用 `--entry-relay URL` 另外指定。

6. 在 Mac 上打开 `http://localhost:4790` 建网。之后**添加设备**生成的链接和二维码都指向 `https://vb.example.com/join#…`。手机扫码打开后确认名字，在 Mac 上核对 6 位安全码并点**一致，允许**。也可以在手机上打开 `https://vb.example.com`，点**扫码**扫描**添加设备**里的二维码。

公共入口只搬运浏览器与 Mac 之间的 Noise 密文，看不到对话内容，也不保存事件或附件。它的 relay 只接纳持有令牌的设备，持错误令牌的 Node 连不上。一台公共入口可以同时服务多台带令牌的 Node，各人的个人网络互不相通。令牌泄露时，停掉入口和使用它的 Node，用同一份新令牌（`openssl rand -hex 32` 生成的 64 个十六进制字符）替换它们数据目录里的 `entry-token`，再依次启动。

已经初始化的 Node 要改用公共入口，停掉它，在 `config.json` 的 `network` 中加入 `"entry": {"url": "https://vb.example.com", "relay": "https://vb.example.com"}`，把令牌放到数据目录的 `entry-token`（权限 600），再启动。

### 成员 Gateway

公网机器上的 Node 也可以作为普通成员提供网页：

1. 准备域名并解析到这台机器，例如 `vibe.example.com`。
2. 用 Caddy 或 nginx 签发证书，把 `https://vibe.example.com` 整站反代到本机 `127.0.0.1:4790`，并允许 WebSocket 升级。Caddy 只需一行：`vibe.example.com { reverse_proxy 127.0.0.1:4790 }`。
3. 在这台 Node 的 `config.json` 设置 `"public_url": "https://vibe.example.com"`，重启。
4. 用 `vibeface join` 让这台 Node 加入个人网络，之后管理设备生成邀请时会使用它的公网地址并显示二维码。

成员 Gateway 保存个人网络的完整副本。反代只转发公开 listener，不要转发本机会话的 `local_listen`。浏览器只经 `/api/client` 跨源连接，其他路径仍要求同源。网页中的**怎么设置**链接指向本节。

## Node 之间的连接

Node 之间经 [iroh](https://www.iroh.computer/) QUIC 互连，设备编号就是 iroh 的 `EndpointId`，连接由 QUIC TLS 证明对端身份，再按成员目录授权。非成员的连接被拒绝。没有局域网发现，也不使用 n0 的 DNS 和 pkarr 地址发布：成员 Node 的地址只来自个人网络内部交换的签名地址记录。

每台 Node 定期为自己签一条地址记录，内容是 iroh 地址（relay URL 和直连 IP）以及网页入口 `gateway_url`。同步时双方交换记录，地址变化后几秒内传到其他成员。浏览器从当前入口读取这些记录，自己验签后作为可切换的访问地址，见下文“多个访问地址”。

两台 Node 不能直连时经 relay 转发。relay 只转发 QUIC 密文，只接纳当前成员 Node 和持有本 Node 所发邀请的加入者，默认不使用 n0 公共 relay。网络设置只在 `init` 或首次直接启动时写入 `config.json`，修改后重启生效：

| 参数 | `config.json` 中的 `network` 字段 | 作用 |
|---|---|---|
| `--relay <URL>` | `relays` | 个人网络中另一台 Node 的 relay，可重复，最多 16 个 |
| `--relay-listen <ADDR>` | `relay_listen` | 本 Node 在这个地址运行 relay |
| `--relay-url <URL>` | `relay_url` | 其他 Node 访问本 relay 的 URL，默认 `http://<relay-listen>`；监听全部网卡时必填 |
| `--relay-paths-only` | `relay_paths_only` | 只经 relay 连接，用于模拟严格 NAT 或排障 |
| 无 | `bind` | iroh 的 UDP 监听地址，默认在 `listen` 的端口上监听全部 IPv4 和 IPv6 网卡 |
| 无 | `n0_relays` | 同时使用 n0 公共 relay，默认关闭 |

公网 Node 承担 relay 角色。例如 PVE 用反向代理把 `https://relay.example` 转到本机 `127.0.0.1:3340`，并允许 WebSocket 升级：

```sh
vibeface init --data-dir "$HOME/.vibeface" \
	--relay-listen 127.0.0.1:3340 --relay-url https://relay.example \
	--public-url https://vibe.example
vibeface join '<管理设备给出的加入链接>' --data-dir "$HOME/.vibeface"
```

家里的 Mac mini 用 `--relay https://relay.example` 初始化，无需入站端口。邀请链接写明发出邀请的 Node 自己的 relay。relay 只认本 Node 登记的邀请，所以只能经 relay 连通的新 Node 要用运行 relay 的那台 Node 发出的邀请加入。

公网 Node 也是普通成员，可同时运行调度或执行。relay 不读取会话内容，也不给其他设备增加任何权限。不想让公网机器保存个人网络数据时，改用 [公共入口](#配置公共入口)。

历史按每台设备一条签名日志同步：双方交换版本向量，只拉对方缺的区间，入库前逐条验签并去重，中断后从已入库的位置续拉。对端重启后，它发来的同步请求带新的启动编号，本机随即关闭旧连接并重新同步，不等 QUIC 空闲超时。同步只写入历史和投影，不会重新执行历史中的操作；调度和执行只发生在被指定的 Node 上，见[事件 DAG 协议](node-protocol.md#事件-dag)。

网页按 1 MiB 分块把附件上传到当前入口 Node，入口重启后从已落盘的位置继续，消息只引用附件的 hash 和大小。附件下载的已收部分保存在本机私有暂存文件。断线或重启后，Node 重新核验授权并从已保存的位置继续；只有最终大小和 hash 校验通过的完整附件才能使用。暂存最多占用 256 MiB、64 个对象。连接恢复只继续传输，不重发业务操作。

## 多个访问地址

每台启用 Gateway 的 Node 都是同一个人网络的一个访问地址。浏览器连上任意一台后，会经它连到目标 Node：Noise 握手仍在浏览器和目标之间完成，中间的 Gateway 只转发密文；目标不是本网络成员时，Gateway 回“目标不在我的设备里”。

左下角的连接状态显示在线、切换中或离线，点开可看到当前访问地址和其他可切换的地址，每个地址标明“公网”或“仅本机或局域网”。只有带公网 HTTPS `gateway_url` 的地址算公网，邀请的二维码只用公网地址。

当前访问地址断开时，页面先重试它，再按最近成功的顺序试其他地址，不需要重新加入或刷新。还没确认送达的消息保存在浏览器 IndexedDB 中，切换后用原来的签名原样重发，Node 按消息编号去重，调度 Node 只执行一次。同一浏览器的多个标签页由其中一个负责重发和探测入口，其他标签页跟随它切换。页面所在 origin 的 Gateway 不可达时，刷新后应用外壳从 service worker 缓存打开，再经其他访问地址恢复会话。

成员 Gateway 的 origin 自动被其他成员的 `/api/client` 接受，无需逐个写进 `allowed_origins`。撤销一台 Node 后，其他 Gateway 不再转发到它，浏览器也不再把它列为访问地址。

## 创建机器人并处理结果

连接自己的设备后，创建一个有名称的机器人，按需填写职责和项目。进入机器人即可直接聊天，每个机器人保留自己的长期历史。创建机器人只保存定义，不启动模型；首次发消息时，程序选择已授权且可用的聊天 Agent。

机器人定义、聊天历史和本机 SDK 会话分开保存。机器人项目配置只是执行偏好，不授予目录或设备权限；执行时仍检查 Node 真实授权。多个机器人可以使用同一 Node 的 SDK 配置，但不共用聊天历史或本机 SDK 缓存。设备、连接和恢复在设置中。

普通查询可以直接回答，本机工作使用 SDK 的宿主工具，远端工作通过 Agent 任务消息委派。日常操作不需要创建任务单、打开执行会话或选择调度器。进展、补充输入和结果都回到原机器人的聊天。

会话的调度设备由本人在建会话时签名指定，每次更换都写一条带本人签名、epoch 加一的负责人记录（`set_owner`）。调度设备离线而当前连接的设备能运行管理 Agent 时，聊天里出现“X 离线了，这个对话在等它”，点**交给这台设备处理**即可换过来，排队的消息在新设备上继续回复。新调度设备只按共享记录和上下文检查点重建上下文，不重放已经执行的工作。

发送后先看状态。`pending_delivery` 表示入口已保存消息，正在投递给原调度设备；只有目标持久入队后才是 `queued`。排队不表示模型已开始，`running` 才表示已跨过 SDK 启动边界。目标离线、能力不满足或回执丢失时，保留原目标与消息身份核对，不自动改派另一台设备。

管理 Agent 忙时，追加消息进入持久队列。尚未开始的本人消息优先于工作结果等后续轮次，同一调度 Node 上的同一会话一次只运行一个管理轮。管理轮派发工作后即结束，不等待工作；工作结束时它的结果作为新的一轮入队，Agent 汇报并可继续派发。关闭网页不会停止 Node 上的工作，但执行设备关机、休眠或断网会影响推进。

需要停止时，区分以下动作：

- **停止当前回复**只请求取消当前管理轮，并撤销该轮的 Node 工具权限。已输出的部分回复保留为一条标记“已停止”的回答。取消待确认时不能当作已经停止，也不停止已派发的工作。
- **停止工作**作用于选中的工作，中断它的原生 turn，并一并停止它委派出的工作。任何入口都能停止其他设备上的工作。目标离线时页面显示“已请求、等待设备确认”，入口继续投递 30 秒，仍未送达则显示“设备没有确认停止”。请求发出后仍需核对实际终态。

Cursor SDK 和 Codex SDK 都提供进度和取消请求，当前都没有管理轮的原生 steer。立即调整须先核对取消结果，再提交新一轮。Cursor 需要匹配的 Provider 终态或受管进程退出证据才能确认取消。Codex 在停止时记录 CLI 进程树中的全部子进程及其进程组，逐组终止并确认这些进程都已退出后才回报已停止；无法确认时该轮进入 `interrupted` 并列入待核对。停止回复不影响已派发的工作，停止工作是单独操作。

Agent 通过 Node 的 `dispatch_work` 工具派发工作，例如 `vibeface tool dispatch_work --json '{"title":"跑测试","prompt":"...","cwd":"...","call_ref":"REF","target_node":"<设备编号>"}'`。`title` 是给本人看的几个字，任务卡片以它为名，`prompt` 放在详细信息里。省略 `target_node` 时派发到本机：CLI 先向 Node 登记，再直接连本机 App Server 开线程、以完全访问（`approvalPolicy=never`、`danger-full-access`）发一个 turn，Node 记录并观察它。`target_node` 是另一台设备时，任务交给那台设备的协调 Agent，作为它的一轮执行，由它在本机派发，不能再向其他设备派发。`continues` 填原工作编号时，在原工作的 thread 上继续。目标 Node 核对派发方仍是成员且处于进行中的调度轮次，并要求 `cwd` 位于目标自己 `config.json` 的 `roots` 内，不满足时拒绝，页面显示“未接收”。同一轮用同一 `call_ref` 重试得到同一工作，不会启动第二个 turn；结果不明时用 `call_lookup` 查询。

协调 Agent 的 Shell 完全访问本机，可以改文件、跑 git 和 gh。它用 `vibeface codex call <method> [json]` 调用 App Server 的任意方法，`vibeface codex send <thread> <text> --id <id>` 给会话插话或开新回合（每个 `--id` 发送前先记入 Node 数据目录的 `codex-sends`，由 `VIBEFACE_DATA_DIR` 给出；同一 `--id` 再次执行只回读、绝不重发，换了内容或会话则拒绝；没有数据目录时须用 `--log DIR` 指定日志目录），`vibeface codex watch <thread>` 查看事件。Socket 路径在 `VIBEFACE_CODEX_SOCKET`。

Codex 默认 bypass，正常不会发起请求。仍发起的请求（审批、提问、登录等）由 Node 挂起并立即唤醒协调 Agent：它用 `vibeface codex requests` 查看，`vibeface codex respond <request_id> '<result-json>'` 回答，同一请求只回答一次。需要本人拍板时，它用 `vibeface triage raise --event-id <ID> --request-id <请求> --choice '批准={"decision":"accept"}' --choice '拒绝={"decision":"decline"}' ...` 生成拍板卡片，或经 webhook 转发 Grok Bot；卡片挂住请求直到本人选择，忽略卡片等于拒绝。没有协调 Agent，或它 120 秒内既没回答也没生成卡片时，Node 按方法拒绝并记录。没有协调 Agent 时分诊事件直接丢弃。

Node 托管本机的 `codex app-server`：未设置 `codex_socket` 时，Node 用 `codex_bin` 启动它，Socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix Socket 路径长度限制。App Server 运行在独立进程组，进程组号写入数据目录的 `codex-app-server.pid`。Node 启动时先结束上次遗留的进程组，正常退出时一并停止它，异常退出则退避重启。要连接自己管理的实例，在 `config.json` 设置 `codex_socket`。启动网页成功不等于程序已安装、已登录或模型可用，应查看聊天中工作的实际状态。

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
vibeface service install --data-dir "$HOME/.vibeface"
vibeface status --data-dir "$HOME/.vibeface"
vibeface service status --data-dir "$HOME/.vibeface"
```

服务管理命令需要完整的受管安装，启动路径始终为安装根目录下的 `current/vibeface`。通过 `~/.local/bin/vibeface` 链接调用也保留该路径，升级后不会固定在旧 release。直接运行 `cargo build` 的程序不安装服务。自定义安装根先使用 `update configure --install-root /absolute/path` 保存路径。

每个系统用户管理一个 Node 服务。各服务命令核对 `--data-dir` 与已安装服务一致，再操作该服务。省略时使用 `$HOME/.vibeface`。管理其他数据目录时显式传入绝对路径。

`status` 通过私有维护 socket 检查运行中的 Node，并核对设备编号，不要求安装服务。它输出设备编号、监听地址、运行版本、写入状态（`Writes`），以及每台其他成员 Node 最近一次同步是否连上、多久之前完成（`Peer`）。`status --json` 输出同样内容，不输出接入授权材料。已停止或无法连接时返回非零退出码。`service status` 则展示 launchd 或 systemd 的 Node 和更新服务状态。各项含义见 [排障](troubleshooting.md)。

```sh
vibeface status --json --data-dir "$HOME/.vibeface"
vibeface service stop --data-dir "$HOME/.vibeface"
vibeface service start --data-dir "$HOME/.vibeface"
vibeface service restart --data-dir "$HOME/.vibeface"
```

macOS 使用当前登录用户的 launchd agent，Linux 使用 `systemd --user`。Node 无论前台还是后台都把 info 级日志写到数据目录的 `node.log`；后台运行时 macOS 的 `service.stderr.log` 和 Linux 的用户 journal 只剩崩溃输出。退出登录后仍需运行的 Linux 主机，由管理员按主机策略启用该用户的 lingering。

后台服务不保证继承终端的 PATH 或临时环境变量。在 `config.json` 使用 Provider 和 sidecar 的绝对路径，并通过对应服务管理器配置模型凭证。安装脚本不保存或复制密钥。

卸载同时移除 Node 服务和更新定时服务，不删除身份、配置和历史：

```sh
vibeface service uninstall --data-dir "$HOME/.vibeface"
```

## 选择更新渠道

自动更新需要完整发布包与受管安装目录。固定的 `current` 符号链接指向当前包，Node 用户服务也通过该路径启动。首次安装方式见 [构建与发布](releasing.md#首次安装)。

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

恢复根身份不会自动找回聊天正文、工程文件或 Codex 会话的隐藏状态。业务历史仍需可达的授权副本或独立数据备份。缺少材料时明确停止，不创建看似相同的新身份替代原网络。

从备份、快照或旧磁盘恢复的 Node 启动后先与其他成员 Node 对齐，确认自己没有漏掉以前写过的内容才开始写入；其他 Node 都连不上时保持只读，由本人在网页上确认“不是从备份恢复的”后才写。同一数据目录的两份副本都写了内容时，后发现的那份停止写入，需要移除后重新加入。处理方法见 [排障](troubleshooting.md#写入状态)。

在**我的设备**中移除丢失设备。收到签名撤销记录的端点拒绝后续操作，已建立会话也重新验权。离线分区收到记录前不能立即生效，撤销不能抹除对方已经读过的数据，也不强制停止已运行的外部进程。被移除设备上还没有结果的工作由会话的调度设备记为“已放弃、未执行”，不会一直挂起。

停掉 Node 后备份完整数据目录，保留 SQLite 数据库及未合并的 WAL。模型 key、本机私钥和 SDK session 留在本机，不进入共享 DAG。完整目录备份含敏感信息，按本机凭证保管。不要复制运行中的 SQLite 文件来实现节点同步。

存储使用 `rusqlite`，没有 ORM。设备授权、配对回执和业务记录共用单一 `node.sqlite3` 及同一个 Store 事务。Atlas HCL 为 `crates/node/schema.hcl`，唯一 initial SQL 和校验清单位于 `crates/node/migrations/`。数据格式为 epoch 7，数据库 `user_version` 为 7，直接初始化最终结构，不提供旧库迁移。部署此版本使用全新数据目录和同版本节点，不能把新程序直接接到旧库。程序不会自动清空旧目录。监控历史正文、附件和服务日志的磁盘占用。
