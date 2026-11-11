# 受保护预付预算实验

此功能只用于手动启用的本机实验室、loopback EVM 31337 和六位精度的本地 TestToken。`enabled`、`paid_enabled` 默认都是 `false`。免费发现、公益传输及公益收据继续免费，不创建付款义务；已经收到的公益文件不能事后升级为受保护付费订单。

发送方 S 在服务前把精确条款不可撤销地分配到预付预算。Provider P 只在 genuine Source、Receiver R 的 genuine Hold 和正确 80 字节恢复包 E 满足原条件时赚取费用，后续不依赖 S 在线或再次签名。当前产品只接受 Recoverable：R 持有正确密文并具有恢复明文的条件，不代表真人已读。Strict 产品入口已撤下，不会自动改写成 Recoverable。

## 预算和发送

预算固定 payer、payee、Provider PeerID、asset 和一次存款金额。多 Provider 使用不同预算。Node 固定当前构建的 DeliveryEscrow、官方 adapter/verifier、产品 profile 和 TestToken runtime，不接受自定义 verifier、公开 RPC、主网或真实资产。

在 S、P 的本机维护命令中创建并解锁独立钱包，备份保管方式沿用钱包命令。密码只通过受保护 FD 传递。R 先用现有 `register-key` 操作完成真实接收密钥注册，再准备订单；历史接收密钥仍保留给已分配订单。

S 和 R 各自管理个人网络，可以属于不同网络。P 是普通公共 Node，可以不加入二者的个人网络。服务通道认证 PeerID，个人网络通道另外验证成员资格；服务 PeerID 不授予个人网络成员权限。付款、报价或发现记录不能访问私人事件、附件、Agent 或模型密钥。

浏览器把明文文件交给当前连接的可信 S。传输加密不能隔离恶意发送端、接收端或提供客户端代码的网页 origin。P 看不到文件名和正文，但能看到 PeerID、对象大小、哈希、时间和访问关系。合约公开钱包、条款、证明和 E；E 是给 R 的加密密钥包，不是明文文件密钥。

```sh
node contracts/delivery/deploy-local.mjs --listen 127.0.0.1:0 --output "$VDP_RUN/deployment.json"
vibeface wallet --data-dir "$VDP_SOURCE" budget open \
  --intent-id "$VDP_OPEN_UUID" --payee "$VDP_PAYEE" --provider-peer "$VDP_PROVIDER_PEER" \
  --deposit 10 --gas-cap "$VDP_PAYER_CAP_WEI"
vibeface wallet --data-dir "$VDP_SOURCE" budget status --budget-id "$VDP_BUDGET_ID"
```

开户命令先精确 approve，再 openBudget。同一 UUID 和条款重试原 intent，不重开预算。存款、服务金额都是测试代币最小单位；gas cap 是原生币 wei。payer 和 provider 各自的 cap 按预算累计，包括开户/分配、Hold、揭钥、提款以及失败 attempt 的保守最大 gas 承诺；换交易不能重置它。`provider_gas_cap` 在 P 第一次接受该预算时固定，不再按每个订单重新授权。

现有实验室文件传输页选择预付预算，固定中转设备及累计链费上限，再准备文件。Prepare 必须包含 `budget_id`，只接受 `acceptance: "recoverable"`。确认单单使用原 `confirm`；两笔不同的单窗口订单先分别 prepare，再明确确认同批：

```json
{"op":"confirm_batch","intent_ids":["<A UUID>","<B UUID>"]}
```

同一 Source 和 Receiver 各最多处理两个活跃窗口，原操作锁及证明锁继续生效。批次确认一次事务保存两份明确授权；后台按原证明执行器逐份生成 Source，全部就绪后才单笔 reserveBatch。预算不足会整批拒绝；未完成分配之前不提供付费传输。页面展示两笔订单的名称和费用后，由管理成员确认该批，不替用户自动确认别的草稿。

`wallet budget reserve --budget-id ID --order-ids A B --intent-id UUID` 可显式提交已经确认且 Source 证明齐全的本机订单。正常 confirm/confirm_batch 已完成分配时不必再执行它。

## Hold、批次揭钥和提款

