# 文件交付实验参考

VDP-1 为本人文件增加独立的免费传输和付费传输。传输默认关闭。节点发现不收费，不需要钱包，也不授予个人网络成员权限。普通会话同步和附件仍使用原来的私人网络协议。

当前付费实现只接受字面回环 HTTP 地址上的 `chain_id=31337` 测试链。仓库提供 `TestToken`，不接入真实 USDT、主网、法币或平台币。此实验不提供长期存储证明。

## 角色与信任

| 角色 | 职责 |
| --- | --- |
| 发送 Node，S | 接收本人选择的文件，切分并加密，固定条款，生成 source 证明并付款。 |
| 服务 Node，P | 提供报价，暂存密文 C 和接收方加密密钥包 E，按合约条件发布 E 并提取已赚费用。 |
| 接收 Node，R | 经本人授权接收，完整持有 C，生成 hold 证明，取得 E 后解密并生成 open 证明。 |

S 和 R 可以属于不同个人网络。P 可以是没有个人网络成员关系的 `public_entry`。服务通道认证 PeerID，个人网络通道另外验证成员资格。付款、报价或发现记录均不能访问私人事件、附件、Agent 或模型密钥。

浏览器先把明文文件交给当前连接的可信 S。传输加密保护 S、R 与 P 之间的对象，不能隔离恶意的发送端、接收端或提供客户端代码的网页 origin。P 看不到文件名和正文，但能看到 PeerID、对象大小、哈希、时间和访问关系。付费合约还公开钱包、条款、证明和 E。E 是给 R 的加密密钥包，链上没有明文文件密钥。

## 实验边界

当前候选配置固定在 [`proofs/delivery/profile.json`](../proofs/delivery/profile.json)。Node 拒绝其他 image、schema 或资源配置。

| 项目 | 上限或规则 |
| --- | --- |
| 每个窗口明文 | 65536 字节。 |
| 窗口数 | 2。 |
| 窗口明文总量 | 131072 字节，包含私有文件封装。 |
| 客户端原文件 | 130816 字节，文件名另限 128 字节。 |
| 每个 C | 对应窗口明文长度加 68 字节。 |
| 每个 E | 80 字节。 |
| guest cycles | 64000000。 |
| profile 内存上限 | 17179869184 字节，实际运行还必须受 Linux cgroup 硬限制。 |

付费 S 和 R 需要匹配 profile 的本机 prover、可信 `r0vm` 和 Linux cgroup v2。cgroup 必须禁用 swap，并限制进程数不超过 64。P 只验证和转交证明，不需要本机 prover。构建、guest 执行、真实 Groth16 证明、资源限制和链上结算是分别验收的结果。

公益采用独立签名条款和加密上下文，当前 Node 使用相同的文件容量边界。它不伪造链地址、钱包、registry 或付费证明，只有公益签名收据。

## 交付与结算

付费订单按窗口推进。S 的 source 证明校验签名、完整有序 manifest，以及明文、C、E 和条款的对应关系。R 的 hold 证明包含完整 C、接收私钥和接收同意签名。open 证明实际执行 E 解封及 C 解密，不接受外部声称的明文或文件密钥。

| 结算条件 | 窗口费用何时变为 P 的收入 |
| --- | --- |
| 普通验收 `recoverable` | 合约已接受 hold，P 在期限内原子发布完整且哈希匹配的 E。R 已证明持有 C 和相应私钥，因此具备恢复条件。 |
| 严格验收 `strict` | 合约已接受 hold 和 E，并在期限内接受 R 的 open 证明。 |

`recoverable` 不表示 R 已经执行解密。`decryption_proven` 证明指定 R 的计算完成了解密，不证明真人阅读、文件内容有商业价值，或副本永远可用。严格验收依赖 R 提交 open。R 离线、拒绝提交或丢失密钥时，P 可能无法收款。

这些证明校验内容对应关系及 R 持有的材料，不能计量真实物理路径、P 独有的带宽贡献或防止参与方串通。此版本按完成窗口结算，不把证明当作网络流量计量。

普通订单的 open 可以补充更强证据，不重复计费。E 发布后不提供删除入口。已发布 E 仍需与 R 保有的完整 C 和旧接收密钥配合使用。

## 费用和期限

服务费为所有窗口的 `delivery_fee`、`reservation_fee`，再加整单 `priority_fee`。金额均为测试代币最小单位的十进制整数。链费预算单独以测试链原生币最小单位表示，不属于服务费托管。

