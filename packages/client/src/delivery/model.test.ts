import assert from 'node:assert/strict'
import test from 'node:test'
import type { ReadyConnection } from '../lib/node.ts'
import { createDeliveryClient } from './client.ts'
import { amount, canRead, canRestart, changeMode, checkedFile, draftFor, fromBase64, orderActions, orderSummary, orderTitle, parseOrder, parseReceived, parseStatus, settlementAmountsLabel, sha256, sizeLabel, toBase64, walletNotice } from './model.ts'
import type { DeliveryDraft, DeliveryOrder, DeliveryStatus } from './types.ts'

const source = '11'.repeat(32), receiver = '22'.repeat(32), provider = '33'.repeat(32)
const uuid = '00000000-0000-4000-8000-000000000001'
const content = new TextEncoder().encode('a file the receiver can recover')
const digest = await sha256(content)
function order(): DeliveryOrder {
  return {
    order_id: '44'.repeat(32), intent_id: uuid, role: 'sender', mode: 'charity', acceptance: 'recoverable', source_peer: source, receiver_peer: receiver, provider_peer: provider,
    name: 'example.txt', bytes: content.length, sha256: digest, phase: 'prepared', evidence: 'none',
    payment: { state: 'free', chain_id: null, asset: null, payer: null, provider_wallet: null, delivery_fee: '0', reservation_fee: '0', priority_fee: '0', total: '0', gas_cap: '0', earned: '0', withdrawn: '0', refunded: '0', tx_hash: null },
    deadlines: null, windows: [{ index: 0, bytes: content.length + 128, committed_bytes: 0, state: 'pending', decryption_proven: false }], created_at_ms: 1, updated_at_ms: 1, error: null,
  }
}
function paidOrder(): DeliveryOrder {
  const paid = order()
  paid.mode = 'paid'; paid.payment.state = 'prepared'
  paid.deadlines = { quote_expires_at: 10, hold_deadline: 20, key_deadline: 30, open_deadline: 40, retain_until: 50, priority_deadline: 25 }
  paid.payment.chain_id = '31337'; paid.payment.asset = `0x${'55'.repeat(20)}`; paid.payment.payer = `0x${'66'.repeat(20)}`; paid.payment.provider_wallet = `0x${'77'.repeat(20)}`
  paid.payment.delivery_fee = '9007199254740993'; paid.payment.total = '9007199254740993'
  return paid
}

test('amounts retain every integer digit and reject imprecise or out of range inputs', () => {
  assert.equal(amount('9007199254740993'), '9007199254740993')
  assert.equal(amount(((1n << 256n) - 1n).toString()), ((1n << 256n) - 1n).toString())
  for (const invalid of [9007199254740993, '-1', '1.5', '01', '1e6', (1n << 256n).toString()]) assert.throws(() => amount(invalid))
})

test('paid accounting remains exact above Number precision and rejects overspending or invented evidence', () => {
  const paid = paidOrder()
  assert.equal(parseOrder(paid).payment.total, '9007199254740993')
  assert.equal(parseOrder({ ...paid, phase: 'proving' }).phase, 'proving')
  assert.throws(() => parseOrder({ ...paid, payment: { ...paid.payment, total: '9007199254740992' } }))
  assert.throws(() => parseOrder({ ...paid, payment: { ...paid.payment, earned: paid.payment.total, refunded: '1' } }))
  assert.throws(() => parseOrder({ ...paid, payment: { ...paid.payment, withdrawn: '1' } }))
  assert.throws(() => parseOrder({ ...paid, deadlines: { ...paid.deadlines, priority_deadline: 10 } }))
  assert.throws(() => parseOrder({ ...paid, deadlines: { ...paid.deadlines, priority_deadline: 31 } }))
  assert.throws(() => parseOrder({ ...order(), evidence: 'decryption_proven' }))
  assert.throws(() => parseOrder({ ...order(), phase: 'success' }))
  assert.throws(() => parseOrder({ ...order(), role: 'provider' }))
})

test('uncertain chain results label retained settlement amounts as the last record rather than current receipts', () => {
  const paid = paidOrder()
  const confirmed = parseOrder({ ...paid, phase: 'paid', payment: { ...paid.payment, state: 'withdrawn', earned: paid.payment.total, withdrawn: paid.payment.total } })
  const uncertain = parseOrder({ ...confirmed, phase: 'reconciling', evidence: 'none', payment: { ...confirmed.payment, state: 'unknown' } })
  assert.equal(settlementAmountsLabel(confirmed), '已赚取 / 已提款 / 已退款')
  assert.equal(settlementAmountsLabel(uncertain), '上次记录（赚取 / 提款 / 退款，到账待确认）')
  assert.equal(uncertain.payment.withdrawn, confirmed.payment.withdrawn)
  assert.equal(settlementAmountsLabel({ ...confirmed, phase: 'failed' }), settlementAmountsLabel(uncertain))
  assert.equal(settlementAmountsLabel({ ...confirmed, payment: { ...confirmed.payment, state: 'failed' } }), settlementAmountsLabel(uncertain))
})

