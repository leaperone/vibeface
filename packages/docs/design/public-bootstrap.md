# 默认公共节点与连接引导

个人 Node 首次初始化后自动向官方公共节点登记，外网设备按设备编号经它找到这台 Node。官方公共节点是一台不加入任何个人网络的普通 Node，没有域名，同时提供发现服务、开放 Relay 和浏览器转发，各有上限。手机 App 和浏览器打开 `https://app.vibeface.dev`，仍优先使用局域网直连。

## 使用方式

普通用户初始化并启动 Node 即可。CLI 初始化和 Core 首次初始化目录时写入 `"bootstrap": {"mode": "public"}`，提供发现服务的 Node 默认 `off`。底层 `NetworkConfig::default()` 与测试用本机配置保持离线，不因单元测试而访问公共服务。

`--bootstrap off` 关闭公共引导。自建服务用 `--bootstrap 'vibeface://<设备编号>@<主机>:<UDP 端口>'` 指定，自建配置不在失败时切到官方服务。

公共节点的设备编号和地址只在 `config/public-bootstrap.json` 定义一次（`issuer`、`udp_host`、`udp_port`），Core 用 `include_str!` 编入。网页和手机客户端不读取它，只认官方页面 `https://app.vibeface.dev`。

## 发现与 Relay

Node 在 Endpoint 绑定后于后台连接发现服务，QUIC 握手证明对端就是钉住的设备。登记应答带回 Node 被观察到的地址；公共节点的 Relay 开放（`relay_open`）时，应答还附上这台 Relay。Node 把它加为候选 Relay，直连不通的设备之间也能经它连通；已配置的同一 Relay 和它的令牌保持不变。

Relay 默认只接纳自己、本网络的 Node 和持邀请令牌的加入者，配置运营者令牌后另外接纳持该令牌的设备。官方公共节点显式开放 Relay（`relay_open`），开放时必须限制每个连接的读取速率和同时连接数；官方还限制同一来源地址的连接数，并可在配置里拒绝指定设备。

## 浏览器接入

`app.vibeface.dev` 是静态网页。它的 Nginx 经 TLS 把 `/api/client`、`/api/info` 和 `/api/decision` 反代给公共节点，钉住公共节点的自签证书，并透传 `Host: app.vibeface.dev` 和取自 Cloudflare 的浏览器地址 `X-Real-IP`，公共节点的网页端口只对 App 主机开放；公共节点以这个 origin 为 `public_url` 检查 Host 和 Origin。浏览器在 `connect` 帧里指定设备编号，公共节点经发现服务找到这台 Node，只搬运 Noise 密文。

转发名额用完时，新会话排队并收到 `{"type":"queued","position":n}`，页面显示“排队中，第 n 位”；队列也满，或同一浏览器地址占满它的份额时回 `gateway_busy`。每个会话的速率也受限。Node 默认不限速、不排队，只有官方公共节点在部署配置里设上限，见 [`deploy/README.md`](../../../deploy/README.md)。

## 信任边界

公共节点看不到对话内容，不保存个人网络的事件或附件。发现和转发不授予成员关系；加入仍需邀请、安全码核对和批准，连接目标的身份由端到端 Noise 验证。运营者可以看到设备编号、IP、查询关系和流量大小。

## 验证范围

- 本地公共节点和只配置其发现服务的全新成员 Node 完成登记，浏览器经公共节点加入、对话、在公共节点重启后重连，decision token 经公共节点送达，不使用生产凭证或模型。
- 转发名额用完时第二个浏览器排队，轮到后接通。
- 带运营者令牌的私有 Relay 接纳持正确令牌的 Node，拒绝持错误令牌的 Node。
- 只开 TLS 的 Node 用 `init --tls` 生成的证书应答钉住它的客户端，换一张证书时校验失败，明文 HTTP 得不到应答。
- 公共节点不保存事件、会话或日志；公共服务离线时，局域网功能保持可用。

本次保持 `DATA_EPOCH=1`，不修改聊天存储格式，不重放历史工作。