传输费和暂存预留费只在该窗口达到约定验收条件时一起赚取。预留费购买暂存容量承诺，不是按持续存储证明支付的租金。时效优先费只在所有窗口均赚取、且最后完成不晚于独立的 `priority_deadline` 时赚取一次。默认测试费率不是商业定价或美元报价。

期限单位为 Unix 秒，由订单固定，重试不能更改。

| 字段 | 含义 |
| --- | --- |
| `quote_expires_at` | 最晚付款入约时间。 |
| `hold_deadline` | 最晚接受 hold 的时间。 |
| `key_deadline` | 最晚发布 E 的时间，也是普通验收的结算硬截止。 |
| `open_deadline` | 最晚接受严格 open 的时间。 |
| `retain_until` | 订单约定的材料保留期限。 |
| `priority_deadline` | 整单时效优先费的独立截止。 |

条款要求 `quote_expires_at <= hold_deadline < key_deadline <= open_deadline <= retain_until`。时效截止必须晚于报价截止，且不晚于所选验收的硬截止。超过时效截止仍可在硬截止内交付，P 只失去时效优先费。

未 hold 的窗口在 hold 超时，或接收 registry 已轮换、撤销时可退款。已 Held 的窗口须等 E 发布超时。严格模式已发布 E 的窗口须等 open 超时。退款只退该窗口未赚取的两项服务费，不撤销其他窗口收入。已有退款终态的窗口不能再次赚取。

时效优先费在已不可能赚取时单独退回。付款人只能在没有 Held、KeyPublished 或 Earned 窗口时取消整单。任何 keeper 都能提交合约允许的证明、退款或提款，但退款地址固定为付款人，提款地址固定为 P。链上 revert、代币冻结或转账异常不会消除尚未支付的债权。已消耗 gas 不在服务退款范围内。

## 本机设置和钱包

交付设置保存在数据目录的 `delivery-settings.json`，由 `vibeface wallet --data-dir <目录> configure <JSON文件>` 应用。它与个人网络的 `config.json` 分开，配置不改变设备身份。默认值如下。

| 字段 | 默认值或要求 |
| --- | --- |
| `enabled` | `false`。服务总开关。 |
| `charity_enabled` | `true`，仅在总开关打开后生效。 |
| `paid_enabled` | `false`。 |
| `storage_limit_bytes` | `67108864`。对象、私密材料和证明收据共享配额，允许范围为 131208 至 1073741824。 |
| `max_orders` | `128`，允许范围为 1 至 128，历史终态订单也占用槽位。 |
| `chain` | 默认 `null`。付费时必须指定 `rpc_url`、`chain_id`、`escrow`、`asset`、完整 `profile` 和 `confirmations`。 |
| `prover` | 默认 `null`。证明端指定绝对路径 `program`、`r0vm`、`cgroup` 和完整 `profile`。 |
| `delivery_fee_per_window` | `"10"`，测试代币最小单位。 |
| `reservation_fee_per_window` | `"2"`，测试代币最小单位。 |
| `priority_fee` | `"5"`，测试代币最小单位。 |
| `provider_gas_cap` | `"0"`，付费 P 必须另行授权非零链费预算。 |

网页只能更改三个服务开关，链部署、prover 路径和钱包由本机所有者配置。创建、解锁和锁定通过同用户维护 socket 完成。钱包口令由关闭回显的 TTY，或所有者私有文件、管道的 `--passphrase-fd` 输入，不放进参数、环境变量、网页或聊天。口令须为单行 UTF-8，长度 12 至 1024 字节。

实验版没有订单归档接口。达到订单上限后拒绝新单，已有订单继续可查询和取回。订单账本和旧接收密钥必须随完整数据目录保留。

EVM 钱包独立于 Ed25519 设备身份和 X25519 接收密钥。钱包创建后保持锁定，每次 Node 重启也保持锁定。`absent` 表示未创建，`unavailable` 表示现有钱包无法读取，不能当作未创建并覆盖。锁定的钱包可以观察原交易，不能签新交易或重播原 raw transaction。

## 接收密钥和恢复材料

接收密钥注册、轮换和撤销只通过本机所有者命令完成，不向网页或 Agent 开放。操作不更换设备 PeerID 或 EVM 钱包。

```sh
vibeface wallet --data-dir <目录> register-key --intent-id <UUID> --gas-cap <原生币最小单位>
vibeface wallet --data-dir <目录> rotate-key --intent-id <UUID> --gas-cap <原生币最小单位>
vibeface wallet --data-dir <目录> revoke-key --intent-id <UUID> --gas-cap <原生币最小单位>
```

