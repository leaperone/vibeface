import { useEffect, useMemo, useRef, useState } from 'react'
import type { ReadyConnection } from '../lib/node.ts'
import { messageOf, record, same } from '../lib/node.ts'
import type { KeyValue, Platform } from '../lib/platform.ts'
import { amount, canRead, canRestart, checkedFile, fileName, intentId, needsRefresh, parseOrder, parseOrders, parsePending, parseReceived, parseStatus, peer, sha256, toBase64 } from './model.ts'
import type { DeliveryDraft, DeliveryOrder, DeliverySettings, DeliveryStatus, PendingIntent, TransferFile } from './types.ts'

function authorization(order: DeliveryOrder) {
  const p = order.payment
  return { order_id: order.order_id, intent_id: order.intent_id, mode: order.mode, acceptance: order.acceptance, source_peer: order.source_peer, receiver_peer: order.receiver_peer, provider_peer: order.provider_peer,
    name: order.name, bytes: order.bytes, sha256: order.sha256, deadlines: order.deadlines,
    payment: { chain_id: p.chain_id, asset: p.asset, payer: p.payer, provider_wallet: p.provider_wallet, delivery_fee: p.delivery_fee, reservation_fee: p.reservation_fee, priority_fee: p.priority_fee, total: p.total, gas_cap: p.gas_cap } }
}

export function shouldPollDelivery(enabled: boolean, orders: readonly DeliveryOrder[]): boolean {
  return enabled || orders.some(needsRefresh)
}

