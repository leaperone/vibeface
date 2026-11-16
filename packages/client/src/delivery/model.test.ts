import assert from 'node:assert/strict'
import { registerHooks } from 'node:module'
import test from 'node:test'
import type { ReadyConnection } from '../lib/node.ts'
import type { Platform } from '../lib/platform.ts'
import { bytesToBase64, sha256Hex } from '../lib/byte-codec.ts'
import { createDeliveryClient } from './client.ts'
import { amount, canRead, canRestart, checkedFile, draftFor, fromBase64, orderActions, orderSummary, orderTitle, parseBudgets, parseOrder, parseReceived, parseStatus, publicationLabel, providerChoices, receiverChoices, receivingBatches, settlementAmountsLabel, sizeLabel, walletNotice } from './model.ts'
import type { DeliveryDraft, DeliveryOrder, DeliveryStatus, ReserveDeliveryIntent } from './types.ts'

const reserveDeliveryIntent: ReserveDeliveryIntent = async (_key, reserve) => reserve()

const source = '11'.repeat(32), receiver = '22'.repeat(32), provider = '33'.repeat(32)
const uuid = '00000000-0000-4000-8000-000000000001'
const content = new TextEncoder().encode('a file the receiver can recover')
const digest = await sha256Hex(content)
function order(): DeliveryOrder {
  return {
    order_id: '44'.repeat(32), intent_id: uuid, role: 'sender', mode: 'charity', acceptance: 'recoverable', source_peer: source, receiver_peer: receiver, provider_peer: provider,
    name: 'example.txt', bytes: content.length, sha256: digest, phase: 'prepared', evidence: 'none',
    payment: { state: 'free', chain_id: null, asset: null, payer: null, provider_wallet: null, delivery_fee: '0', reservation_fee: '0', priority_fee: '0', total: '0', gas_cap: '0', earned: '0', withdrawn: '0', refunded: '0', tx_hash: null, budget_id: null, settlement: null },
    deadlines: null, windows: [{ index: 0, bytes: content.length + 128, committed_bytes: 0, state: 'pending', decryption_proven: false }], created_at_ms: 1, updated_at_ms: 1, error: null,
  }
}
function paidOrder(): DeliveryOrder {
  const paid = order()
  paid.mode = 'paid'; paid.payment.state = 'prepared'; paid.payment.budget_id = '88'.repeat(32)
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
  const confirmed = parseOrder({ ...paid, phase: 'paid', payment: { ...paid.payment, state: 'withdrawn', earned: paid.payment.total, withdrawn: paid.payment.total,
    settlement: { earned: paid.payment.total, withdrawn: paid.payment.total, refunded: '0', publication: { state: 'final_earned', total_amount: paid.payment.total, economic_cutoff: 30, availability_risk: null } } } })
  const uncertain = parseOrder({ ...confirmed, phase: 'reconciling', evidence: 'none', payment: { ...confirmed.payment, state: 'unknown' } })
  assert.equal(settlementAmountsLabel(confirmed, 'order'), '本单已赚取 / 已退款')
  assert.equal(settlementAmountsLabel(confirmed, 'budget'), '预算累计赚取 / 提款 / 退款')
  assert.equal(settlementAmountsLabel(uncertain, 'order'), '本单上次记录（赚取 / 退款，当前待确认）')
  assert.equal(settlementAmountsLabel(uncertain, 'budget'), '预算上次记录（赚取 / 提款 / 退款，到账待确认）')
  assert.equal(uncertain.payment.withdrawn, confirmed.payment.withdrawn)
  assert.equal(settlementAmountsLabel({ ...confirmed, phase: 'failed' }, 'budget'), settlementAmountsLabel(uncertain, 'budget'))
  assert.equal(settlementAmountsLabel({ ...confirmed, payment: { ...confirmed.payment, state: 'failed' } }, 'order'), settlementAmountsLabel(uncertain, 'order'))
  assert.equal(publicationLabel(confirmed), '本批约定费用已赚取')
  assert.equal(publicationLabel(uncertain), '批次状态待确认，请检查原交易')
  assert.equal(publicationLabel({ ...confirmed, phase: 'failed' }), publicationLabel(uncertain))
  assert.equal(publicationLabel({ ...confirmed, payment: { ...confirmed.payment, settlement: null } }), '本单费用已赚取')
})