R 明确接受订单，完整密文落盘后产生 Hold。P 先核对接收方同意签名，再用已核验部署的 adapter 对同一 Hold statement 验证 seal，通过后才耐久保存授权，不立即公开 E。无效证明或临时 RPC 错误不占用队列，可重新提交有效证明。R 等待批次是正常可恢复状态；实际恢复后不会再自动启动 Open 证明。显式 Open 仅能为已经 Earned 的窗口增加解密证据，不能阻止付款。

P 可以在现有实验室预算区或本机 CLI 先查看就绪窗口，再明确结算当前最多两个窗口：

```sh
vibeface wallet --data-dir "$VDP_PROVIDER" budget status --budget-id "$VDP_BUDGET_ID"
vibeface wallet --data-dir "$VDP_PROVIDER" budget flush --budget-id "$VDP_BUDGET_ID"
```

状态中的 `ready_windows` 只公开 budget/order/index，表示 P 已耐久收到 Hold 请求、恢复包完整、链上分配仍处于 Funded/Held 且未入其他 claim。它不包含 seal、E、calldata 或 raw。链上 Hold 尚未确认时，不把本机队列标成已经确认的持有证据。

flush 固定选中的窗口，持久授权后以一笔 holdBatch 核验逐窗 Hold，再进入唯一的持久 publication claim 路径。后台只推进已有 flush 授权、恢复 claim 和累计提款，不抢先把第二份 Hold 尚未完成的第一窗自动发出。revealBatch 不转代币；逐窗 Earned 和预算累计债权独立保存。withdrawBudget 把 `earned - withdrawn` 一次转给固定 payee，任何 keeper 都不能改收款人。

```sh
vibeface wallet --data-dir "$VDP_PROVIDER" budget withdraw \
  --budget-id "$VDP_BUDGET_ID" --intent-id "$VDP_WITHDRAW_UUID"
```

Order 的 `withdrawn` 保持 0，因为提款归属预算。页面分别展示本单 earned/refunded 和预算累计 earned/withdrawn/refunded，不能把整个预算提款算作每一单分别到账。token 冻结或转账失败保留 Earned 债权，实际到账仍依赖 token 恢复及成功提款。

## 首次披露门与恢复

P 必须显式配置本机 `publication_policy`，未配置时不提供受保护付费报价或自动揭钥。示例值只用于受控本地链，不是公网 SLA：

```json
{
  "watchdog_seconds": 32,
  "inclusion_seconds": 5,
  "finality_seconds": 5,
  "clock_skew_seconds": 2,
  "max_fee_per_gas": "3000000000",
  "priority_fee_per_gas": "1000000000"
}
```

W 是本机最多两个活动 claim 和一个历史复查槽的调度界；每槽有 10 秒执行截止，轮询间隔 1 秒。I/F/S 分别是配置明确承担的 canonical 执行、后续最终性和时钟误差边界。网络或主机不满足这些边界时必须报告可用性风险，配置数字本身不证明边界已经成立。

publication claim 与签名 attempt 分开。claim 固定预算、payee、窗口/E、完整金额截止、目标金额、原累计 cap 和 policy；每个 nonce 一份不可变原 reveal attempt，同 nonce 的清理候选另存不可变签名账本。五种状态是 PrivateReady、DisclosedOutstanding、FinalEarned、FinalSettled、ExpiredUnrevealed。签好 raw 仍是私密状态。

已赚取或已结束的 claim 仍按原 hash 对账每份 attempt，不用窗口收入推断某笔原交易成功。经济截止后的 nonce 清理也持续对账。FinalSettled 表示本批原窗口和奖金权利已全部结束，款项可能部分赚取、其余退款，且恢复材料可能已公开。只有每个原 nonce 各有 canonical confirmed 或 reverted 的原交易或无害清理交易、同 anchor 的逐窗费用及奖金与 earned/refunded 精确核对后，才记录候选终态；再经过原 F+S 的本机与 confirmed 链时间双重等待、重新验证原 anchor 和全部终态，才释放活动槽。未知、浅确认、重组、时钟异常或未结奖金会清除候选并继续占槽。claim、首次披露事实、原签名和累计 cap 不删除。FinalSettled 的历史复查发现反证或无法确认终态时恢复 Outstanding 并重新占槽，阻止新增队列绕过不确定结果。FinalEarned 保留从首次可能披露时刻起 policy.margin() 的原延迟释放规则；历史读取失败或超时会保留该状态并显示风险，不自动重新占槽。完整 canonical 读取确认已赚取条件不再成立时，才沿既有路径恢复 Outstanding；若只观察到某份 attempt 重组而后续读取失败，仍按前述历史风险规则处理。这是原 I/F/S 假设边界，不构成绝对最终性保证。

