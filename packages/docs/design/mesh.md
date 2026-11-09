# 全网层（Mesh）

每台 Node 默认加入同一个 iroh-gossip 主题，向全网宣告一条自己签名的记录，并持有、转发、持久化所有 Node 的记录。任何在表里的 Node 都能当入网起点，官方公共节点只是默认 bootstrap 节点和一台普通的志愿 relay 与网关。

## 记录

记录由设备密钥签名（域 `mesh-v1`），接收端验签、检查网络名、有效期和序号后才入表。

| 字段 | 含义 |
|---|---|
| `network` | `mesh.network`，默认 `vibeface` |
| `device_id` | 设备编号，即 iroh EndpointId |
| `seq` | 毫秒时间戳，单调递增，大者胜 |
| `ttl_s` | 有效期，默认 1800 秒 |
| `addrs` | 直连地址，最多 8 条：公网观察地址与本机绑定地址，不含别人的地址 |
| `relay_url` | 当前 home relay |
| `reachable` | `public`、`nat` 或 `unknown`，只影响别人选入网起点的顺序 |
| `roles` | `relay`、`gateway`、`service`，最多 4 个 |

单条记录不超过 1 KiB。Node 每 10 分钟重发一次，地址、relay 或角色变化时立即重发，两次宣告至少间隔 30 秒。表最多 65536 条，每个设备一条；新记录的 `seq` 必须不小于已持有记录的 `seq` 加 30 秒，发送方取当前时间与已持有 `seq` 加 30 秒中的较大者，乱序到达的旧包不会挡住新记录。IP 地址对全网可见。

角色：

- `relay {url, access, rate, max_clients, offer?}`：公共 relay。`access` 为 `open`、`token` 或 `paid`。
- `gateway {url}`：对任何设备转发 `/api/client` 的 HTTPS 入口。
- `service {service: delivery, offer?}`：实验室文件传输的中转设备。

`offer {asset, price, escrow, grant_endpoint}` 是付费字段，本版本只定义不收费。

## 入网与交换

主题 ID 由 `SHA-256("vibeface:mesh:v1:" ‖ network)` 派生，gossip 与交换各用一条带网络名的 ALPN：`vibeface/mesh-gossip/<network>/1`、`vibeface/mesh/<network>/1`。测试用随机网络名，不会进入真实网络。

入网候选依次是：本地旧表里最近见过且公网可达的 8 台、本个人网络的成员 Node、`mesh.bootstrap`。任一连上就加入主题。全部失败时退避重试，个人网络内的功能不受影响。

gossip 只送新消息。两台 Node 成为邻居时，各自把 `(device_id, seq)` 摘要发给对方，再只取回自己缺少或更旧的记录。同一条流也回答按设备编号的查询，作为表里没有目标时的兜底。

表保存在数据目录的 `mesh.sqlite3`，保留 30 天内见过的记录。重启后先用旧表入网，最初的 bootstrap 节点不可达也能回到原来的网络。这个文件只是公共数据的缓存，删除后 Node 会重新从 bootstrap 节点入网。

## 发现

Mesh 实现 iroh 的地址查找：拨号时先查本地表，得到直连地址和 relay，iroh 并行尝试并自行打洞。表里没有时向邻居查询，8 秒超时。gossip 协议自带的邻居地址只当拨号提示，不改 relay 列表。

浏览器和手机不是 Node，不进表，经网关接入，网关 Node 用自己的表找到目标。

## 志愿 relay

Node 在网页设备页打开「公共转发」或运行 `vibeface relay volunteer on` 后运行一台开放 relay，默认每连接 16 KiB/s、最多 128 个连接、同一地址最多 8 个。它先请一台邻居从外部探测自己的 relay URL，探测通过才在记录里宣告 `relay` 角色。关闭后从记录里移除并重发。

其他 Node 从表里取开放 relay，逐个用 `/generate_204` 和真实握手验证，按延迟和历史成功率评分，取最好的 8 台加入 relay 列表。iroh 在其中按延迟选 home relay。评分保存在 `mesh_relays`。配置的 relay、本个人网络成员运行的 relay 和自己的 relay 始终保留。

私网和回环地址上的 relay 与网关默认不采用、不探测，`mesh.allow_private` 打开后才采用，供局域网和测试使用。

## 浏览器入口

`app.vibeface.dev` 是静态网页。首次连接使用 `config/public-bootstrap.json` 的 `gateway_url`。连上 Node 后，`entrances` 返回本网络成员的网页入口和表里经验证的公共网关，网页把它们存为备选。验证方式是请求 `<url>/api/info`，返回的设备编号必须与记录一致。

## 付费地基

relay 前置代理按 endpoint 累计双向字节，写入 `mesh.sqlite3` 的 `relay_usage`。relay 接受运营者签发的凭证：绑定被授予的设备编号和到期时间，可限字节数。本版本不签发凭证，也不收费。收费要等预付预算与结算上线。

## 配置

```json
"mesh": {"enabled": true, "network": "vibeface", "bootstrap": ["vibeface://<设备编号>@<主机>:<UDP 端口>?relay=<relay URL>"], "allow_private": false}
```

`init` 默认开启并写入官方 bootstrap 节点。`--no-mesh` 关闭，`--mesh-network` 改网络名（不再带官方 bootstrap 节点），`--bootstrap` 指定 bootstrap 节点（可重复，最多 16 个）。`vibeface mesh bootstrap add|remove` 修改列表，Node 运行时立即生效；`vibeface mesh status` 和 `vibeface mesh records [--role relay|gateway|service]` 查看全网。`NetworkConfig::default()` 关闭 Mesh，单元测试不访问公共网络。

## 信任边界

记录只说明一台设备在哪里、愿意提供什么。成员关系、任务权限和端到端身份仍由个人网络的签名历史和 Noise 握手决定。任何人都能宣称自己是 relay 或网关，采用方验证不过就不用。