test('a received file is saved only after exact content, length, name and digest agree', async () => {
  const received = { name: 'example.txt', bytes: content.length, content_base64: toBase64(content), sha256: digest }
  assert.deepEqual(await checkedFile(parseReceived(received, 131072), order()), content)
  await assert.rejects(checkedFile({ ...received, content_base64: toBase64(new Uint8Array(content.length)) }, order()))
  await assert.rejects(checkedFile({ ...received, name: 'other.txt' }, order()))
  await assert.rejects(checkedFile({ ...received, bytes: content.length - 1 }, order()))
  assert.throws(() => parseReceived({ ...received, name: '../file' }, 131072))
  assert.throws(() => parseReceived({ ...received, content_base64: undefined }, 131072))
  assert.throws(() => fromBase64('Zh=='))
})

test('file retrieval permits charity senders and accepted receivers but never paid senders or providers', () => {
  const charityReceived: DeliveryOrder = { ...order(), role: 'receiver', phase: 'recoverable', evidence: 'charity_receipt', windows: order().windows.map(window => ({ ...window, state: 'held', committed_bytes: window.bytes + 68 })) }
  const delivered: DeliveryOrder = { ...paidOrder(), phase: 'paid', evidence: 'recoverable', payment: { ...paidOrder().payment, state: 'earned', earned: paidOrder().payment.total }, windows: paidOrder().windows.map(window => ({ ...window, state: 'earned', committed_bytes: window.bytes + 68 })) }
  assert.equal(canRead(order()), true)
  assert.equal(canRead(charityReceived), true)
  assert.equal(canRead(delivered), false)
  assert.equal(canRead({ ...delivered, role: 'receiver', phase: 'recoverable' }), true)
  assert.equal(canRead({ ...delivered, role: 'receiver', phase: 'proving' }), true)
  assert.equal(canRead({ ...paidOrder(), role: 'receiver', evidence: 'none' }), false)
  assert.equal(canRead({ ...delivered, role: 'provider', name: null, sha256: null }), false)
})

const status: DeliveryStatus = { enabled: true, charity_enabled: true, paid_enabled: false, node_peer: source, wallet: { state: 'absent', address: null }, max_file_bytes: 130816, receiver_key: null }
const draft: DeliveryDraft = { receiver_peer: receiver, provider_peer: provider, mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0' }
const file = { name: 'example.txt', size: content.length, arrayBuffer: async () => new Uint8Array(content).buffer }
function connection(request: ReadyConnection['request']): ReadyConnection {
  return { generation: 1, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: source }, principal: { source: 'membership', can_manage: true, network_id: 'network', device_id: 'client', epoch: 1 }, info: { device_id: source, name: 'own Node', version: 'test', initialized: true, network_id: 'network' }, request, rpc: async () => { throw new Error('delivery must not use private RPC') }, subscribe: () => () => {}, isCurrent: () => true }
}

test('a lost prepare response reuses its persisted intent and confirm submits only that original id', async () => {
  const records = new Map<string, string>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  const transport = connection(async (method, params = {}) => {
    assert.equal(method, 'delivery'); calls.push(params)
    if (calls.length === 1) throw new Error('response lost after Node persistence')
    return { ...order(), intent_id: params.intent_id }
  })
  await assert.rejects(createDeliveryClient(transport, storage).prepare(draft, file, status))
  const client = createDeliveryClient(transport, storage)
  const prepared = await client.prepare(draft, file, status)
  assert.equal(calls[0].intent_id, calls[1].intent_id)
  await client.confirm(prepared)
  assert.deepEqual(calls[2], { op: 'confirm', intent_id: prepared.intent_id })
})

test('a local session cannot configure or confirm and storage failure prevents prepare submission', async () => {
  let calls = 0
  const transport = connection(async () => { calls += 1; return order() })
  transport.principal = { ...transport.principal, source: 'local_session', can_manage: false }
  const storage = { getItem: () => null, setItem: () => { throw new Error('disk unavailable') } }
  const local = createDeliveryClient(transport, storage)
  await assert.rejects(local.configure({ enabled: true, charity_enabled: true, paid_enabled: false }))
  await assert.rejects(local.confirm(order()))
  transport.principal = { ...transport.principal, source: 'membership', can_manage: true }
  await assert.rejects(createDeliveryClient(transport, storage).prepare(draft, file, status))
  assert.equal(calls, 0)
})