export function createDeliveryClient(connection: ReadyConnection, storage: KeyValue) {
  const intentPrefix = `vibeface.delivery.${connection.principal.network_id}.${connection.principal.device_id}.${connection.entry.device_id}`
  const current = () => {
    if (!connection.isCurrent()) throw new Error('连接已切换。先查询原订单，不会自动重新付款。')
  }
  const manage = () => {
    current()
    if (connection.principal.source !== 'membership' || !connection.principal.can_manage) throw new Error('只有管理设备能确认传输和付款。')
  }
  async function request(op: string, params: Record<string, unknown> = {}): Promise<unknown> {
    current()
    const result = await connection.request('delivery', { op, ...params })
    current()
    return result
  }
  function pending(fingerprint: string): PendingIntent | null {
    try {
      const saved = storage.getItem(`${intentPrefix}.intent.${fingerprint}`)
      if (saved === null) return null
      const intent = parsePending(JSON.parse(saved))
      if (intent.fingerprint !== fingerprint) throw new Error('intent fingerprint mismatch')
      return intent
    } catch { throw new Error('读不了上次的发送记录。先在传输记录里点“检查进度”，不要清除应用数据或重新付款。') }
  }
  function recent(): string | null {
    const saved = storage.getItem(`${intentPrefix}.recent`)
    return saved === null ? null : intentId(saved)
  }
  async function findIntent(id: string): Promise<DeliveryOrder | null> {
    const value = record(await request('find_intent', { intent_id: intentId(id) }))
    if (value.order === null) return null
    const order = parseOrder(value.order)
    if (order.intent_id !== id || order.role !== 'sender' || order.source_peer !== connection.entry.device_id) throw new Error('查询结果不属于原来那次发送。')
    return order
  }
  const client = {
    current, pending, recent, findIntent,
    async status() {
      const status = parseStatus(await request('status'))
      if (status.node_peer !== connection.entry.device_id) throw new Error('传输状态不属于当前连接的设备。')
      return status
    },
    list: async () => parseOrders(await request('list')),
    async configure(settings: DeliverySettings) { manage(); return parseStatus(await request('configure', settings)) },
    async prepare(draft: DeliveryDraft, file: TransferFile, status: DeliveryStatus, restartIntentId?: string) {
      manage()
      if (draft.mode === 'charity' && draft.acceptance !== 'recoverable') throw new Error('公益转发不能选“对方打开后付款”。')
      if (!status.enabled || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled) throw new Error('这台设备尚未启用所选传输方式。')
      if (!Number.isSafeInteger(file.size) || file.size < 0 || file.size > status.max_file_bytes) throw new Error(`文件须在 ${status.max_file_bytes} 字节以内。`)
      const bytes = new Uint8Array(await file.arrayBuffer())
      if (bytes.length !== file.size) throw new Error('文件读取大小发生变化，请重新选择。')
      const name = fileName(file.name), digest = await sha256(bytes)
      const params = {
        receiver_peer: peer(draft.receiver_peer.trim()), provider_peer: peer(draft.provider_peer.trim()), mode: draft.mode, acceptance: draft.acceptance,
        name, max_service_amount: amount(draft.mode === 'charity' ? '0' : draft.max_service_amount.trim()), max_gas_amount: amount(draft.mode === 'charity' ? '0' : draft.max_gas_amount.trim()),
      }
      const fingerprint = await sha256(new TextEncoder().encode(JSON.stringify({ ...params, sha256: digest, bytes: file.size })))
      const previous = pending(fingerprint)
      if (restartIntentId !== undefined) {
        if (previous?.intent_id !== intentId(restartIntentId)) throw new Error('再次发送须选择原订单相同的文件和条款；已有新的一次请先点“检查进度”。')
        const observed = await findIntent(restartIntentId)
        if (!observed || !canRestart(observed)) throw new Error('原来那次发送还没结束。先查看原订单，不会另付一笔。')
        if (observed.mode !== draft.mode || observed.acceptance !== draft.acceptance || observed.receiver_peer !== params.receiver_peer || observed.provider_peer !== params.provider_peer
          || observed.name !== name || observed.sha256 !== digest || observed.bytes !== file.size) throw new Error('原来那次发送的文件或条款对不上，不能创建新订单。')
        if (pending(fingerprint)?.intent_id !== previous.intent_id) throw new Error('这个文件的发送记录已在另一处更新，请先刷新并查看原订单。')
      }
      const intent = previous && restartIntentId === undefined ? previous : { intent_id: crypto.randomUUID(), fingerprint }
      current()
      try {
        storage.setItem(`${intentPrefix}.intent.${fingerprint}`, JSON.stringify(intent))
        storage.setItem(`${intentPrefix}.recent`, intent.intent_id)
      }
      catch { throw new Error('无法保存这次发送记录，尚未提交。请允许保存本地数据后重试。') }
      const order = parseOrder(await request('prepare', { ...params, intent_id: intent.intent_id, content_base64: toBase64(bytes) }))
      if (order.intent_id !== intent.intent_id || order.role !== 'sender' || order.source_peer !== status.node_peer || order.receiver_peer !== params.receiver_peer || order.provider_peer !== params.provider_peer
        || order.mode !== draft.mode || order.acceptance !== draft.acceptance || order.name !== name || order.sha256 !== digest || order.bytes !== file.size
        || BigInt(order.payment.total) > BigInt(params.max_service_amount) || BigInt(order.payment.gas_cap) > BigInt(params.max_gas_amount)) throw new Error('设备返回的订单与本次选择不一致，未确认付款。请先查看原订单。')
      return order
    },
    async confirm(order: DeliveryOrder) {
      manage()
      if (order.role !== 'sender' || order.phase !== 'prepared') throw new Error('这笔订单需要先刷新或点“检查进度”，不能重新确认付款。')
      const confirmed = parseOrder(await request('confirm', { intent_id: order.intent_id }))
      if (!same(authorization(confirmed), authorization(order))) throw new Error('确认结果与原订单的条款不一致。先点“检查进度”，不会自动再次付款。')
      return confirmed
    },
    async get(orderId: string) { return parseOrder(await request('get', { order_id: peer(orderId) })) },
    async accept(order: DeliveryOrder) { manage(); return parseOrder(await request('accept', { order_id: order.order_id })) },
    async reconcile(order: DeliveryOrder) { manage(); return parseOrder(await request('reconcile', { order_id: order.order_id })) },
    async save(order: DeliveryOrder, platform: Platform) {
      if (!platform.files) throw new Error('此客户端尚不支持保存文件。')
      if (!canRead(order)) throw new Error('这笔订单的文件尚不可取回。')
      const file = parseReceived(await request('read', { order_id: order.order_id }), order.bytes)
      const bytes = await checkedFile(file, order)
      current()
      return platform.files.save(bytes, file.name)
    },
  }
  return client
}

