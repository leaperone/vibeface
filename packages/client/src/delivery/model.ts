import { array, boolean, record, text, type Member } from '../lib/node.ts'
import type { Acceptance, DeliveryDeadlines, DeliveryDraft, DeliveryEvidence, DeliveryMode, DeliveryOrder, DeliveryPayment, DeliveryPhase, DeliveryStatus, PendingIntent, PaymentState, ReceivedFile } from './types.ts'

const maxAmount = (1n << 256n) - 1n
const protocolMaxBytes = 16 * 65_536
function member<const T extends string>(value: unknown, values: readonly T[]): T {
  const found = values.find(candidate => candidate === value)
  if (!found) throw new Error('文件传输状态无法识别，请先刷新，勿重复付款。')
  return found
}
function integer(value: unknown, max = Number.MAX_SAFE_INTEGER): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0 || value > max) throw new Error('文件传输的大小或编号无效。')
  return value
}
export function peer(value: unknown): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) throw new Error('设备连接 ID 无效。填入 64 位小写 a-f 和数字 0-9。')
  return value
}
function address(value: unknown): string {
  if (typeof value !== 'string' || !/^0x[a-fA-F0-9]{40}$/.test(value)) throw new Error('订单的钱包或资产地址无效。')
  return value
}
function nullable<T>(value: unknown, parse: (input: unknown) => T): T | null { return value === null ? null : parse(value) }
export function amount(value: unknown): string {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,77})$/.test(value) || BigInt(value) > maxAmount) throw new Error('金额须为代币最小单位的非负整数，不能带小数。')
  return value
}
export function fileName(value: unknown): string {
  const name = text(value, '文件名')
  if (name !== name.trim() || name === '.' || name === '..' || /[\\/:\u0000-\u001f\u007f]/.test(name) || new TextEncoder().encode(name).length > 128) throw new Error('文件名须为 1 至 128 字节，不能包含路径或控制字符。')
  return name
}
export function intentId(value: unknown): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value)) throw new Error('付款编号无效。')
  return value
}
export function parseStatus(value: unknown): DeliveryStatus {
  const v = record(value), wallet = record(v.wallet)
  const state = member(wallet.state, ['absent', 'locked', 'unlocked', 'unavailable'])
  const receiving = v.receiver_key === null ? null : record(v.receiver_key)
  if ((state === 'absent' || state === 'unavailable') && wallet.address !== null) throw new Error('未创建或不可用的钱包不能带地址。')
  return {
    enabled: boolean(v.enabled), charity_enabled: boolean(v.charity_enabled), paid_enabled: boolean(v.paid_enabled),
    node_peer: peer(v.node_peer), wallet: state === 'absent' ? { state, address: null } : state === 'unavailable' ? { state, address: null, reason: text(wallet.reason) } : { state, address: address(wallet.address) },
    max_file_bytes: integer(v.max_file_bytes, protocolMaxBytes),
    receiver_key: receiving ? { key_id: peer(receiving.key_id), serial: integer(receiving.serial), public_key: peer(receiving.public_key) } : null,
  }
}
function payment(value: unknown): DeliveryPayment {
  const v = record(value)
  const result: DeliveryPayment = {
    state: member(v.state, ['free', 'prepared', 'submitted', 'included', 'confirmed', 'earned', 'withdrawn', 'refunded', 'unknown', 'failed']),
    chain_id: nullable(v.chain_id, amount), asset: nullable(v.asset, address), payer: nullable(v.payer, address), provider_wallet: nullable(v.provider_wallet, address),
    delivery_fee: amount(v.delivery_fee), reservation_fee: amount(v.reservation_fee), priority_fee: amount(v.priority_fee), total: amount(v.total), gas_cap: amount(v.gas_cap),
    earned: amount(v.earned), withdrawn: amount(v.withdrawn), refunded: amount(v.refunded),
    tx_hash: nullable(v.tx_hash, value => {
      const hash = text(value)
      if (!/^0x[a-fA-F0-9]{64}$/.test(hash)) throw new Error('交易摘要无效。')
      return hash
    }),
  }
  if (BigInt(result.delivery_fee) + BigInt(result.reservation_fee) + BigInt(result.priority_fee) !== BigInt(result.total)
    || BigInt(result.withdrawn) > BigInt(result.earned) || BigInt(result.earned) + BigInt(result.refunded) > BigInt(result.total)) throw new Error('订单金额对不上，已暂停付款。')
  return result
}
function deadlines(value: unknown): DeliveryDeadlines | null {
  if (value === null) return null
  const v = record(value)
  return { quote_expires_at: integer(v.quote_expires_at, 8_640_000_000_000), hold_deadline: integer(v.hold_deadline, 8_640_000_000_000), key_deadline: integer(v.key_deadline, 8_640_000_000_000), open_deadline: integer(v.open_deadline, 8_640_000_000_000), retain_until: integer(v.retain_until, 8_640_000_000_000), priority_deadline: integer(v.priority_deadline, 8_640_000_000_000) }
}
export function parseOrder(value: unknown): DeliveryOrder {
  const v = record(value)
  const windows = array(v.windows)
  if (windows.length > 16) throw new Error('订单分段数超过上限。')
  const order: DeliveryOrder = {
    order_id: peer(v.order_id), intent_id: intentId(v.intent_id), role: member(v.role, ['sender', 'receiver', 'provider']), mode: member(v.mode, ['charity', 'paid']), acceptance: member(v.acceptance, ['recoverable', 'strict']),
    source_peer: peer(v.source_peer), receiver_peer: peer(v.receiver_peer), provider_peer: peer(v.provider_peer), name: nullable(v.name, fileName), bytes: integer(v.bytes, protocolMaxBytes), sha256: nullable(v.sha256, peer),
    phase: member(v.phase, ['prepared', 'funding_pending', 'proving', 'funded', 'transferring', 'cipher_held', 'key_published', 'recoverable', 'paid', 'partial', 'refundable', 'refunded', 'reconciling', 'key_unavailable', 'failed']),
    evidence: member(v.evidence, ['none', 'charity_receipt', 'ciphertext_held', 'recoverable', 'decryption_proven']), payment: payment(v.payment), deadlines: deadlines(v.deadlines),
    windows: windows.map((item, index) => {
      const w = record(item), bytes = integer(w.bytes, 65_536)
      if (integer(w.index, 15) !== index) throw new Error('订单分段不连续。')
      return { index, bytes, committed_bytes: integer(w.committed_bytes, bytes + 68), state: member(w.state, ['pending', 'stored', 'held', 'key_published', 'earned', 'refunded']), decryption_proven: boolean(w.decryption_proven) }
    }),
    created_at_ms: integer(v.created_at_ms), updated_at_ms: integer(v.updated_at_ms), error: nullable(v.error, text),
  }
  if (order.mode === 'charity') {
    const p = order.payment
    if (order.acceptance !== 'recoverable' || p.state !== 'free' || [p.delivery_fee, p.reservation_fee, p.priority_fee, p.total, p.gas_cap, p.earned, p.withdrawn, p.refunded].some(value => value !== '0')
      || [p.chain_id, p.asset, p.payer, p.provider_wallet, p.tx_hash].some(value => value !== null)
      || !['none', 'charity_receipt'].includes(order.evidence) || order.windows.some(window => window.decryption_proven)) throw new Error('免费订单不能带付款或付款条件。')
  } else if (order.payment.state === 'free' || order.payment.chain_id === null || order.payment.asset === null || order.payment.payer === null || order.payment.provider_wallet === null) throw new Error('付费订单缺少固定付款信息。')
  if (order.mode === 'paid') {
    const d = order.deadlines
    if (!d || d.quote_expires_at === 0 || d.quote_expires_at > d.hold_deadline || d.hold_deadline >= d.key_deadline || d.key_deadline > d.open_deadline || d.open_deadline > d.retain_until
      || d.priority_deadline <= d.quote_expires_at || d.priority_deadline > (order.acceptance === 'strict' ? d.open_deadline : d.key_deadline)) throw new Error('订单的报价、交付或时效截止对不上，已暂停付款。')
  }
  if (order.role === 'provider' && (order.name !== null || order.sha256 !== null)) throw new Error('服务商视图不能包含明文文件信息。')
  return order
}
export function parseOrders(value: unknown): DeliveryOrder[] {
  const rows = array(record(value).orders)
  if (rows.length > 1024) throw new Error('订单列表过大。')
  const orders = rows.map(parseOrder)
  if (new Set(orders.map(order => order.order_id)).size !== orders.length) throw new Error('订单列表包含重复记录。')
  return orders
}
export function parseReceived(value: unknown, limit: number): ReceivedFile {
  const v = record(value)
  if (typeof v.content_base64 !== 'string') throw new Error('文件内容缺失。')
  const content = v.content_base64
  if (content.length > 4 * Math.ceil(limit / 3)) throw new Error('取回文件超过当前设备上限。')
  return { name: fileName(v.name), content_base64: content, sha256: peer(v.sha256), bytes: integer(v.bytes, limit) }
}
export function toBase64(bytes: Uint8Array): string {
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}
export function fromBase64(value: string): Uint8Array {
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)) throw new Error('文件编码无效。')
  const bytes = Uint8Array.from(atob(value), char => char.charCodeAt(0))
  if (toBase64(bytes) !== value) throw new Error('文件编码不是规范格式。')
  return bytes
}
export async function sha256(bytes: Uint8Array): Promise<string> {
  return [...new Uint8Array(await crypto.subtle.digest('SHA-256', new Uint8Array(bytes)))].map(byte => byte.toString(16).padStart(2, '0')).join('')
}
export async function checkedFile(file: ReceivedFile, order: DeliveryOrder): Promise<Uint8Array> {
  const bytes = fromBase64(file.content_base64)
  if (bytes.length !== file.bytes || file.bytes !== order.bytes || order.name !== null && file.name !== order.name || order.sha256 !== null && file.sha256 !== order.sha256 || await sha256(bytes) !== file.sha256) throw new Error('文件摘要、名称或大小与订单不一致，未保存。')
  return bytes
}
export function parsePending(value: unknown): PendingIntent {
  const v = record(value)
  return { intent_id: intentId(v.intent_id), fingerprint: peer(v.fingerprint) }
}
export function changeMode(draft: DeliveryDraft, mode: DeliveryMode): DeliveryDraft {
  return { ...draft, mode, acceptance: mode === 'charity' ? 'recoverable' : draft.acceptance }
}
/** What gets sent: without 实验功能 only free delivery, since paid runs on a test chain. */
export const draftFor = (draft: DeliveryDraft, experiments: boolean): DeliveryDraft => experiments ? draft : changeMode(draft, 'charity')
export function canRestart(order: DeliveryOrder): boolean {
  if (order.role !== 'sender') return false
  if (order.mode === 'charity') return order.phase === 'recoverable' && order.evidence === 'charity_receipt' && order.windows.length > 0
    && order.windows.every(window => ['held', 'key_published', 'earned'].includes(window.state))
  return order.phase === 'paid' && ['earned', 'withdrawn', 'confirmed'].includes(order.payment.state) && order.windows.length > 0 && order.windows.every(window => window.state === 'earned')
    || order.phase === 'refunded' && order.payment.state === 'refunded' && order.payment.refunded === order.payment.total
}
export function walletNotice(wallet: DeliveryStatus['wallet']): string {
  switch (wallet.state) {
    case 'absent': return '未创建钱包。免费传输仍可使用。'
    case 'locked': return '钱包已锁定。订单仍可查看，付款需在发送设备本机解锁。'
    case 'unlocked': return '钱包已在本机解锁。每笔付款仍需单独确认。'
    case 'unavailable': return '钱包暂不可用。请在发送设备本机检查，从独立钱包备份恢复后重启。免费传输和订单查看仍可使用。'
  }
}