test('a received file is saved only after exact content, length, name and digest agree', async () => {
  const received = { name: 'example.txt', bytes: content.length, content_base64: bytesToBase64(content), sha256: digest }
  assert.deepEqual(await checkedFile(parseReceived(received, 131072), order()), content)
  await assert.rejects(checkedFile({ ...received, content_base64: bytesToBase64(new Uint8Array(content.length)) }, order()))
  await assert.rejects(checkedFile({ ...received, name: 'other.txt' }, order()))
  await assert.rejects(checkedFile({ ...received, bytes: content.length - 1 }, order()))
  assert.throws(() => parseReceived({ ...received, name: '../file' }, 131072))
  assert.throws(() => parseReceived({ ...received, content_base64: undefined }, 131072))
  assert.throws(() => fromBase64('Zh=='))
})

test('settled publication preserves partial earnings and refunds without claiming recovery material stayed private', () => {
  for (const earned of ['0', '3']) {
    const refunded = earned === '0' ? '10' : '7'
    const settled = parseOrder({ ...paidOrder(), phase: 'refunded', payment: { ...paidOrder().payment,
      state: 'refunded', delivery_fee: '10', total: '10', earned, refunded,
      settlement: { earned, withdrawn: '0', refunded, publication: { state: 'final_settled', total_amount: '10', economic_cutoff: 30, availability_risk: null } } } })
    assert.equal(settled.payment.earned, earned)
    assert.equal(settled.payment.refunded, refunded)
    assert.equal(publicationLabel(settled), '本批款项已赚取或退款，恢复材料可能已公开')
    assert.throws(() => parseOrder({ ...settled, payment: { ...settled.payment, settlement: { ...settled.payment.settlement,
      publication: { state: 'invented_terminal', total_amount: '10', economic_cutoff: 30, availability_risk: null } } } }))
  }
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
const draft: DeliveryDraft = { receiver_peer: receiver, provider_peer: provider, mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0', budget_id: null }
const file = { name: 'example.txt', size: content.length, arrayBuffer: async () => new Uint8Array(content).buffer }
function connection(request: ReadyConnection['request']): ReadyConnection {
  return { generation: 1, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: source }, principal: { source: 'membership', can_manage: true, network_id: 'network', device_id: 'client', epoch: 1 }, info: { device_id: source, name: 'own Node', version: 'test', initialized: true, network_id: 'network' }, request, rpc: async () => { throw new Error('delivery must not use private RPC') }, subscribe: () => () => {}, isCurrent: () => true }
}

test('delivery refresh keeps read-only history and file access without requesting manager budgets', async () => {
  const identity = { network_id: 'network', device_id: 'client', epoch: 1 }
  const principals: ReadyConnection['principal'][] = [
    { ...identity, source: 'membership', can_manage: false },
    { ...identity, source: 'local_session', can_manage: false },
    { ...identity, source: 'membership', can_manage: true },
  ]
  const budget = { budget_id: '88'.repeat(32), payer: `0x${'66'.repeat(20)}`, payee: `0x${'77'.repeat(20)}`, provider_peer: provider,
    asset: `0x${'55'.repeat(20)}`, deposit: '10', free: '10', locked: '0', earned: '0', withdrawn: '0', refunded: '0', state: 'open', gas_cap: '0', ready_windows: [] }
  const react = `
    export const effects = [], updates = [];
    export const useMemo = create => create();
    export const useRef = current => ({ current });
    export const useState = initial => [initial, value => updates.push(value)];
    export const useEffect = effect => effects.push(effect);
  `
  for (const principal of principals) for (const history of [false, true]) {
    const managing = principal.source === 'membership' && principal.can_manage
    const key = `${principal.source}-${managing}-${history}`
    const subject = new URL('./client.ts', import.meta.url); subject.searchParams.set('refresh-permissions', key)
    const reactUrl = `data:text/javascript,${encodeURIComponent(react)}#${key}`
    const hooks: { effects: (() => void | (() => void))[]; updates: unknown[] } = await import(reactUrl)
    const loader = registerHooks({ resolve(specifier, context, nextResolve) {
      return specifier === 'react' && context.parentURL === subject.href ? { url: reactUrl, shortCircuit: true } : nextResolve(specifier, context)
    } })
    let loaded: typeof import('./client.ts')
    try { loaded = await import(subject.href) }
    finally { loader.deregister() }
    const calls: Record<string, unknown>[] = []
    const readable = { ...order(), order_id: 'aa'.repeat(32) }
    const nextStatus = { ...status, paid_enabled: !history }, nextOrders = history ? [paidOrder(), readable] : [readable]
    const transport = connection(async (method, params = {}) => {
      assert.equal(method, 'delivery'); calls.push(params)
      switch (params.op) {
        case 'status': return nextStatus
        case 'list': return { orders: nextOrders }
        case 'providers': return { providers: [] }
        case 'budgets': if (!managing) throw new Error('budget permission denied'); return { budgets: [budget] }
        case 'get': assert.equal(params.order_id, readable.order_id); return readable
        case 'read': assert.equal(params.order_id, readable.order_id); return { name: readable.name, bytes: content.length, content_base64: bytesToBase64(content), sha256: digest }
        default: throw new Error('unexpected delivery request')
      }
    })
    transport.principal = principal
    const platform: Platform = {
      reserveDeliveryIntent,
      storage: { getItem: () => null, setItem: () => { throw new Error('a read must not write storage') } },
      onForeground: () => () => {}, foreground: () => false, self: 'test', saveRecovery: () => { throw new Error('unexpected recovery') },
      get secure(): never { throw new Error('unexpected secure API') }, get transport(): never { throw new Error('unexpected transport') },
      get outbox(): never { throw new Error('unexpected outbox') }, get notifier(): never { throw new Error('unexpected notifier') },
      files: { pick: async () => null, save: async (bytes, name) => { assert.deepEqual(bytes, content); assert.equal(name, readable.name); return 'saved' } },
    }
    assert.equal(loaded.useDelivery(transport, platform).canManage, managing)
    const cleanups = hooks.effects.map(effect => effect())
    try {
      await new Promise(resolve => setImmediate(resolve))
      assert.deepEqual(calls, [{ op: 'status' }, { op: 'list' }, { op: 'providers' }, ...(managing ? [{ op: 'budgets' }] : [])])
      assert.deepEqual(hooks.updates.slice(-4), [nextStatus, nextOrders, managing ? [budget] : [], ''])
      const client = createDeliveryClient(transport, platform.storage, reserveDeliveryIntent)
      assert.deepEqual(await client.get(readable.order_id), readable)
      assert.equal(await client.save(readable, platform), 'saved')
      assert.deepEqual(calls.slice(-2), [{ op: 'get', order_id: readable.order_id }, { op: 'read', order_id: readable.order_id }])
    } finally { for (const cleanup of cleanups) cleanup?.() }
  }
})

test('a lost prepare response reuses its persisted intent and confirm submits only that original id', async () => {
  const records = new Map<string, string>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  const transport = connection(async (method, params = {}) => {
    assert.equal(method, 'delivery'); calls.push(params)
    if (calls.length === 1) throw new Error('response lost after Node persistence')
    return { ...order(), intent_id: params.intent_id }
  })
  await assert.rejects(createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(draft, file, status))
  const client = createDeliveryClient(transport, storage, reserveDeliveryIntent)
  const prepared = await client.prepare(draft, file, status)
  assert.equal(calls[0].intent_id, calls[1].intent_id)
  await client.confirm(prepared)
  assert.deepEqual(calls[2], { op: 'confirm', intent_id: prepared.intent_id })
})

test('recoverable preparation preserves exact wire bytes, saved fingerprints and confirmation ids in both modes', async () => {
  for (const mode of ['charity', 'paid'] satisfies DeliveryDraft['mode'][]) {
    const paid = mode === 'paid'
    const fingerprint = paid ? '0381cb0459e5a0dce80dc0a232bee292930d47ab172b0ff8dee6745d63ea9b6f' : 'e4d2ac185fb4490bd606a5ba49f715bed1df4b5a85ffd888c0ac06f127f34eec'
    const key = `vibeface.delivery.network.client.${source}.intent.${fingerprint}`
    const saved = JSON.stringify({ intent_id: uuid, fingerprint })
    const writes: [string, string][] = [], calls: Record<string, unknown>[] = []
    const storage = {
      getItem: (requested: string) => { assert.equal(requested, key); return saved },
      setItem: (requested: string, value: string) => { writes.push([requested, value]) },
    }
    const expected = {
      op: 'prepare', receiver_peer: receiver, provider_peer: provider, mode, acceptance: 'recoverable',
      name: 'example.txt', max_service_amount: paid ? '9007199254740993' : '0', max_gas_amount: '0', budget_id: paid ? '88'.repeat(32) : null,
      intent_id: uuid, content_base64: 'YSBmaWxlIHRoZSByZWNlaXZlciBjYW4gcmVjb3Zlcg==',
    }
    const client = createDeliveryClient(connection(async (_method, params = {}) => { calls.push(params); return paid ? paidOrder() : order() }), storage, reserveDeliveryIntent)
    const prepared = await client.prepare({ ...draft, mode, max_service_amount: expected.max_service_amount, budget_id: expected.budget_id }, file, { ...status, paid_enabled: true })
    assert.equal(JSON.stringify(calls[0]), JSON.stringify(expected))
    assert.deepEqual(writes, [[key, saved], [`vibeface.delivery.network.client.${source}.recent`, uuid]])
    await client.confirm(prepared)
    assert.deepEqual(calls[1], { op: 'confirm', intent_id: uuid })
  }
})

test('untyped strict drafts are rejected before reading the file, persisting an intent or sending a request', async () => {
  const forbidden = () => { throw new Error('unsupported acceptance must stop before side effects') }
  const client = createDeliveryClient(connection(async () => forbidden()), { getItem: forbidden, setItem: forbidden }, reserveDeliveryIntent)
  for (const mode of ['charity', 'paid']) {
    const externalDraft: unknown = { ...draft, mode, acceptance: 'strict', budget_id: '88'.repeat(32) }
    await assert.rejects(async () => {
      await Reflect.apply(client.prepare, client, [externalDraft, { ...file, arrayBuffer: forbidden }, { ...status, paid_enabled: true }])
    }, mode === 'charity' ? /免费传输不能选/ : /Strict/)
    const projected: unknown = Reflect.apply(draftFor, undefined, [externalDraft, false])
    await assert.rejects(async () => {
      await Reflect.apply(client.prepare, client, [projected, { ...file, arrayBuffer: forbidden }, status])
    }, /免费传输不能选/)
  }
})

test('strict prepare and confirm wire responses remain rejected in both delivery modes', async () => {
  for (const mode of ['charity', 'paid'] satisfies DeliveryDraft['mode'][]) {
    const original = mode === 'paid' ? paidOrder() : order()
    const client = createDeliveryClient(connection(async (_method, params = {}) => ({ ...original, intent_id: params.intent_id, acceptance: 'strict' })), { getItem: () => null, setItem: () => {} }, reserveDeliveryIntent)
    await assert.rejects(client.prepare({ ...draft, mode, max_service_amount: original.payment.total, budget_id: original.payment.budget_id }, file, { ...status, paid_enabled: true }), /无法识别/)
    await assert.rejects(client.confirm(original), /无法识别/)
  }
})

test('a local session cannot configure or confirm and storage failure prevents prepare submission', async () => {
  let calls = 0
  const transport = connection(async () => { calls += 1; return order() })
  transport.principal = { ...transport.principal, source: 'local_session', can_manage: false }
  const storage = { getItem: () => null, setItem: () => { throw new Error('disk unavailable') } }
  const local = createDeliveryClient(transport, storage, reserveDeliveryIntent)
  await assert.rejects(local.configure({ enabled: true, charity_enabled: true, paid_enabled: false }))
  await assert.rejects(local.confirm(order()))
  transport.principal = { ...transport.principal, source: 'membership', can_manage: true }
  await assert.rejects(createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(draft, file, status))
  assert.equal(calls, 0)
})

test('charity rejects strict acceptance and every financial or strong proof field at the shared wire boundary', () => {
  assert.throws(() => parseOrder({ ...order(), acceptance: 'strict' }))
  for (const field of ['chain_id', 'asset', 'payer', 'provider_wallet', 'tx_hash']) assert.throws(() => parseOrder({ ...order(), payment: { ...order().payment, [field]: field === 'chain_id' ? '31337' : field === 'tx_hash' ? `0x${'88'.repeat(32)}` : `0x${'88'.repeat(20)}` } }))
  for (const field of ['delivery_fee', 'reservation_fee', 'priority_fee', 'total', 'gas_cap', 'earned', 'withdrawn', 'refunded']) assert.throws(() => parseOrder({ ...order(), payment: { ...order().payment, [field]: '1' } }))
  assert.throws(() => parseOrder({ ...order(), windows: order().windows.map(window => ({ ...window, decryption_proven: true })) }))
})

test('preparing B cannot erase the unknown A intent and returning to A after restart reuses its id', async () => {
  const records = new Map<string, string>(), stored = new Map<string, DeliveryOrder>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  const paidDraft: DeliveryDraft = { ...draft, mode: 'paid', max_service_amount: '9007199254740993', budget_id: '88'.repeat(32) }
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
    const saved = stored.get(id) ?? { ...paidOrder(), name: typeof params.name === 'string' ? params.name : '', intent_id: id, order_id: await sha256Hex(new TextEncoder().encode(id)) }
    stored.set(id, saved)
    return saved
  })
  const firstClient = createDeliveryClient(transport, storage, reserveDeliveryIntent)
  const first = await firstClient.prepare(paidDraft, file, paidStatus)
  await assert.rejects(firstClient.confirm(first), /broadcast result unknown/)
  await createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(paidDraft, { ...file, name: 'B.txt' }, paidStatus)
  const resumed = await createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(paidDraft, file, paidStatus)
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
    const saved = stored.get(id) ?? { ...order(), intent_id: id, order_id: await sha256Hex(new TextEncoder().encode(id)) }
    stored.set(id, saved)
    if (losePrepare) { losePrepare = false; throw new Error('new prepare response lost') }
    return saved
  })
  const client = createDeliveryClient(transport, storage, reserveDeliveryIntent)
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
  const resumed = await createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(draft, file, status)
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
  for (const state of ['submitted', 'included']) assert.throws(() => parseOrder({ ...paid, payment: { ...paid.payment, state } }), /\u72b6\u6001/)
  assert.throws(() => parseOrder({ ...paid, phase: 'refundable' }), /\u72b6\u6001/)
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
  const paid: DeliveryDraft = { receiver_peer: receiver, provider_peer: provider, mode: 'paid', acceptance: 'recoverable', max_service_amount: '5', max_gas_amount: '7', budget_id: '88'.repeat(32) }
  assert.deepEqual(draftFor(paid, false), { ...paid, mode: 'charity' })
  assert.equal(draftFor(paid, true), paid)
})

