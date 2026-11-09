# 全网层（Mesh）

每台 Node 默认加入同一个 iroh-gossip 主题，向全网宣告一条自己签名的记录，并持有、转发、持久化所有 Node 的记录。任何在表里的 Node 都能当入网起点，官方公共节点只是默认 bootstrap 节点和一台普通的志愿 relay 与网关。

## 记录

记录由设备密钥签名（域 `mesh-v1`），接收端验签，检查版本 `v` 为 1、网络名、`ttl_s` 在 60 到 3600 秒之间、未过期、`seq` 不超前本机时钟 5 分钟、地址和角色条数及 URL 形式后才入表。

| 字段 | 含义 |
|---|---|
| `v` | 记录格式版本，当前为 1 |
| `network` | `mesh.network`，默认 `vibeface` |
| `device_id` | 设备编号，即 iroh EndpointId |
| `seq` | 毫秒时间戳，单调递增，大者胜 |
| `ttl_s` | 有效期，默认 1800 秒 |
| `addrs` | 直连地址，最多 8 条：公网观察地址与本机绑定地址，不含别人的地址 |
| `relay_url` | 当前 home relay |
| `reachable` | `public`、`nat` 或 `unknown`，只影响别人选入网起点的顺序 |
| `roles` | `relay`、`gateway`、`service`，最多 4 个 |

单条记录不超过 1 KiB。Node 每 10 分钟重发一次，地址、relay 或角色变化时立即重发，两次宣告至少间隔 30 秒。表最多 65536 条，每个设备一条；新记录的 `seq` 必须不小于已持有记录的 `seq` 加 30 秒，发送方取当前时间与已持有 `seq` 加 30 秒中的较大者，乱序到达的旧包不会挡住新记录。IP 地址对全网可见。

表满时先丢最久没见过的过期记录，没有过期记录就丢最晚第一次见到的那条，所以早就认识的记录不会被一批新记录挤掉。本机记录、本个人网络成员 Node 和 bootstrap 节点的记录从不丢；表里全是这些记录时，新设备的记录不入表。每个邻居送来的新设备记录（表里还没有的设备）每秒最多入表 20 条、一次最多 200 条，超出的丢掉，经拉取送来的停止本轮拉取；成员 Node 和 bootstrap 节点送来的不限。已有设备的更新不计入。表的改动先在内存生效，约 200 毫秒内的改动合成一个事务写入 `mesh.sqlite3`，写盘时不占用表。

角色：

- `relay {url, access, rate, max_clients, offer?}`：公共 relay。`access` 为 `open`、`token` 或 `paid`。
- `gateway {url}`：对任何设备转发 `/api/client` 的 HTTPS 入口。
- `service {service: delivery, offer?}`：实验室文件传输的中转设备。

`offer {settlement: {chain_id, escrow, asset}, price}` 是付费字段，本版本只定义不收费。`settlement` 与实验室付费传输的 `chain` 配置同名同值，`price` 是资产最小单位的十进制整数；由谁签发凭证看记录的 `device_id`，不另写地址。

## 两种链接

两种链接都用 `vibeface://`，用途不同，互不接受。

| 链接 | 形式 | 用途 |
|---|---|---|
| 节点链接 | `vibeface://node/<设备编号>?addr=<主机>:<UDP 端口>&relay=<URL>` | 说明一台 Node 是谁、在哪里。可当 bootstrap，公开无妨 |
| 加入链接 | `vibeface://join/vibeface1…` | 加入一个个人网络，含一次性 secret，只交给自己的设备 |

节点链接最多 4 个 `addr`、1 个 `relay`，`addr` 的主机是 IP 或小写域名，`relay` 是最长 256 个字符、只含可见 ASCII 且不含 `` &#?%"<>\`{}|^ `` 的 http(s) URL，整条链接最长 2048 个字符。地址和 relay 只是拨号提示，QUIC 握手证明对端就是这台设备。`share_link` 返回本机当前的节点链接：公网地址在前，再是局域网和回环地址，去掉 IPv6 链路本地地址，加上当前 home relay。

