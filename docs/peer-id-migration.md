# Peer ID 命名

Peer ID 是设备公钥原始字节的 SHA256，使用 64 个小写十六进制字符。设备编号与密钥算法不变，不采用 libp2p 的 PeerId 编码。

## 统一格式

| 边界 | 当前名称 |
|---|---|
| Rust 类型和身份方法 | `PeerId`、`peer_id()` |
| WASM 和浏览器身份方法 | `peerId()` |
| JSON、RPC、CLI、邀请、连接记录和证书投影 | `peer_id` |
| Peer 握手、发现记录、中转预约和签名字节 | `peer_id` |
| mDNS TXT、维护 Health、更新 Journal | `peer_id` |
| 恢复挑战 | `target_peer_id` |
| blob 下载清单 | `source_peer_id` |
| SQLite 设备编号列 | `peer_id` |

本项目尚未正式发布，直接使用统一格式，不保留旧类型别名、旧字段读入或双写。签名与验签均使用当前字段生成的规范字节，所有通信端使用同一版本。

`cluster_id` 仍表示集群编号。`subject_id` 仍表示签名成员编号。`Node` 运行角色、`NodeRole`、`target_node`、`scheduler_node` 和 `entry_node` 的语义不变。当前设备授权目录为 `ClusterState`，成员使用 `subject_id`。

## 数据目录

当前 `data_epoch=6`，设备授权与业务共用的 `node.sqlite3` 使用 `user_version=6`。旧目录在打开数据库前被拒绝，不自动迁移或清空。运行新版时使用新的数据目录初始化，并为各设备重新配对。保留旧目录可供人工核对，不要把程序回退当作数据格式转换。

旧邀请和旧浏览器连接记录不受支持。浏览器保留自身密钥，重新输入新版入口或邀请。设备授权已合入同一个 Store，不再维护独立身份数据库 schema。集群恢复代次不随数据目录格式递增。