test('a file goes to my other computers without pasting, never to this one or a browser', () => {
  const members = [
    { device_id: source, kind: 'node' as const, name: '我的 Mac', manage: true },
    { device_id: receiver, kind: 'node' as const, name: '书房电脑', manage: false },
    { device_id: provider, kind: 'client' as const, name: 'Mac 上的 Chrome', manage: true },
  ]
  assert.deepEqual(receiverChoices(members, source).map(item => item.device_id), [receiver])
})

test('protected budgets enforce exact conservation and fixed roles at the shared boundary', () => {
  const budget = { budget_id: '88'.repeat(32), payer: `0x${'66'.repeat(20)}`, payee: `0x${'77'.repeat(20)}`, provider_peer: provider,
    asset: `0x${'55'.repeat(20)}`, deposit: '100', free: '66', locked: '0', earned: '34', withdrawn: '17', refunded: '0', state: 'open', gas_cap: '9007199254740993', ready_windows: [] }
  assert.equal(parseBudgets({ budgets: [budget] })[0].gas_cap, '9007199254740993')
  assert.throws(() => parseBudgets({ budgets: [{ ...budget, free: '67' }] }))
  assert.throws(() => parseBudgets({ budgets: [{ ...budget, withdrawn: '35' }] }))
  assert.throws(() => parseBudgets({ budgets: [{ ...budget, payee: budget.payer }] }))
  assert.throws(() => parseBudgets({ budgets: [budget, budget] }))
  assert.throws(() => parseOrder({ ...paidOrder(), acceptance: 'strict' }))
})