加入链接的票据是 postcard 编码再小写 base32，内容是版本、入口 Node 的节点链接和 32 字节 secret，`vibeface1` 之后的 base32 正文最长 2400 个字符。入口 Node 在 `invite` 应答里给出自己的节点链接，管理设备把它和 secret 一起编进票据。新设备先把票据里的地址和 relay 放进地址簿，入口的 relay 同时拿到邀请凭证；再查全网和配置的 relay。新设备关闭全网、用别的网络名，或 bootstrap 节点连不上时，照样能连到入口。

## 入网与交换

主题 ID 由 `SHA-256("vibeface:mesh:v1:" ‖ network)` 派生，gossip 与交换各用一条带网络名的 ALPN：`vibeface/mesh-gossip/<network>/1`、`vibeface/mesh/<network>/1`。测试用随机网络名，不会进入真实网络。

启动时同时尝试本地旧表里最近见过且公网可达的 8 台（`mesh.allow_private` 打开时也含带地址的私网记录）和 `mesh.bootstrap`；本个人网络的成员 Node 在成员列表变化时加入。任一连上就加入主题。`mesh.bootstrap` 的每一项是节点链接，至少带一个 `addr` 或 `relay`，可以是你认识的任何一台 Node：在那台 Node 上运行 `vibeface mesh share`，或在网页设备页的「节点链接」复制。没有邻居时从 30 秒起加倍重试，最长间隔 5 分钟，个人网络内的功能不受影响。

gossip 只送新消息，单条消息最多 4096 字节。两台 Node 成为邻居时，各自向对方要摘要：请求带一个 Bloom 过滤器，装着自己持有的 `(device_id, seq)`，每条约 16 位、最多 64 KiB，每次随机换种子；对方只回过滤器里没有的记录。过滤器误判时那条记录这一轮拿不到，下一次拉取或该设备的下一次宣告会补上。同一条流也回答按设备编号的查询，作为表里没有目标时的兜底。

交换的限额：

- 请求帧最多 96 KiB，直接解析成 `digest`、`lookup` 或 `probe_relay`，未知类型或多余字段都拒绝。
- 每页最多 2000 条、约 4 MiB，有后续页时带 `more`；一次拉取最多 33 页。
- 全部对端合计同时回答 16 个请求，每个 30 秒内答完；发起方每个请求也等 30 秒。
- 同一设备的摘要每 10 秒一次（后续页不计），查询每秒最多 5 次，relay 探测每分钟一次。

表保存在数据目录的 `mesh.sqlite3`，保留 30 天内见过的记录。重启后先用旧表入网，最初的 bootstrap 节点不可达也能回到原来的网络。这个文件只是公共数据的缓存，删除后 Node 会重新从 bootstrap 节点入网。

## 发现

Mesh 实现 iroh 的地址查找：拨号时先查本地表，得到直连地址和 relay，iroh 并行尝试并自行打洞。表里的记录已过期时先把旧地址当拨号提示，再向邻居查询；表里没有时直接查询。查询最多问 3 个邻居，合计 8 秒。gossip 协议自带的邻居地址只当拨号提示，不改 relay 列表。

浏览器和手机不是 Node，不进表，经网关接入，网关 Node 用自己的表找到目标。

## 志愿 relay

Node 在网页设备页打开「公共转发」或运行 `vibeface relay volunteer on` 后运行一台开放 relay，默认监听 `0.0.0.0:3340`，每连接 16 KiB/s、最多 128 个连接、同一地址最多 8 个；设了 `relay_operator_token` 时不能打开。它依次请最多 3 个邻居从外部探测自己的 relay URL，任一通过才在记录里宣告 `relay` 角色，之后每 30 分钟再探测一次；都不通过时 60 秒后重试并加倍，最长 30 分钟，没有邻居时 10 秒后再试。探测方只回答通过或“the relay check failed”，不转述自己看到的错误。探测方先解析并检查 relay 的地址，`/generate_204` 和 relay 握手都连这个地址，不再解析第二次。