轮换生成新的 X25519 接收密钥。撤销递增 registry serial，并停止新订单使用旧 key。两者均保留旧 KeyId 材料，已 Held 订单继续使用原密钥。尚未 Held 的旧 serial 窗口在 registry 确认后不能继续 hold，可以按合约退款。撤销后只有本机所有者以新 UUID 显式 `register-key` 才重新开启接收，读取状态不会自动恢复它。

注册、轮换和撤销都要求显式 UUID，同一操作重试使用原 UUID 和原 gas 预算。UUID 固定 action、profile 和部署，未知结果复用原 binding、serial、证明、nonce 和签名交易。另有未决 key 操作时先对账，不另起 serial。只有响应 `registry_confirmed=true` 才代表 registry 已按确认策略生效，`submitted`、`included`、`confirming` 和 `unknown` 均未完成。

`wallet backup` 只导出加密 EVM 钱包，不覆盖已存在的目标。它不包含接收私钥、订单、密文或续传和交易账本。个人根身份恢复文件也不含这些材料。停掉 Node 后备份完整数据目录，包括 `identity.json`、`delivery-settings.json`、`delivery-wallet.json` 和整个 `delivery/`。该目录含私钥及私密证明材料，须按凭证保管。

恢复时保留原意图和交易账本，先对账。不要运行同一数据目录的两份副本，也不要只用旧钱包备份另起一个付款实例。缺少旧接收密钥时明确显示 `key_unavailable`，不能以新 key 冒充原订单解密成功。

## 重试、状态和文件取回

订单意图 `intent_id` 为 UUID，订单内容和条款固定。确认、刷新、断线恢复和“检查进度”使用原意图。签名前在 SQLite 事务中保留 nonce、精确 calldata 和链费预算，签名交易落盘后广播。结果未知时查询原交易和链上状态，只在仍有授权时重播原字节，不换 nonce 再付款。

客户端按网络、客户端、源 Node、文件和条款保存意图。同一文件的普通准备复用原意图。只有原订单终态已由 Node 核对，用户显式选择“作为新订单再次发送”才创建另一笔订单。刷新和轮询不接受订单、不确认费用，也不重新付款。

正常取消和停机会结束证明进程组。强杀 Node 后，已授权的本机证明可能继续运行；继承的文件锁阻止重启后并发证明。旧任务退出后，Node 先验真原收据再恢复。接收密钥操作继续使用原 UUID，钱包重启后仍须本机解锁。

| 证据 | 能确认的事实 |
| --- | --- |
| `none` | 尚无交付证据。 |
| `charity_receipt` | 公益签名收据，无链上强证明和付款。 |
| `ciphertext_held` | 合约已接受指定 R 持有完整 C 和相应接收私钥的证明。 |
| `recoverable` | 完整 C 与正确 E 已具备恢复条件。 |
| `decryption_proven` | 合约已接受指定 R 实际解封、解密的证明。 |

订单阶段、交付证据和资金状态分别展示。`included` 只表示入块，`confirmed` 按配置的确认深度观察，`earned` 为合约债权，`withdrawn` 才表示实际提款。浅确认不是绝对终局。检测到 reorg 或无法继续核实原交易时，Node 撤回暂定完成状态并进入 `reconciling` 或 `unknown`。

接收端取回先在 Node 重组并核对文件，客户端再核对完整长度、文件名和 SHA-256。Web 的保存动作只确认已交给浏览器，不能声称用户磁盘已写入。Mobile 写入独立文档文件后再次核对，再提供分享副本。

## 构建与验收入口

规范 ABI 为 [`crates/delivery/abi/Statement.sol`](../crates/delivery/abi/Statement.sol)，Rust 关系和字节约定见 [领域 API](../crates/delivery/API.md)。规范构建与真实证明入口见 [proof 构建说明](../proofs/delivery/README.md)。合约、只读结算观察器和本地部署工具见 [合约说明](../contracts/delivery/README.md)。

`deploy-local.mjs` 只在字面回环地址启动独立测试链，部署正常 escrow、固定 verifier 和 TestToken，输出公开配置，不输出私钥。完整 Node 验收使用 [e2e 的付费模式](e2e.md#验证付费交付和实际结算)，在同一受限 Linux 环境生成真实证明、完成双窗口结算并只读复核原链。缺失真实 fixtures 必须使 proof gate 失败，不能换成 always-true verifier 或旧 image 的证明。