test('protected preparation rejects missing budgets before any request; a two-order confirmation preserves both exact authorizations', async () => {
  let requests = 0
  const a = paidOrder(), b = { ...paidOrder(), order_id: '99'.repeat(32), intent_id: '00000000-0000-4000-8000-000000000002' }
  const client = createDeliveryClient(connection(async (_method, params = {}) => { requests++; assert.equal(params.op, 'confirm_batch'); return { orders: [a, b].map(order => ({ ...order, phase: 'funding_pending' })) } }),
    { getItem: () => null, setItem: () => {} }, reserveDeliveryIntent)
  const paidStatus: DeliveryStatus = { ...status, paid_enabled: true }
  await assert.rejects(client.prepare({ ...draft, mode: 'paid' }, file, paidStatus), /预付预算/)
  assert.equal(requests, 0)
  const confirmed = await client.confirmBatch([a, b])
  assert.equal(confirmed.length, 2); assert.equal(confirmed[0].payment.budget_id, a.payment.budget_id); assert.equal(requests, 1)
  await assert.rejects(client.confirmBatch([a, { ...b, payment: { ...b.payment, budget_id: 'aa'.repeat(32) } }]), /同一预算/)
  await assert.rejects(client.confirmBatch([a, a]), /同一预算/)
  assert.equal(requests, 1)
})