test('charity resets strict mode and rejects every financial or strong proof field at the shared boundary', async () => {
  assert.equal(changeMode({ ...draft, mode: 'paid', acceptance: 'strict' }, 'charity').acceptance, 'recoverable')
  let calls = 0
  const client = createDeliveryClient(connection(async () => { calls += 1; return order() }), { getItem: () => null, setItem: () => { throw new Error('must not store a strict charity intent') } })
  await assert.rejects(client.prepare({ ...draft, acceptance: 'strict' }, file, status), /公益转发不能选/)
  assert.equal(calls, 0)
  assert.throws(() => parseOrder({ ...order(), acceptance: 'strict' }))
  for (const field of ['chain_id', 'asset', 'payer', 'provider_wallet', 'tx_hash']) assert.throws(() => parseOrder({ ...order(), payment: { ...order().payment, [field]: field === 'chain_id' ? '31337' : field === 'tx_hash' ? `0x${'88'.repeat(32)}` : `0x${'88'.repeat(20)}` } }))
  for (const field of ['delivery_fee', 'reservation_fee', 'priority_fee', 'total', 'gas_cap', 'earned', 'withdrawn', 'refunded']) assert.throws(() => parseOrder({ ...order(), payment: { ...order().payment, [field]: '1' } }))
  assert.throws(() => parseOrder({ ...order(), windows: order().windows.map(window => ({ ...window, decryption_proven: true })) }))
})

test('preparing B cannot erase the unknown A intent and returning to A after restart reuses its id', async () => {
  const records = new Map<string, string>(), stored = new Map<string, DeliveryOrder>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  const paidDraft: DeliveryDraft = { ...draft, mode: 'paid', max_service_amount: '9007199254740993' }
  const paidStatus: DeliveryStatus = { ...status, paid_enabled: true, wallet: { state: 'unlocked', address: `0x${'66'.repeat(20)}` } }
  const transport = connection(async (_method, params = {}) => {
    calls.push(params)
    const id = typeof params.intent_id === 'string' ? params.intent_id : ''
    if (params.op === 'confirm') {
      const previous = stored.get(id)
      assert.ok(previous)
      stored.set(id, { ...previous, phase: 'reconciling', payment: { ...previous.payment, state: 'unknown' } })
      throw new Error('A signed transaction persisted, broadcast result unknown')
    }
    const saved = stored.get(id) ?? { ...paidOrder(), name: typeof params.name === 'string' ? params.name : '', intent_id: id, order_id: await sha256(new TextEncoder().encode(id)) }
    stored.set(id, saved)
    return saved
  })
  const firstClient = createDeliveryClient(transport, storage)
  const first = await firstClient.prepare(paidDraft, file, paidStatus)
  await assert.rejects(firstClient.confirm(first), /broadcast result unknown/)
  await createDeliveryClient(transport, storage).prepare(paidDraft, { ...file, name: 'B.txt' }, paidStatus)
  const resumed = await createDeliveryClient(transport, storage).prepare(paidDraft, file, paidStatus)
  const preparations = calls.filter(call => call.op === 'prepare')
  assert.equal(preparations[0].intent_id, preparations[2].intent_id)
  assert.notEqual(preparations[0].intent_id, preparations[1].intent_id)
  assert.equal(resumed.payment.state, 'unknown')
  const before = calls.length
  await assert.rejects(firstClient.confirm(resumed), /需要先刷新或点“检查进度”/)
  assert.equal(calls.length, before, 'an unknown transaction cannot be confirmed as a new payment')
  assert.equal([...records.keys()].filter(key => key.includes('.intent.')).length, 2)
})

