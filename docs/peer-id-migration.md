# Peer ID 命名与兼容

Peer ID 是设备公钥原始字节的 SHA256，使用 64 个小写十六进制字符。更名不改变编号、密钥、网络归属或签名 domain，不采用 libp2p 的 PeerId 编码。网络编号仍为 `network_id`。

## 兼容矩阵

本 PR 的内部标准是 `PeerId`、`peer_id()` 和 `peer_id`。兼容读入发生在边界，不向严格 JSON 对象同时写两个键。

| 边界 | 新版写出 | 新版读入旧数据 | 旧版读取新版 | 策略 |
|---|---|---|---|---|
| HTTP `/api/info`、RPC `info` / `nodes`、CLI `status --json` / `access --json` | `peer_id` | 输入不适用 | 依赖 `node_id` 的调用方需升级 | Web、工具和验收脚本同步迁移 |
| 配对邀请、待配对文件、Web 连接记录 | `peer_id` | 接受 `node_id` | 旧客户端需升级后读取新记录 | Rust serde alias，Web 边界双读 |
| `NodeCertificate` 授权投影 | `peer_id` | serde alias 接受 `node_id` | 依赖旧字段的调用方需升级 | 它不是签名成员载荷 |
| v1 Peer Hello、Reachability、relay Reserve | `node_id` | 接受 `node_id` 或 `peer_id` | 可读 | Rust 字段改名，v1 序列化名固定 |
| Reachability 与 ReservationProof 签名字节 | `node_id` | 保留旧规范编码 | 可验签 | 不改顺序、内容或 domain |
| 维护 socket Health、更新 Journal | `node_id` | 接受新旧字段 | 可读 | 保住旧 updater 探活与回滚 |
| mDNS TXT | `peer_id` 和 `node_id` | 双读 | 可发现 | 同一值双广告 |
| Rust `Identity` / `MembershipSigner` 与 WASM | `peer_id()` / `peerId()` | 保留 `node_id()` / `nodeId()` 转发 | 旧方法仍可用 | 旧方法弃用，内部不再调用 |
| SQLite 历史列和身份文件 | 不改物理格式 | 可读 | 可读 | 不改历史 migration，无重写 |
| TrustBundle 对应的 `NetworkState` | 仍用 `subject_id` | 可读 | 可读 | 当前没有 TrustBundle 类型或 `node_id` 成员键 |

## 双读与弃用期

从包含本 PR 的版本起，新版接受旧邀请、连接记录、证书和待配对文件。保存时只写 `peer_id`。不要把新版普通 JSON 或新生成邀请交给依赖 `node_id` 的旧客户端。先升级管理端、CLI 和脚本，再生成新邀请。回退客户端时使用旧版生成的邀请与记录。

`node_id` 输入别名和旧 SDK 方法至少保留整个 wire v1 兼容期，本 PR 不设自动移除日期。删除别名、删除 SDK 方法、改变签名字节或停止写出 v1 旧键，必须另开协议版本迁移 PR，并验证混合版本。当前同时提交两个同义 JSON 键会被 Rust 严格解析拒绝，Web 也拒绝同义键冲突。

## 范围与验证

`Node` 运行角色、`NodeRole`、`subject_id`、`target_node`、`scheduler_node` 与 `entry_node` 语义不变。签名恢复挑战的 `target_node_id` 维持 v1 编码。用户界面统一使用 Peer ID、设备编号和网络编号。

验证覆盖新旧 JSON 读取、固定 v1 签名字节、旧 SDK 别名、Web 旧连接读取，以及真实 CLI 与临时身份进程。NET-1 的 store 事务、relay 恢复和可靠性实现不属于本 PR。