export const phaseLabels: Record<DeliveryPhase, string> = {
  prepared: '待确认', funding_pending: '正在付款', proving: '正在确认付款条件', funded: '已付款', transferring: '传输中', cipher_held: '对方已收到，等待解锁', key_published: '正在解锁',
  recoverable: '已送达', paid: '已完成', partial: '部分完成', refundable: '可以退款', refunded: '已退款', reconciling: '正在确认付款结果', key_unavailable: '暂时打不开', failed: '未完成',
}
export const evidenceLabels: Record<DeliveryEvidence, string> = { none: '还没送达', charity_receipt: '对方已收到', ciphertext_held: '对方已收到，还没解锁', recoverable: '对方可以打开', decryption_proven: '对方已打开' }
export const paymentLabels: Record<PaymentState, string> = {
  free: '免费', prepared: '未付款', submitted: '付款已提交', included: '付款处理中', confirmed: '已付款', earned: '服务费已结算', withdrawn: '服务费已提取', refunded: '款已退回', unknown: '付款结果未知', failed: '付款失败',
}
export function settlementAmountsLabel(order: DeliveryOrder): string {
  return order.payment.state === 'unknown' || order.payment.state === 'failed' || order.phase === 'reconciling' || order.phase === 'failed'
    ? '上次记录（赚取 / 提款 / 退款，到账待确认）'
    : '已赚取 / 已提款 / 已退款'
}
export function canRead(order: DeliveryOrder): boolean {
  return order.mode === 'charity' && order.role === 'sender'
    || order.role === 'receiver' && (order.evidence === 'charity_receipt' || order.evidence === 'recoverable' || order.evidence === 'decryption_proven')
}
export function needsRefresh(order: DeliveryOrder): boolean {
  return !['prepared', 'paid', 'refunded', 'failed', 'key_unavailable'].includes(order.phase) || order.payment.state === 'unknown'
}
export function deadlineRows(order: DeliveryOrder): Array<[string, string]> {
  const d = order.deadlines
  if (order.mode !== 'paid' || !d) return []
  const rows: Array<[string, number]> = [
    ['报价确认截止', d.quote_expires_at], ['时效优先费截止', d.priority_deadline],
    ['交付结算硬截止', order.acceptance === 'strict' ? d.open_deadline : d.key_deadline],
    ['对方收到文件截止', d.hold_deadline], ['密钥包发布截止', d.key_deadline], ['保留义务截止', d.retain_until],
  ]
  return rows.map(([label, seconds]) => [label, new Date(seconds * 1000).toLocaleString(undefined, { timeZoneName: 'short' })])
}