test('same-file retries keep a completed intent while an explicit new send checks Node terminal state and survives a lost response', async () => {
  const records = new Map<string, string>(), stored = new Map<string, DeliveryOrder>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  let lookupMissing = false, losePrepare = false
  const transport = connection(async (_method, params = {}) => {
    calls.push(params)
    const id = typeof params.intent_id === 'string' ? params.intent_id : ''
    if (params.op === 'find_intent') return { order: lookupMissing ? null : stored.get(id) ?? null }
    const saved = stored.get(id) ?? { ...order(), intent_id: id, order_id: await sha256(new TextEncoder().encode(id)) }
    stored.set(id, saved)
    if (losePrepare) { losePrepare = false; throw new Error('new prepare response lost') }
    return saved
  })
  const client = createDeliveryClient(transport, storage)
  const first = await client.prepare(draft, file, status)
  await assert.rejects(client.prepare(draft, file, status, first.intent_id), /还没结束/)
  const complete: DeliveryOrder = { ...first, phase: 'recoverable', evidence: 'charity_receipt', windows: first.windows.map(window => ({ ...window, state: 'held' })) }
  stored.set(first.intent_id, complete)
  assert.equal((await client.prepare(draft, file, status)).intent_id, first.intent_id, 'ordinary retry never means a new send')
  lookupMissing = true
  await assert.rejects(client.prepare(draft, file, status, first.intent_id), /还没结束/)
  lookupMissing = false; losePrepare = true
  await assert.rejects(client.prepare(draft, file, status, first.intent_id), /response lost/)
  const attempted = calls.at(-1)?.intent_id
  assert.notEqual(attempted, first.intent_id)
  const before = calls.length
  await assert.rejects(client.prepare(draft, file, status, first.intent_id), /已有新的一次/)
  assert.equal(calls.length, before, 'another click cannot replace the new unknown intent')
  const resumed = await createDeliveryClient(transport, storage).prepare(draft, file, status)
  assert.equal(resumed.intent_id, attempted)
  await client.confirm(resumed)
  assert.deepEqual(calls.at(-1), { op: 'confirm', intent_id: resumed.intent_id })
})

test('only completed delivery or a full refund permits an explicit new paid order', () => {
  const paid = paidOrder()
  paid.phase = 'paid'; paid.payment.state = 'earned'; paid.payment.earned = paid.payment.total
  paid.windows = paid.windows.map(window => ({ ...window, state: 'earned' }))
  assert.equal(canRestart(parseOrder(paid)), true)
  assert.equal(canRestart({ ...paid, payment: { ...paid.payment, state: 'unknown' } }), false)
  assert.equal(canRestart({ ...paid, payment: { ...paid.payment, state: 'included' } }), false)
  for (const phase of ['failed', 'prepared', 'partial', 'reconciling'] satisfies DeliveryOrder['phase'][]) assert.equal(canRestart({ ...paid, phase }), false)
  const refunded: DeliveryOrder = { ...paidOrder(), phase: 'refunded', payment: { ...paidOrder().payment, state: 'refunded', refunded: paid.payment.total } }
  assert.equal(canRestart(parseOrder(refunded)), true)
  assert.equal(canRestart({ ...refunded, payment: { ...refunded.payment, refunded: '1' } }), false)
})

test('an unavailable wallet retains its distinct failure state and asks for its own backup', () => {
  const unavailable = parseStatus({ ...status, wallet: { state: 'unavailable', address: null, reason: 'The encrypted keystore is unavailable.' } })
  assert.equal(unavailable.wallet.state, 'unavailable')
  assert.match(walletNotice(unavailable.wallet), /独立钱包备份/)
  assert.throws(() => parseStatus({ ...status, wallet: { state: 'unavailable', address: `0x${'66'.repeat(20)}`, reason: 'bad' } }))
})

test('sizes read as bytes, KB or MB and a limit never rounds up past what fits', () => {
  assert.equal(sizeLabel(512), '512 字节')
  assert.equal(sizeLabel(48 * 1024), '48 KB')
  assert.equal(sizeLabel(1536), '1.5 KB')
  assert.equal(sizeLabel(130_816, true), '127 KB')
  assert.equal(sizeLabel(1024 * 1024 - 1, true), '1023 KB')
  assert.equal(sizeLabel(5 * 1024 * 1024), '5.0 MB')
})

test('a record reads as file name and one plain line, and offers only what this side can do', () => {
  const sent = { ...order(), name: 'a.txt', bytes: 48 * 1024, phase: 'recoverable', evidence: 'charity_receipt' } as DeliveryOrder
  assert.equal(orderTitle(sent), 'a.txt')
  assert.equal(orderSummary(sent), '免费 · 48 KB · 已送达')
  assert.deepEqual(orderActions({ ...sent, phase: 'prepared', evidence: 'none' }), { confirm: true, accept: false, save: false }, 'nothing is saved before the send is confirmed')
  const received = { ...sent, role: 'receiver', name: null, evidence: 'none', phase: 'transferring' } as DeliveryOrder
  assert.equal(orderTitle(received), '待接收的文件')
  assert.deepEqual(orderActions(received), { confirm: false, accept: true, save: false })
  assert.equal(canRestart({ ...received, phase: 'recoverable', evidence: 'charity_receipt' }), false, 'a receiver never sends again')
})

test('without 实验功能 a send is free whatever mode the draft last held', () => {
  const paid: DeliveryDraft = { receiver_peer: receiver, provider_peer: provider, mode: 'paid', acceptance: 'strict', max_service_amount: '5', max_gas_amount: '7' }
  assert.deepEqual(draftFor(paid, false), { ...paid, mode: 'charity', acceptance: 'recoverable' })
  assert.equal(draftFor(paid, true), paid)
})