产品 publication 路径禁止任何含 E 的 estimateGas、eth_call、simulation。公开固定材料的 `experiment-delivery.mjs` 故障对照会故意调用 `revealBatch.staticCall` 并绕过 Node gate；这些原始提前披露/回滚反例不构成产品 gate 通过，也不改写旧 FAIL。有限两窗 reveal 使用离线固定 800000 gas：没有 verifier 调用，每项固定 80 字节及有界 storage/事件；相关真实有限形状和合成账务用例检查该上界。runtime 改变时须重新核对，不能用真实 E 去估算。首次外发前检查原 cap、原生币余额、固定 fee 足额和无低 nonce 阻塞；raw、nonce、claim 关联原子落盘。紧邻首次可能含 E 的 RPC 再检查 `now + W + I + F + S < economic_cutoff`，耐久记录可能披露，之后才广播。RPC 超时或错误仍是 Outstanding，不能按失败另付。

已可能披露的同一 claim 恢复只受原经济截止和原累计授权限制，不重新套完整首次余量；否则合法披露后的临界回滚会被本机错误阻断。canonical revert 消耗 nonce 后，先对账所有原 attempt 和窗口，再按同一 claim、固定收款人及原总 cap 生成新 nonce attempt。未知、pending nonce 前进都不证明失败。经济截止前维持固定费率；原 raw 仅可在原披露 gate 和 nonce 条件仍成立时原样重播。

经济截止后不再广播含 E 的 reveal。原 nonce 未终结时，专用签名动作只允许同 chain、wallet、nonce 对固定 escrow 调用零 value、精确 calldata 的 `profileId()` getter。原 reveal 和各份清理 raw/hash 分别保留，发送不同字节前先核对全部 hash 和 latest/pending nonce。清理使用固定 30000 gas，可在原 reveal 已预留的最大 gas 成本内提高费用，不额外消耗一份累计 gas 预算。只有一个同 nonce 候选能在 canonical 链上消耗 gas。清理交易 confirmed 或 reverted 均表示 nonce 已消耗，原 reveal 单独记为 conflicted，绝不据此记为 earned。

重启后继续使用原清理账本。钱包锁定、原授权撤销、paid/lab 关闭、时钟不符、余额不足、配额不足或当前 fee 超出原预留成本时保留记录并暂停；恢复条件后继续处理。它不承诺任意费率和资源条件下的活性。

完整 17 由 10 传输费、2 预留费和有条件奖金 5 组成；承诺完整17的经济截止是 keyDeadline 与 priorityDeadline 的较早值。奖金为0的订单只受 keyDeadline；首次在奖金截止后组建的合法 base-only claim 只计原条款仍能赚取的费用，页面显示原报价与该批目标金额，不声称仍能赚17。已经生成的完整金额 claim 不悄悄改价、延长截止或重置 cap。

关闭实验室、关闭 paid、锁钱包或更改授权会停止新签名/广播，保留 claim/raw 并允许只读链上对账。用户暂停唯一执行者、无 gas、长于界限的停链/重组/审查、分配本身回滚或 fee 超出固定策略，可能导致 E 已知但收入未 canonical 成立。此模型不承诺绝对公平或任意公网活性。

## 关闭和退款

```sh
vibeface wallet --data-dir "$VDP_SOURCE" budget close \
  --budget-id "$VDP_BUDGET_ID" --intent-id "$VDP_CLOSE_UUID"
vibeface wallet --data-dir "$VDP_SOURCE" budget refund \
  --budget-id "$VDP_BUDGET_ID" --intent-id "$VDP_REFUND_UUID" --windows "$VDP_ORDER_ID:0"
```