/** Where a file can go without pasting an ID: my other computers, since a Node is what receives. */
export const receiverChoices = (members: readonly Member[], self: string): Member[] => members.filter(item => item.kind === 'node' && item.device_id !== self)

export const modeOptions: Array<{ value: DeliveryMode; label: string }> = [{ value: 'charity', label: '免费传输' }, { value: 'paid', label: '付费传输' }]
export const acceptanceOptions: Array<{ value: Acceptance; label: string }> = [{ value: 'recoverable', label: '对方能取回后付款' }, { value: 'strict', label: '对方打开后付款' }]

/** Sizes for people: bytes below 1 KB, then KB or MB. `floor` keeps a limit from rounding up past what fits. */
export function sizeLabel(bytes: number, floor = false): string {
  if (bytes < 1024) return `${bytes} 字节`
  const [value, unit] = bytes < 1024 * 1024 ? [bytes / 1024, 'KB'] : [bytes / 1024 / 1024, 'MB']
  const digits = value < 10 ? 1 : 0
  const scale = 10 ** digits
  return `${(floor ? Math.floor(value * scale) / scale : value).toFixed(digits)} ${unit}`
}
export function orderTitle(order: DeliveryOrder): string {
  if (order.name) return order.name
  return order.role === 'provider' ? '代为转发的文件' : '待接收的文件'
}
/** One line under a transfer record: 免费 · 48 KB · 已送达. */
export function orderSummary(order: DeliveryOrder): string {
  return `${order.mode === 'charity' ? '免费' : '付费'} · ${sizeLabel(order.bytes)} · ${phaseLabels[order.phase]}`
}
export function orderStatus(order: DeliveryOrder): string {
  return order.mode === 'paid' ? `${phaseLabels[order.phase]} · ${paymentLabels[order.payment.state]}` : phaseLabels[order.phase]
}
export function orderHeading(order: DeliveryOrder): string {
  return order.role === 'sender' && order.phase === 'prepared' ? '确认发送' : '传输详情'
}
/** What this device can do with a transfer now; a file is saved only after the send is confirmed. */
export function orderActions(order: DeliveryOrder) {
  return {
    confirm: order.role === 'sender' && order.phase === 'prepared',
    accept: order.role === 'receiver' && !canRead(order) && order.phase !== 'refunded' && order.phase !== 'failed',
    save: canRead(order) && order.phase !== 'prepared',
  }
}
export const transferCopy = {
  offline: '重新连接后可查看传输记录，不会重复付款。',
  loading: '正在读取传输设置。',
  off: '打开后可以加密收发文件',
  adminOnly: '只有管理设备能更改设置和发送',
  free: '全程加密 · 免费',
  connectionId: '连接 ID · 收文件时发给对方',
  charity: '免费传输',
  paid: '付费传输',
  sendTo: '发给',
  pickDevice: '选一台设备',
  otherDevice: '发给别人的设备',
  receiver: '接收方的连接 ID',
  provider: '中转设备的连接 ID',
  providerHint: '只经手加密后的文件，不会加入你的设备',
  noRecords: '还没有传输记录',
  serviceCap: '服务费上限',
  gasCap: '链费上限',
  serviceTotal: '服务费合计',
  units: '金额按测试代币和测试链原生币的最小单位计。',
  strict: '对方确认打开后才付款。对方离线或没确认时，中转设备可能收不到费用。',
  walletFirst: '先在发送设备本机创建或解锁钱包，再刷新。',
  unknownPayment: '付款结果还没确定。点“检查进度”会查询原来那笔，不会重复付款。',
  walletDetails: '钱包的创建、解锁和备份通过本机命令行完成。个人恢复文件不包含钱包和订单解密密钥。',
  prepareDetails: '下一步会把文件交给当前连接的可信设备加密，还不付款。同一文件和条款不会重复发送。原订单完成后，选择相同文件和条款，点“作为新订单再次发送”才会另建一笔。',
  prepare: (mode: DeliveryMode) => mode === 'paid' ? '查看费用' : '下一步',
  confirm: (mode: DeliveryMode) => mode === 'paid' ? '确认并付款' : '确认发送',
  maxSize: (bytes: number) => `最大 ${sizeLabel(bytes, true)}`,
  tooLarge: (bytes: number) => `文件超过 ${sizeLabel(bytes, true)}，换一个小点的`,
} as const
