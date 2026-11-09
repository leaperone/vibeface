# 默认公共节点与连接引导

个人 Node 首次初始化后自动向官方公共节点登记，外网设备按设备编号经它找到这台 Node。官方公共节点是一台不加入任何个人网络的普通 Node，同时提供发现服务、开放 Relay 和浏览器转发，各有上限，后两者经 Cloudflare 代理在 `https://node.vibeface.dev`。手机 App 和浏览器打开 `https://app.vibeface.dev`，仍优先使用局域网直连。

## 使用方式

普通用户初始化并启动 Node 即可。CLI 初始化和 Core 首次初始化目录时写入 `"bootstrap": {"mode": "public"}`，提供发现服务的 Node 默认 `off`。底层 `NetworkConfig::default()` 与测试用本机配置保持离线，不因单元测试而访问公共服务。

`--bootstrap off` 关闭公共引导。自建服务用 `--bootstrap 'vibeface://<设备编号>@<主机>:<UDP 端口>'` 指定，自建配置不在失败时切到官方服务。

公共节点的设备编号和地址只在 `config/public-bootstrap.json` 定义一次（`issuer`、`udp_host`、`udp_port`、`gateway_url`），Core 用 `include_str!` 编入。网页和手机客户端在构建时导入同一文件，取 `gateway_url` 作为默认接入地址；用户可在设置里改成自己的 Gateway，存在本设备上。

## 发现与 Relay

Node 在 Endpoint 绑定后于后台连接发现服务，QUIC 握手证明对端就是钉住的设备。登记应答带回 Node 被观察到的地址；公共节点的 Relay 开放（`relay_open`）时，应答还附上这台 Relay。Node 把它加为候选 Relay，直连不通的设备之间也能经它连通；已配置的同一 Relay 和它的令牌保持不变。

Relay 默认只接纳自己、本网络的 Node 和持邀请令牌的加入者，配置运营者令牌后另外接纳持该令牌的设备。官方公共节点显式开放 Relay（`relay_open`），开放时必须限制每个连接的读取速率和同时连接数；官方还限制同一来源地址的连接数，并可在配置里拒绝指定设备。

## 浏览器接入

`app.vibeface.dev` 是静态网页，不转发任何接口。浏览器直接连公共节点的 `gateway_url`（`https://node.vibeface.dev`，经 Cloudflare 代理）：`/api/client`、`/api/info` 和 `/api/decision` 都在这个地址上。公共节点以它为 `public_url` 检查 Host，`allowed_origins` 包含 `https://app.vibeface.dev`，所以官方页面的跨源 WebSocket 和 `/api/info` 被接受。Cloudflare 按路径把 `/relay`、`/ping`、`/generate_204` 转到节点的 relay（所以 `relay_url` 也是这个地址），其余转到网页接口；源站证书是 Cloudflare Origin CA 证书。节点只信任 Cloudflare 网段，按它给出的 `CF-Connecting-IP` 计浏览器和 relay 限额，其余来源的同名头不起作用。UDP 发现服务不经过 Cloudflare。自建公共节点也可以不用代理，按 IP 直连并使用 Let's Encrypt 的短期 IP 证书，用户在设置里把接入地址改成它即可，见 [`deploy/README.md`](../../../deploy/README.md)。网页 CSP 的 `connect-src` 为 `'self' https: ws: wss:`，因为接入地址可由用户改成任意 Gateway，不能在静态头里列举主机。浏览器在 `connect` 帧里指定设备编号，公共节点经发现服务找到这台 Node，只搬运 Noise 密文。

转发名额用完时，新会话排队并收到 `{"type":"queued","position":n}`，页面显示“排队中，第 n 位”；队列也满时回 `gateway_busy`。同一浏览器地址的连接超出份额时，在升级前直接拒绝。每个会话的速率也受限。Node 默认不限速、不排队，只有官方公共节点在部署配置里设上限，见 [`deploy/README.md`](../../../deploy/README.md)。

## 信任边界

公共节点看不到对话内容，不保存个人网络的事件或附件。发现和转发不授予成员关系；加入仍需邀请、安全码核对和批准，连接目标的身份由端到端 Noise 验证。运营者可以看到设备编号、IP、查询关系和流量大小。

## 验证范围

- 本地公共节点和只配置其发现服务的全新成员 Node 完成登记，浏览器经公共节点加入、对话、在公共节点重启后重连，decision token 经公共节点送达，不使用生产凭证或模型。
- 转发名额用完时第二个浏览器排队，轮到后接通。
- 带运营者令牌的私有 Relay 接纳持正确令牌的 Node，拒绝持错误令牌的 Node。
- 只开 TLS 的 Node 用 `init --tls` 生成的证书应答钉住它的客户端，换一张证书时校验失败，明文 HTTP 得不到应答。
- 公共节点不保存事件、会话或日志；公共服务离线时，局域网功能保持可用。

本次保持 `DATA_EPOCH=1`，不修改聊天存储格式，不重放历史工作。
