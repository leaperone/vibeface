# 默认公共发现与连接引导

个人 Node 首次启动后自动连接默认发现服务，取得经过身份验证的公共入口和 Relay，再将自己的入口地址写入签名地址记录。手机 App 和浏览器使用同一发现服务的 HTTPS 接口取得入口，仍优先使用现有的局域网直连。

## 使用方式

普通用户初始化并启动 Node 即可。CLI 初始化和 Core 首次初始化目录时应用公共默认值。底层 `NetworkConfig::default()` 与测试用本机配置保持离线，不因单元测试而访问公共服务。

`--bootstrap off` 关闭公共引导。自建服务使用指定设备编号的 Discovery 和可选的 HTTPS 引导地址；自建配置不在失败时偷偷切到官方服务。显式配置的私有入口仍可使用运营者授权方式。

默认服务的信息集中保存在 `config/public-bootstrap.json`，由 Rust 和共享客户端共同使用。设备编号只定义一次，UDP 地址由它与主机、端口组合得到。

## 服务目录

Discovery 是服务目录和公共接入许可的签发者。目录使用原始 JSON 字节及独立签名，客户端以配置中钉住的设备编号验证签名。UDP 和 HTTPS 返回同一种结构。

```text
SignedCatalog { payload: string, signature: hex }
Catalog {
  version: 1,
  issuer: device_id,
  issued_at_ms: integer,
  expires_at_ms: integer,
  entries: [{ gateway_url: origin, relay_url: origin }]
}
```

每个入口与对应 Relay 成对发布，不能把不同服务的网关和 Relay 任意组合。目录最多包含四项，整个签名响应最多 4096 字节，公共地址使用 HTTPS，回环 HTTP 仅用于明确的本机夹具。拒绝凭证、查询、fragment、超长地址、过期数据和错误签名。

Discovery 的 UDP 协议增加目录查询和接入许可请求。HTTPS 对应 `GET /bootstrap/catalog` 和 `POST /bootstrap/grant`。HTTPS 接口由同一个 Discovery 实例提供，可通过现有公网入口反代；网页代码、连接网关和 Relay 可以使用同一域名。

## Relay 接入许可

公共客户端不内置或复制运营者的 `entry-token`。Discovery 给能够证明自己设备密钥的 Node 签发短期许可，Relay 只接受配置的签发者。

```text
SignedGrant = base64url(raw_claim_json) + "." + hex(signature)
Grant {
  version: 1,
  issuer: device_id,
  device_id: connecting_node,
  relay_url: origin,
  issued_at_ms: integer,
  expires_at_ms: integer,
  nonce: hex
}
```

签名域分别为 `bootstrap-catalog`、`bootstrap-grant`、`bootstrap-request`。HTTPS 请求同样采用 `{payload, signature}`，其中 payload 包含 `device_id`、`relay_url`、`at_ms` 和随机 nonce，由请求设备签名。UDP 请求的身份由已有 QUIC TLS 连接证明。

Relay 校验签名者、连接的设备编号、目标 Relay、签发时间和到期时间。许可不授予个人网络成员资格，成员授权和端到端 Noise 身份验证继续独立执行。

默认许可有效期为 30 分钟，最多一小时，提前续期并带抖动。许可字符串最多 1024 字节，请求正文最多 1024 字节。签发限额按已验证的设备和来源地址计算，计数表和连接总数有界。许可到期只阻止新接入，不重放业务任务。

## 运行时行为

Core 先创建本机网络端点，再在后台完成目录获取、许可申请和 Relay 注册。UDP 不可用时，目录和许可通过 HTTPS 获取，Relay 仍通过 HTTPS/WebSocket 工作。所有公共基础设施不可用时，Node 的本机服务和局域网发现仍可启动。

网络模块对外提供一个引导状态快照，只有已经获得有效许可的入口才用于签名地址记录。状态变化触发原有地址刷新机制。重复的 Relay URL 不得使带许可的配置被无凭证配置覆盖。

浏览器和 APK 不申请 Relay 许可。它们验证目录，在现有局域网和已知入口尝试之后，将目录入口作为额外候选。已有入口失效时也能尝试目录入口，不能只在候选列表完全为空时引导。连接目标始终来自本人已授权设备，目录不能授予或伪造成员关系。

## 验证范围

- 本地 Discovery、公共入口和全新成员 Node 能自动完成发现、许可、Relay 注册与客户端连接，不使用生产凭证或模型。
- 封锁 UDP 的夹具通过 HTTPS 完成同一流程。
- 公共服务离线时，局域网功能保持可用。
- 校验过期、错误签发者、错误设备、错误 Relay、篡改、超长请求和许可续期。
- 通过真实 Relay 连接验证凭证替换和重复 URL 合并，不以内部 map 值代替运行结果。

本次保持 `DATA_EPOCH=1`，不修改聊天存储格式，不重放历史工作。