test('the receiver authorizes both original orders together and groups only two live windows from one budget', async () => {
  const a: DeliveryOrder = { ...paidOrder(), role: 'receiver', phase: 'funded' }
  const b: DeliveryOrder = { ...a, order_id: '99'.repeat(32), intent_id: '00000000-0000-4000-8000-000000000002' }
  let requests = 0
  const client = createDeliveryClient(connection(async (_method, params = {}) => {
    requests++
    assert.equal(params.op, 'accept_batch')
    assert.deepEqual(params.order_ids, [a.order_id, b.order_id])
    return { orders: [a, b] }
  }), { getItem: () => null, setItem: () => {} }, reserveDeliveryIntent)
  assert.deepEqual(receivingBatches([a, b, order()]), [[a.payment.budget_id, [a, b]]])
  assert.deepEqual(receivingBatches([a, { ...b, payment: { ...b.payment, budget_id: 'aa'.repeat(32) } }]), [])
  assert.deepEqual(receivingBatches([a, { ...b, phase: 'failed' }]), [])
  assert.deepEqual(await client.acceptBatch([a, b]), [a, b])
  await assert.rejects(client.acceptBatch([a, a]), /同时接收/)
  await assert.rejects(client.acceptBatch([a, { ...b, phase: 'failed' }]), /同时接收/)
  assert.equal(requests, 1)
})

