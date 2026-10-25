import { array, boolean, record, text } from '../lib/node.ts'
import type { DeliveryDeadlines, DeliveryDraft, DeliveryEvidence, DeliveryMode, DeliveryOrder, DeliveryPayment, DeliveryPhase, DeliveryStatus, PendingIntent, PaymentState, ReceivedFile } from './types.ts'

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
  if (typeof value !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value)) throw new Error('付款意图编号无效。')
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
      if (!/^0x[a-fA-F0-9]{64}$/.test(hash)) throw new Error('链上交易摘要无效。')
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
  if (windows.length > 16) throw new Error('订单窗口数超过协议上限。')
  const order: DeliveryOrder = {
    order_id: peer(v.order_id), intent_id: intentId(v.intent_id), role: member(v.role, ['sender', 'receiver', 'provider']), mode: member(v.mode, ['charity', 'paid']), acceptance: member(v.acceptance, ['recoverable', 'strict']),
    source_peer: peer(v.source_peer), receiver_peer: peer(v.receiver_peer), provider_peer: peer(v.provider_peer), name: nullable(v.name, fileName), bytes: integer(v.bytes, protocolMaxBytes), sha256: nullable(v.sha256, peer),
    phase: member(v.phase, ['prepared', 'funding_pending', 'proving', 'funded', 'transferring', 'cipher_held', 'key_published', 'recoverable', 'paid', 'partial', 'refundable', 'refunded', 'reconciling', 'key_unavailable', 'failed']),
    evidence: member(v.evidence, ['none', 'charity_receipt', 'ciphertext_held', 'recoverable', 'decryption_proven']), payment: payment(v.payment), deadlines: deadlines(v.deadlines),
    windows: windows.map((item, index) => {
      const w = record(item), bytes = integer(w.bytes, 65_536)
      if (integer(w.index, 15) !== index) throw new Error('订单窗口不连续。')
      return { index, bytes, committed_bytes: integer(w.committed_bytes, bytes + 68), state: member(w.state, ['pending', 'stored', 'held', 'key_published', 'earned', 'refunded']), decryption_proven: boolean(w.decryption_proven) }
    }),
    created_at_ms: integer(v.created_at_ms), updated_at_ms: integer(v.updated_at_ms), error: nullable(v.error, text),
  }
  if (order.mode === 'charity') {
    const p = order.payment
    if (order.acceptance !== 'recoverable' || p.state !== 'free' || [p.delivery_fee, p.reservation_fee, p.priority_fee, p.total, p.gas_cap, p.earned, p.withdrawn, p.refunded].some(value => value !== '0')
      || [p.chain_id, p.asset, p.payer, p.provider_wallet, p.tx_hash].some(value => value !== null)
      || !['none', 'charity_receipt'].includes(order.evidence) || order.windows.some(window => window.decryption_proven)) throw new Error('公益订单只能使用免费收据，不能带付款或强证明条件。')
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
export function canRestart(order: DeliveryOrder): boolean {
  if (order.mode === 'charity') return order.phase === 'recoverable' && order.evidence === 'charity_receipt' && order.windows.length > 0
    && order.windows.every(window => ['held', 'key_published', 'earned'].includes(window.state))
  return order.phase === 'paid' && ['earned', 'withdrawn', 'confirmed'].includes(order.payment.state) && order.windows.length > 0 && order.windows.every(window => window.state === 'earned')
    || order.phase === 'refunded' && order.payment.state === 'refunded' && order.payment.refunded === order.payment.total
}
export function walletNotice(wallet: DeliveryStatus['wallet']): string {
  switch (wallet.state) {
    case 'absent': return '未创建钱包。公益转发仍可使用。'
    case 'locked': return '钱包已锁定。订单仍可查看，付款需在发送设备本机解锁。'
    case 'unlocked': return '钱包已在本机解锁。每笔付款仍需单独确认。'
    case 'unavailable': return '钱包暂不可用。请在发送设备 本机检查，从独立钱包备份恢复后重启。公益转发和订单查看仍可使用。'
  }
}

export const phaseLabels: Record<DeliveryPhase, string> = {
  prepared: '待确认', funding_pending: '已授权，等待处理', proving: '正在生成交付证明', funded: '资金已托管', transferring: '传输中', cipher_held: '对方已持有密文', key_published: '密钥包已发布',
  recoverable: '文件可恢复', paid: '交付条件已结算', partial: '部分交付', refundable: '可申请退款', refunded: '已退款', reconciling: '正在查询原交易', key_unavailable: '解密密钥暂不可用', failed: '未完成',
}
export const evidenceLabels: Record<DeliveryEvidence, string> = { none: '尚无交付确认', charity_receipt: '公益收据', ciphertext_held: '密文持有证明', recoverable: '链上已确认可恢复', decryption_proven: '已证明实际解密' }
export const paymentLabels: Record<PaymentState, string> = {
  free: '免费', prepared: '未付款', submitted: '交易已提交', included: '已打包，等待确认', confirmed: '链上已确认', earned: '服务商已取得债权', withdrawn: '服务商已提款', refunded: '已退还', unknown: '交易结果未知，需对账', failed: '付款失败',
}
export function canRead(order: DeliveryOrder): boolean {
  return order.role !== 'provider' && (order.role === 'sender' || order.evidence === 'charity_receipt' || order.evidence === 'recoverable' || order.evidence === 'decryption_proven')
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
    ['密文持有截止', d.hold_deadline], ['密钥包发布截止', d.key_deadline], ['保留义务截止', d.retain_until],
  ]
  return rows.map(([label, seconds]) => [label, new Date(seconds * 1000).toLocaleString(undefined, { timeZoneName: 'short' })])
}
