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

包内包括 Rust 程序、SDK sidecar 源码、文档、用户级服务安装脚本和执行镜像构建脚本，不包含模型凭证、Provider 程序或 npm 依赖。Codex SDK 管理 Agent 在固定 Docker 镜像内运行，镜像包含 Node.js 18+ 和锁定的 npm 依赖，见 [执行镜像说明](execution-image.md)。

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

直接运行 `./vibeface` 即可启动本机节点。首次运行仅在不存在或为空的数据目录中初始化，默认目录是 `$HOME/.vibeface`，不授予执行目录或启用管理 Agent。根 `--data-dir` 可选择数据目录；有子命令时使用子命令自己的 `--data-dir`，两种位置不能混用。显式 `serve` 要求已初始化且采用当前数据格式。旧目录在任何数据库写入、恢复处理或 Provider 启动前拒绝，不自动转换或清空。

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

`init` 不覆盖现有身份和配置。默认数据目录为 `$HOME/.vibeface`，包含本机身份、配置、一次性接入材料、格式标记和唯一数据库 `node.sqlite3`。身份授权与业务状态共用这个数据库。`data-epoch.json` 为 `{"format":"vibeface-node","data_epoch":3,"layout_version":1}`。不同 Node 使用独立数据目录，同一目录同时只允许运行一个进程。

## 本机自动登录

节点启动后，在同一台机器运行：

```sh
./vibeface open --data-dir "$HOME/.vibeface"
```

命令使用本机私有凭据打开独立的回环入口，不打印访问凭据。首次仍须创建或解锁浏览器设备配置，完成正常的账户授权。成功连接本机节点后，该浏览器保存独立加密的自动解锁材料，之后打开本机入口即可连接。新浏览器仍需首次授权。

「允许本机自动登录」默认开启，可以在网页的连接页或设备设置页关闭。关闭后，本机会话失效、网页锁定，已启动的任务继续运行。原密码仓库保持可用。要重新开启，可再次运行 `vibeface open` 进入设置。

也可以停止节点后修改 `config.json` 再启动：

```json
"local_access": {
  "enabled": false,
  "listen": "127.0.0.1:4791"
}
```

`listen` 必须是与公开入口分开的回环地址。初次默认自动分配端口并写回配置，后续启动保留此端口；浏览器存储按地址隔离，修改端口后须重新设置该浏览器。公开入口不接收本机会话，反向代理从 `127.0.0.1` 转发也不会自动获得权限。

纯 Relay 上的本机访问只管理 Relay 自身。它不会获得其他节点的账户或执行权限；访问 Mac mini 仍走 Mac mini 的正常授权。

## 配置管理 Agent

聊天 Agent 理解指令，查询或调用受管命令，并按需委派编码执行器。编码执行器在明确授权的目标设备和项目目录中工作。调度 SDK 会话与共享业务聊天分开保存。

`manager` 只选择 Provider 和模型，不接受宿主 SDK 程序路径。已有 Node 先停止进程，再修改配置并重启，保留原身份和数据库：

```json
{
  "manager": {
    "provider": "codex_sdk",
    "model": "your-model-id"
  }
}
```

模型名称使用账户实际可用的值。省略模型时采用 Provider 默认值，默认值不承诺响应速度。

管理 Agent 使用 `execution_isolation` 配置的固定 Docker 镜像和受信模型网关。按 [执行镜像说明](execution-image.md) 构建依赖并核对实际 image ID。宿主 Provider 密钥和 Node 工具回调凭据不进入管理进程，管理进程只挂载本次授权资源的项目目录。当前 Cursor SDK 管理配置会被拒绝，不会回退为宿主进程。

Codex sidecar 通过私有 Unix socket 向 Node 请求动态目录中列出的工具。所有写工具要求稳定 `call_ref`；丢回执时先查询同一引用，不能换 ID 重做。工具目录按本轮权限过滤，只读结果汇报不会获得写工具。Node 继续负责授权、执行账本、审批、撤销和恢复，SDK 不直接获得这些权限。

Codex 编码执行器使用 App Server 协议。Hook 可辅助发现会话和上报事件，不代替签名授权、实际状态查询或恢复。管理 SDK 能力与编码执行器能力分别返回，以当前 Node 的真实能力为准。

## 添加自己的设备

每个 Node Service 和客户端设备都有独立密钥。设备名称、IP 和中继返回的地址不证明所有权。