test('the provider field offers announced devices: paid delivery those that quote a price, free delivery all', async () => {
  const offer = { settlement: { chain_id: 31337, escrow: `0x${'66'.repeat(20)}`, asset: `0x${'55'.repeat(20)}` }, price: '9007199254740993' }
  const asked: unknown[] = []
  const client = createDeliveryClient(connection(async (method, params) => {
    asked.push([method, params])
    return { providers: [
      { device_id: provider, paid: true, offer, last_seen_ms: 5 },
      { device_id: receiver, paid: false, offer: null, last_seen_ms: 3 },
      { device_id: 'not-a-device', paid: false, offer: null, last_seen_ms: 1 },
      { device_id: source, paid: true, offer: { ...offer, price: '1.5' }, last_seen_ms: 2 },
    ] }
  }), { getItem: () => null, setItem: () => {} }, reserveDeliveryIntent)
  const providers = await client.providers()
  assert.deepEqual(asked, [['delivery', { op: 'providers' }]])
  assert.deepEqual(providers, [{ device_id: provider, paid: true, offer, last_seen_ms: 5 }, { device_id: receiver, paid: false, offer: null, last_seen_ms: 3 }], 'a malformed record drops only itself')
  assert.deepEqual(providerChoices(providers, 'paid').map(item => item.device_id), [provider])
  assert.deepEqual(providerChoices(providers, 'charity').map(item => item.device_id), [provider, receiver])
})