只有 payer 能 close；它立即禁止新增分配，不能取消活跃权利。事后 key 轮换、撤销、其他窗口失败都不能提前撤回已经分配的款项。有限退款批次最多两个窗口，须晚于预算全部分配的最大 keyDeadline。已 Earned 费用永不退给 payer；未 Earned 部分及未赚取奖金按原条款退款。普通 keepers 可以退款到固定 payer；未花尽预算仍需退款操作，已精确花尽的关闭预算不要求零金额转账。

R 拒绝 Hold 时没有 delivery earned，预算到期可退，P 的资源支出不一定获得补偿。Source/Hold+E 不证明中继实际物理路径、独有带宽或持续存储时长；传输/预留费只是报价组成，没有新增存储或带宽证明。

## 本机安全与容量

网页只设置服务开关；链部署、prover 和独立钱包由本机所有者配置。钱包口令通过关闭回显的 TTY 或受保护 FD 输入，不进入参数、环境变量、网页或聊天。历史接收密钥缺失时订单显示 `key_unavailable`，停止证明与恢复，保留原订单、密文、恢复包和债权；须从独立备份恢复，不能生成新密钥冒充旧密钥。

订单、对象、私密材料、证明收据、交易账本和 gas 预算共享 `storage_limit_bytes`。本人授权订单、未接受的外来 Receiver 和外部 Provider 分三类；后两类各不超过 `max_orders` 和字节上限的四分之一，合计不超过总上限，数据库及对象文件另受同一物理上限约束。预算交易、Hold、claim 和其窗口关联按同一实际归属计入配额，未找到归属不会自动归到本人容量。历史订单只在当前 canonical 逐窗和奖金会计完整、预算收入已提取，且 Receiver 确已本地恢复时释放活动槽；记录及字节继续计入。未知、重组、缺密钥或待办使订单重新活跃，claim 另行持续对账。

签名、nonce、累计 gas 及本次 attempt 绑定在同一事务内提交；签名或配额失败回滚本次预留，保留既存逻辑 claim、旧 raw 和未知广播账本。最早 unsigned intent 未恢复前禁止新 nonce。Provider 仅在保留期限已过、历史复查成立且没有预算、claim 或 attempt 的待处理引用时回收密文。过期未资助报价须由确认锚后的 Funded 日志与原期限核验，RPC 错误或重组不能当作未付款。

## 验证范围

完整付费闭环尚未通过验收。此前的完整实验只完成并独立验证了 KeyBinding 证明，Source 证明达到既定时限，未导出 Source 或 Hold 证明。批量结算、独立提款、离线恢复和最终重启验收仍未验证；本实现继续作为默认关闭的实验室功能提供。

默认相关检查仍执行合约编译/trust、`run.mjs` 受保护资金正反例、schema2 证据边界自检、官方 verifier 和真实 fixture seal 验真。账务测试使用明确标记的 digest test verifier，不能替代 genuine proof 或真实 Node。

原 fee5 公益拒签、genuine17 提前披露/退款失败保留在固定旧源码及任务证据中，不因新名称变成通过。新 fee5 必须创建发送前选择的新 paid 订单并产生对应 genuine Source/Hold；旧17不能改成5，也不能变成不同订单的第二窗。最小两单样本需要一份 KeyBinding、A/B 各一份 Source/Hold，共五份真实证明。必须实际观察 reserve/hold/reveal 各自含两个不同 slot、payer 离线、R 恢复原文件及预算累计真实提款。

旧 `--delivery-channel` 和 Strict/13-proof live 协议已撤下；旧 `verify-node.py --delivery`/`run-delivery-live.py` 明确拒绝原启动方式。正式 native 入口是 `run-delivery-live.py --protected-package <same-head-linux-package> --source-commit <HEAD>`，由宿主或默认关闭的手动 CI 共用；已准备受限环境和核验制品时，也可使用低层 `--protected-config` 或 `verify-node.py --delivery-protected`；配置、资源和命令见 [e2e](e2e.md#验证受保护付费交付)。`run-delivery-live.py --protected-selfcheck` 只自检配置与公开证据边界，不启动进程或证明。genuine5/双单运行、实际重启、渲染及制品证据必须分别完成；只读 schema2 observer、genuine17 重验或本地单元通过不能替代这些覆盖。