另一台 Node 先在独立空数据目录初始化。使用已有账户的可信客户端核对新 Node 的连接卡、公钥和一次性认领信息，完成账户 Node Service 登记。协议使用 `account_node_possession`、`account_node_commit` 和 `account_node_install`，客户端保留并核验签名回执。中断后对原申请重试，不新建账户代替登记。详细字段见 [协议参考](node-protocol.md#http-与客户端通道)。

Service 登记只确认该 Node 属于账户，不自动授予 Project 历史、同步或执行权限。客户端必须分别提交资源的签名授权，执行 Node 还需满足本机目录和隔离配置。Agent 不能添加设备、扩大权限或撤销设备。

连接卡可保存直连地址或公共中继地址。更换入口后仍核验原目标 Node 和资源授权，再读取业务历史。跨 origin 的浏览器存储互相隔离，使用固定可信客户端来源可保留自己的设备密钥。

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

## 创建机器人并处理结果

连接自己的设备后，创建一个有名称的机器人，按需填写职责和项目。进入机器人即可直接聊天，每个机器人保留自己的长期历史。创建机器人只保存定义，不启动模型；首次发消息时，程序选择已授权且可用的聊天 Agent。

机器人定义、聊天历史和本机 SDK 会话分开保存。机器人项目配置只是执行偏好，不授予目录或设备权限；执行时仍检查 Node 真实授权。多个机器人可以使用同一 Node 的 SDK 配置，但不共用聊天历史或本机 SDK 缓存。设备、连接和恢复在设置中。

普通查询可以直接回答，本机命令由受管工具执行，较长或远端工作才按需委派编码 Agent。日常操作不需要创建工单、打开执行会话或选择调度器。进展、本人审批、补充输入和结果都回到原机器人的聊天。

发送后先看状态。`pending_delivery` 表示入口已保存消息，正在投递给原调度设备；只有目标持久入队后才是 `queued`。排队不表示模型已开始，`running` 才表示已跨过 SDK 启动边界。目标离线、能力不满足或回执丢失时，保留原目标与消息身份核对，不自动改派另一台设备。

管理 Agent 忙时，追加消息进入持久队列。尚未开始的本人消息优先于自主后续，同一调度 Node 上的同一会话一次只运行一个管理轮。已派发的后台任务继续运行，普通聊天不必等待任务结束。关闭网页不会停止 Node 上的工作，但执行设备关机、休眠或断网会影响推进。

需要停止时，区分以下动作：

- **停止当前回复**只请求取消当前管理轮，并撤销该轮的 Node 工具权限。取消待确认时不能当作已经停止，也不停止已派发的后台任务。
- **停止后台工作**作用于选中的实际执行。Node 从原签名派发找到执行设备和当前 turn，按执行器真实能力中断。不支持中断的执行不会显示可用的停止按钮。
- 明确长期目标的自主继续权限仍由本人决定。撤销权限阻止新的自主派发，不证明当前外部进程已停止。

Cursor SDK 和 Codex SDK 都提供进度和取消请求，当前都没有管理轮的原生 steer。立即调整须先核对取消结果，再提交新一轮。Cursor 需要匹配的 Provider 终态或受管进程退出证据才能确认取消。Codex CLI 退出仍不能证明已分离的工具进程全部退出，当前真实适配器将这种取消保留为 `unknown`。

Codex 执行器支持打开、读取、发送、调整当前 turn、中断和回答原请求。Claude Code 与 Cursor CLI 只支持打开和发送，其他操作明确拒绝。CLI 遇到需要交互审批的工作不会自动放行；需要完整审批往返时使用 Codex。

Node 默认尝试启动与本机身份关联的私有 `codex app-server`，socket 位于私有的 `/tmp/vibeface-<identity>/` 目录，以避免 macOS Unix socket 路径长度限制。要连接已有实例，在 `config.json` 设置 `codex_socket`。`codex_bin`、`claude_bin`、`cursor_bin` 指向对应程序。启动网页成功不等于程序已安装、已登录或模型可用，应查看聊天中后台工作的实际状态。

启动后，Node 恢复已知 Codex 线程的原生事件订阅，断线后重新订阅。完成、失败和待审批状态无需浏览器在线，也不要求修改用户全局 Codex hooks。恢复只读核对原线程，不重发旧指令，不自动批准审批。未由此 Node 接管的外部 CLI 会话不在该订阅范围内。

普通执行的终态结果自动送回原聊天，模型总结使用只读权限，不能借结果回调创建或派发新工作。明确长期目标的自主继续仍须本人授权，模型不能代授予。并发修改授权会要求刷新版本，已知撤销后旧权限不能继续写入；离线设备尚未收到撤销时不能宣称全网已经生效。

普通进度和通知不调用管理模型。`daily_decision_limit` 只限制当前 Node、当前个人空间在 UTC 当天的自主管理调用，本人主动聊天不消耗这个额度。失败的自主调用也可能消耗已预留次数。各 Node 分别记账，不构成全网 token 或费用硬上限。没有模型汇报授权或额度耗尽时，原始结果和通知仍保留。

编码执行器结束一轮不等于交付目标已经达成。已有明确长期目标的历史保留目标和验收记录，本人核对产物后确认。审批必须由本人回答，并对应仍有效的原执行请求。

已确认完成或取消的长期目标不能由 Agent 自行恢复。需要继续时，在机器人聊天中明确提出新要求。取消目标不会自动停止已经运行的进程，仍需核对后台工作的实际状态。

未读提醒归属原机器人聊天。普通通知不消耗模型调用；浏览器提醒取决于浏览器支持与设置中的通知权限。关闭网页后没有独立的手机推送服务。

从另一台入口打开原会话，可只读观察原管理节点和执行节点的临时进度。重连后读取持久状态，缺失的临时文字不会触发重做。缓存状态会标明新鲜度，离线时不能据此确认远端仍在运行。长文本结果可在聊天中展开或下载；尚未同步的附件需等待原副本可达，只有哈希不能补回正文。

## 核对未知结果

网络断开或进程中断后，写操作可能已经执行，但回执未保存。Node 将这种情况保留为 `unknown`，不自动重发，也不释放该操作占用的执行资源。

管理 SDK 已返回明确结果时，Node 先将最终结果保存到本机。历史依赖暂未补齐或最终事件暂时写入失败时，只补发最终记录，不重新调用 SDK。

管理轮重启后也可能变为 `unknown`。原轮保留未知，不重跑原触发。Node 隔离旧工具权限并核对在途请求后，新的本人消息可进入独立的只读会话查询状态。恢复写入须由本人核对原操作并附证据确认；管理未知确认不会把执行未知改成成功，旧只读权限也不会自动升级。

在原机器人的聊天中使用“核对执行结果”查询原执行 Node。若仍无法判断，检查实际进程、项目产物和外部副作用，再附证据确认成功或失败。人工确认只解决原账本记录，不重放原操作。失联不能证明远端进程已停止。

A 已派发 B 后掉线时，从 C 打开原业务会话，等待 DAG 补齐，再核对 B 的实际执行状态。缺失历史、执行器不可达和结果未知分别显示，不能通过创建同名新任务代替恢复。

同一 Node 检查会话版本，并对规范化工作目录及父子目录冲突进行本机互斥。不同 Node 之间没有全局文件锁。若两台设备挂载同一可写目录，使用独立 worktree 或独立根目录隔离。

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

`status` 通过私有维护 socket 检查运行中的 Node，并核对 NodeID，不要求安装服务。`status --json` 输出状态、NodeID、监听地址、角色及运行版本，不输出接入授权材料。已停止或无法连接时返回非零退出码。`service status` 则展示 launchd 或 systemd 的 Node 和更新服务状态。

```sh
vibeface status --json --data-dir "$HOME/.vibeface"
vibeface service stop --data-dir "$HOME/.vibeface"
vibeface service start --data-dir "$HOME/.vibeface"
vibeface service restart --data-dir "$HOME/.vibeface"
```

macOS 使用当前登录用户的 launchd agent，日志位于数据目录的 `service.stdout.log` 和 `service.stderr.log`。Linux 使用 `systemd --user`，Node 日志由用户 journal 保存。退出登录后仍需运行的 Linux 主机，由管理员按主机策略启用该用户的 lingering。

后台服务不保证继承终端的 PATH 或临时环境变量。执行镜像、宿主执行器和受信模型网关须使用服务能够访问的明确配置。安装脚本不保存或复制密钥。

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

存储使用 `rusqlite`，没有 ORM。Atlas 声明位于 `crates/node/schema.hcl`，唯一初始 SQL 为 `crates/node/migrations/202610180001_initial.sql`，同目录的 `atlas.sum` 校验文件完整性。`bash scripts/verify-schema.sh` 在私有空 SQLite 库中应用初始 SQL，再比较 Atlas 声明。运行程序使用内嵌的同一份 SQL，不需要安装 Atlas。Atlas 不负责节点同步。数据格式 epoch 为 `3`，layout version 为 `1`，与授权或恢复 epoch 分开。启动和更新不转换或清空旧目录，旧目录须保留给对应版本。监控历史正文、附件和服务日志的磁盘占用。