test('reservation storage failures stop prepare and a failed recent write retains the next retry id', async () => {
  const calls: Record<string, unknown>[] = [], records = new Map<string, string>()
  const transport = connection(async (_method, params = {}) => { calls.push(params); return { ...order(), intent_id: params.intent_id } })
  for (const bad of [() => { throw new Error('read inaccessible') }, () => '{', () => JSON.stringify({ intent_id: uuid, fingerprint: 'wrong' }), () => JSON.stringify({ intent_id: 'bad', fingerprint: 'e4d2ac185fb4490bd606a5ba49f715bed1df4b5a85ffd888c0ac06f127f34eec' })]) {
    await assert.rejects(createDeliveryClient(transport, { getItem: bad, setItem: () => assert.fail('bad records must not be overwritten') }, reserveDeliveryIntent).prepare(draft, file, status), /读不了上次/)
  }
  await assert.rejects(createDeliveryClient(transport, { getItem: () => null, setItem: () => { throw new Error('first write failed') } }, reserveDeliveryIntent).prepare(draft, file, status), /尚未提交/)
  let failRecent = true
  const storage = {
    getItem: (key: string) => records.get(key) ?? null,
    setItem: (key: string, value: string) => {
      if (key.endsWith('.recent') && failRecent) throw new Error('recent write failed')
      records.set(key, value)
    },
  }
  await assert.rejects(createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(draft, file, status), /尚未提交/)
  assert.equal(calls.length, 0)
  const key = `vibeface.delivery.network.client.${source}.intent.e4d2ac185fb4490bd606a5ba49f715bed1df4b5a85ffd888c0ac06f127f34eec`
  const retained = records.get(key)
  assert.ok(retained)
  const id: unknown = JSON.parse(retained).intent_id
  failRecent = false
  const retry = await createDeliveryClient(transport, storage, reserveDeliveryIntent).prepare(draft, file, status)
  assert.equal(retry.intent_id, id)
  assert.equal(records.get(key), retained)
  assert.deepEqual(calls, [{ op: 'prepare', receiver_peer: receiver, provider_peer: provider, mode: 'charity', acceptance: 'recoverable', name: 'example.txt', max_service_amount: '0', max_gas_amount: '0', budget_id: null, intent_id: id, content_base64: 'YSBmaWxlIHRoZSByZWNlaXZlciBjYW4gcmVjb3Zlcg==' }])
})

function reservationGate() {
  let release = () => {}, entered = () => {}
  const waiting = new Promise<void>(resolve => { entered = resolve })
  const ready = new Promise<void>(resolve => { release = resolve })
  const reserve: ReserveDeliveryIntent = async (_key, callback) => { entered(); await ready; return callback() }
  return { waiting, release, reserve }
}