关闭只撤掉志愿时设的东西：开放、限额，以及只为志愿运行的 relay 的监听和 URL；`relay_operator_token` 和屏蔽列表不动，有屏蔽列表时 relay 继续为本个人网络运行。没有在志愿的 relay 关闭时不变。关闭后从记录里移除并重发。

其他 Node 每 5 分钟、以及表变化时（最多每分钟一次）重选 relay。每轮从整张表的 `access: open` relay 里取 32 台：先取历史上通过检查最多的，再从其余里随机取；同一网段（IPv4 /24、IPv6 /48，按之前检查时解析到的地址或 URL 里的 IP）最多取 2 台。其中 240 秒内没检查过的并行用 `/generate_204` 和真实握手检查。排名先看历史通过次数（每次通过加 1，最多 288，失败一次减半），再看延迟；连续 3 次失败的不用；同一网段只采用 1 台，取最好的 8 台加入 relay 列表。iroh 在其中按延迟选 home relay。检查结果保存在 `mesh_relays`。配置的 relay、本个人网络成员运行的 relay、bootstrap 节点的 relay（节点链接里的 `relay`，以及它记录里宣告的开放 relay）和自己的 relay 始终保留，不参与排名。`mesh_relays` 和 `relay_usage` 里 30 天没有更新的行在启动和每轮重选时删除。

私网和回环地址上的 relay 与网关默认不采用、不探测，记录里私网主机上的 `relay_url` 也不当拨号地址，`mesh.allow_private` 打开后才采用，供局域网和测试使用。

## 浏览器入口

`app.vibeface.dev` 是静态网页。首次连接使用 `config/public-bootstrap.json` 的 `gateway_url`；由 Node 提供的页面用该页面自己的 origin。连上 Node 后，`entrances` 返回本网络成员的网页入口和表里经验证的公共网关，网页把它们存为备选。验证方式是请求 `<url>/api/info`，返回的设备编号必须与记录一致：只接受 HTTPS，解析出的地址除非 `allow_private` 都必须是公网地址，连接已检查的地址，不跟随重定向，解析、连接和整个请求各限 5 秒，响应最多 16 KiB。每次最多检查最近见过的 32 个网关，返回 16 个；结果通过的保留 30 分钟，失败的 5 分钟。

## 付费地基

relay 前置代理按 endpoint 累计双向字节，写入 `mesh.sqlite3` 的 `relay_usage`。relay 接受运营者签发的凭证：绑定被授予的设备编号和到期时间，可限字节数。本版本不签发凭证，也不收费。

计划的购买流程：买方从表里选一条 `access: paid` 的 relay 记录，按其 `offer.settlement` 在实验室付费传输已用的同一 escrow、同一资产上锁定 `price`，不另建合约或钱包。买方按设备编号连到该记录的设备，交出锁定交易，运营方确认链上锁定后签发 `RelayGrant`，写明买方设备、到期时间和字节上限。买方把凭证当 relay token 使用，relay 按 `relay_usage` 计量，到期或用满即拒绝。结算沿用 escrow 的领取与退款期限，未用完的预算按同一规则退回。

## 配置

```json
"mesh": {"enabled": true, "network": "vibeface", "bootstrap": ["vibeface://node/<设备编号>?addr=<主机>:<UDP 端口>&relay=<relay URL>"], "allow_private": false}
```

`init` 默认开启并写入官方 bootstrap 节点。`--no-mesh` 关闭，`--mesh-network` 改网络名（不再带官方 bootstrap 节点），`--bootstrap` 指定你认识的第一台 Node，取它的节点链接（可重复，最多 16 个）。`vibeface mesh share` 打印本机运行中的节点链接，`vibeface mesh bootstrap add|remove` 修改列表，Node 运行时立即生效；`vibeface mesh status` 和 `vibeface mesh records [--role relay|gateway|service]` 查看全网。`NetworkConfig::default()` 关闭 Mesh，单元测试不访问公共网络。

## 信任边界

记录只说明一台设备在哪里、愿意提供什么。成员关系、任务权限和端到端身份仍由个人网络的签名历史和 Noise 握手决定。任何人都能宣称自己是 relay 或网关，采用方验证不过就不用。