export function useDelivery(connection: ReadyConnection, platform: Platform) {
  const client = useMemo(() => createDeliveryClient(connection, platform.storage), [connection, platform.storage])
  const [status, setStatus] = useState<DeliveryStatus | null>(null)
  const [orders, setOrders] = useState<DeliveryOrder[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  // A failed read clears itself on the next good read; a failed action stays until the next action.
  const [readError, setReadError] = useState('')
  const [notice, setNotice] = useState('')
  const alive = useRef(false), running = useRef<string | null>(null), sequence = useRef(0)
  const reading = useRef<typeof client | null>(null)
  const current = () => alive.current && connection.isCurrent()
  const remember = (order: DeliveryOrder) => {
    if (!current()) return
    sequence.current += 1
    setOrders(previous => [order, ...previous.filter(item => item.order_id !== order.order_id)])
    setSelectedId(order.order_id)
  }
  async function refresh() {
    if (reading.current === client) return
    reading.current = client
    const owner = ++sequence.current
    try {
      const [statusResult, ordersResult] = await Promise.allSettled([client.status(), client.list()])
      if (statusResult.status === 'rejected') throw statusResult.reason
      if (ordersResult.status === 'rejected') throw ordersResult.reason
      const nextStatus = statusResult.value, nextOrders = ordersResult.value
      if (!current() || owner !== sequence.current) return
      setStatus(nextStatus); setOrders(nextOrders); setReadError('')
      const recent = client.recent()
      if (recent) setSelectedId(previous => previous ?? nextOrders.find(order => order.intent_id === recent)?.order_id ?? null)
    } catch (cause) { if (current() && owner === sequence.current) setReadError(messageOf(cause)) }
    finally { if (reading.current === client) reading.current = null }
  }
  useEffect(() => {
    alive.current = true; running.current = null
    setStatus(null); setOrders([]); setSelectedId(null); setBusy(null); setError(''); setReadError(''); setNotice('')
    void refresh()
    const foreground = platform.onForeground(() => { void refresh() })
    return () => { alive.current = false; sequence.current += 1; foreground() }
  }, [client])
  const polling = shouldPollDelivery(status?.enabled === true, orders)
  useEffect(() => {
    if (!polling) return
    const timer = setInterval(() => { if (platform.foreground() && running.current === null) void refresh() }, 3000)
    return () => clearInterval(timer)
  }, [client, polling])
  async function run(key: string, action: () => Promise<void>) {
    if (running.current !== null || !current()) return
    running.current = key; setBusy(key); setError(''); setNotice('')
    try { await action() }
    catch (cause) { if (current()) setError(messageOf(cause)) }
    finally { if (current()) { running.current = null; setBusy(null) } }
  }
  const selected = orders.find(order => order.order_id === selectedId) ?? null
  return {
    status, orders, selected, busy, error: error || readError, notice,
    canManage: connection.principal.source === 'membership' && connection.principal.can_manage,
    select: setSelectedId,
    refresh: () => run('refresh', refresh),
    configure: (settings: DeliverySettings) => run('configure', async () => { const result = await client.configure(settings); if (current()) { sequence.current += 1; setStatus(result) } }),
    prepare: (draft: DeliveryDraft, file: TransferFile, restartIntentId?: string) => run('prepare', async () => {
      if (!status) throw new Error('请先读取设备状态。')
      remember(await client.prepare(draft, file, status, restartIntentId))
    }),
    confirm: (order: DeliveryOrder) => run('confirm', async () => { remember(await client.confirm(order)) }),
    accept: (order: DeliveryOrder) => run('accept', async () => { remember(await client.accept(order)) }),
    reconcile: (order: DeliveryOrder) => run('reconcile', async () => { remember(await client.reconcile(order)) }),
    save: (order: DeliveryOrder) => run('save', async () => {
      const saved = await client.save(order, platform)
      if (current()) setNotice(saved === 'saved' ? '文件校验通过，并已保存到此应用。分享面板可导出副本。' : '文件校验通过，已交给浏览器保存。请确认下载文件。')
    }),
  }
}