test('connection changes while awaiting reservation stop writes and changes after reservation stop submission', { timeout: 10000 }, async () => {
  for (const change of ['waiting', 'reserved']) {
    let current = true, writes = 0, requests = 0
    const transport = connection(async () => { requests++; return order() })
    transport.isCurrent = () => current
    const gate = reservationGate()
    const reserve: ReserveDeliveryIntent = change === 'waiting' ? gate.reserve : async (_key, callback) => { const saved = callback(); current = false; return saved }
    const pending = createDeliveryClient(transport, { getItem: () => null, setItem: () => { writes++ } }, reserve).prepare(draft, file, status)
    if (change === 'waiting') { await gate.waiting; current = false; gate.release() }
    await assert.rejects(pending, /连接已切换/)
    assert.equal(writes, change === 'waiting' ? 0 : 2)
    assert.equal(requests, 0)
  }
})

test('waiting for reservation retains the original draft, file properties and selected bytes', { timeout: 10000 }, async () => {
  const mutableDraft = { ...draft }, buffer = new Uint8Array(content)
  const mutableFile = { ...file, arrayBuffer: async () => buffer.buffer }
  const gate = reservationGate(), calls: Record<string, unknown>[] = [], writes: [string, string][] = []
  const client = createDeliveryClient(connection(async (_method, params = {}) => { calls.push(params); return { ...order(), intent_id: params.intent_id } }), { getItem: () => null, setItem: (key, value) => { writes.push([key, value]) } }, gate.reserve)
  const pending = client.prepare(mutableDraft, mutableFile, status)
  await gate.waiting
  mutableDraft.receiver_peer = source; mutableDraft.provider_peer = source; mutableDraft.mode = 'paid'
  mutableDraft.max_service_amount = '99'; mutableDraft.max_gas_amount = '99'; mutableDraft.budget_id = '99'.repeat(32)
  mutableFile.name = 'mutated.txt'; mutableFile.size = 0; buffer.fill(0)
  gate.release()
  const saved = await pending
  assert.equal(writes[0][0], `vibeface.delivery.network.client.${source}.intent.e4d2ac185fb4490bd606a5ba49f715bed1df4b5a85ffd888c0ac06f127f34eec`)
  assert.deepEqual(calls, [{ op: 'prepare', receiver_peer: receiver, provider_peer: provider, mode: 'charity', acceptance: 'recoverable', name: 'example.txt', max_service_amount: '0', max_gas_amount: '0', budget_id: null, intent_id: saved.intent_id, content_base64: 'YSBmaWxlIHRoZSByZWNlaXZlciBjYW4gcmVjb3Zlcg==' }])
})

test('delivery retains canonical base64 errors and validates the received field budget before decoding', async () => {
  assert.deepEqual(fromBase64('Zg=='), new Uint8Array([102]))
  assert.deepEqual(fromBase64(''), new Uint8Array())
  assert.throws(() => fromBase64('Zh=='), { message: '文件编码不是规范格式。' })
  for (const value of ['Zg', ' Zg==\n', 'Zg===', '*', '_w==']) assert.throws(() => fromBase64(value), { message: '文件编码无效。' })
  const received = { name: 'example.txt', bytes: 1, content_base64: 'Zg==', sha256: '0'.repeat(64) }
  assert.deepEqual(parseReceived(received, 1), received)
  assert.throws(() => parseReceived({ ...received, content_base64: 'Zg==A' }, 1), { message: '取回文件超过当前设备上限。' })
  assert.throws(() => parseReceived({ ...received, content_base64: null }, 1), { message: '文件内容缺失。' })
  await assert.rejects(checkedFile({ ...received, content_base64: 'Zh==' }, order()), { message: '文件编码不是规范格式。' })
  await assert.rejects(checkedFile({ ...received, content_base64: '*' }, order()), { message: '文件编码无效。' })
})
