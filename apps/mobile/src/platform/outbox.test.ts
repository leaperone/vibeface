import assert from 'node:assert/strict'
import test from 'node:test'
import { createHash } from 'node:crypto'
import { mobileOutbox } from './outbox.ts'
import { decodeOutbox } from '../../../../packages/client/src/workspace/outbox-codec.ts'
import { enqueue, settle, OutboxCorruptError, type OutboxItem } from '../../../../packages/client/src/lib/outbox.ts'
import { writeBody, writeKeySuffix, ownerBody, type WriteOperation } from '../../../../packages/client/src/workspace/write-request.ts'
import { requestKey } from '../../../../packages/client/src/lib/secure.ts'
import { storageOf } from '../../../../packages/client/src/workspace/storage.ts'
import { decodeLastSeen, displayLastSeen } from '../data/last-seen.ts'

const network = 'a'.repeat(64), client = 'b'.repeat(64), entry = 'c'.repeat(64)
const prefix = `${network}.${client}:`
// A controlled verifier checks every byte. These are test fingerprints, not cryptographic attestations.
const signature = (payload: Uint8Array) => createHash('sha512').update(payload).digest('hex')
const secure = {
  attestationPayload: (network: string, scope: string, body: string) => new TextEncoder().encode(JSON.stringify({ network, scope, body })),
  verify(device: string, domain: string, payload: Uint8Array, signed: string) { assert.equal(device, client); assert.equal(domain, 'attestation'); assert.equal(signed, signature(payload)) },
}
async function signed(operation: WriteOperation, scope = 'chat'): Promise<OutboxItem> {
  if (operation.method === 'workspace_command') {
    scope = 'workspace'
    operation.params.command_id = await requestKey(secure.attestationPayload(network, scope, JSON.stringify({ type: 'workspace_commanded', command: operation.params.command })), client)
  }
  if (operation.method === 'set_primary_bot') scope = 'bots'
  const payload = secure.attestationPayload(network, scope, JSON.stringify(writeBody(operation, client)))
  if (operation.method === 'control_memory') operation.params.operation_id = await requestKey(payload, client)
  if (operation.method === 'create_bot') operation.params.owner_attestation = { device_id: client, signature: signature(secure.attestationPayload(network, scope, JSON.stringify(ownerBody(entry, 1)))) }
  const key = prefix + writeKeySuffix(operation)
  return decodeOutbox({ key, method: operation.method, scope_id: scope, params: { ...operation.params, attestation: { device_id: client, signature: signature(payload) } }, created_at_ms: 1, ...(operation.method === 'message' ? { delivery: 'saved' } : {}) }, key)
}
function fixture() {
  const raw = new Map<string, string>(), mutations: string[] = []
  const native = { getAllKeys: () => [...raw.keys()], getString: (key: string) => raw.get(key), set: (key: string, value: string) => { mutations.push('put'); raw.set(key, value) }, remove: (key: string) => { mutations.push('delete'); raw.delete(key) } }
  const store = mobileOutbox(native, secure)
  return { raw, native, mutations, store }
}
const ack = () => signed({ method: 'ack', params: { notification_id: 'n1' } })
const message = () => signed({ method: 'message', params: { conversation_id: 'chat', message_id: 'm1', text: 'hi', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, report_results: true } })

test('production MMKV factory round trips every known request without changing signed bytes', async () => {
  const operations: WriteOperation[] = [
    { method: 'message', params: { conversation_id: 'chat', message_id: 'm1', text: 'hi', target: { kind: 'bot_main', bot_id: 'bot' }, intent: 'continue', sequence: 1, report_results: true } },
    { method: 'workspace_command', params: { command_id: '', command: { kind: 'control_input', message_id: 'm1', expected_event: 'd'.repeat(64), action: { kind: 'cancel' } } } },
    { method: 'control_memory', params: { conversation_id: 'chat', memory_id: 'memo', base: { epoch: 1, version: null }, after: '记忆', operation_id: '' } },
    { method: 'ack', params: { notification_id: 'n1' } },
    { method: 'create_bot', params: { bot_id: 'bot', conversation_id: 'chat', name: 'Bot', instructions: '', scheduler_node: entry, owner_attestation: { device_id: client, signature: '' } } },
    { method: 'delete_bot', params: { conversation_id: 'chat' } },
    { method: 'set_primary_bot', params: { conversation_id: 'chat' } },
    { method: 'set_bot_project', params: { conversation_id: 'chat', project: null } },
    { method: 'set_bot_avatar', params: { conversation_id: 'chat', version: null, avatar: null } },
    { method: 'control_schedule', params: { conversation_id: 'chat', schedule_id: 's', action: 'pause' } },
    { method: 'set_owner', params: { conversation_id: 'chat', scheduler_node: entry, epoch: 2 } },
    { method: 'dismiss_work', params: { conversation_id: 'chat', work_id: 'w' } },
    { method: 'answer', params: { question_id: 'q', text: 'yes' } },
    { method: 'resolve', params: { conversation_id: 'chat', check_id: 'work:w', outcome: 'completed', note: '' } },
  ]
  const f = fixture(), expected = []
  for (const operation of operations) { const item = await signed(operation).catch(cause => { throw new Error(operation.method, { cause }) }); expected.push(item); await enqueue(f.store, item) }
  assert.deepEqual(await f.store.all(), JSON.parse(JSON.stringify(expected)))
  const before = [...f.raw]
  await enqueue(f.store, { ...expected[0], created_at_ms: 99 })
  assert.deepEqual([...f.raw], before)
})

test('a corrupt final record stops actual enqueue and settle, retaining every raw byte', async () => {
  for (const broken of ['{', 'null', '[]', 'false', '"s"', JSON.stringify({ ...await ack(), method: 'future_method' })]) {
    const f = fixture(), item = await ack()
    f.raw.set('outbox:' + item.key, JSON.stringify(item)); f.raw.set('outbox:bad-last', broken)
    const before = [...f.raw]
    let calls = 0
    await assert.rejects(settle(f.store, { recorded: async () => { calls++; return true }, submit: async () => { calls++ } }), OutboxCorruptError)
    await assert.rejects(enqueue(f.store, item), OutboxCorruptError)
    await assert.rejects(f.store.put(item), OutboxCorruptError)
    await assert.rejects(f.store.delete(item.key), OutboxCorruptError)
    assert.equal(calls, 0); assert.deepEqual(f.mutations, []); assert.deepEqual([...f.raw], before)
  }
})

test('wrong scope, physical key, signer, unsafe numbers and extra signed fields fail closed', async () => {
  const good = await ack()
  const mutations = [ { ...good, scope_id: 'other' }, { ...good, created_at_ms: 1e400 }, { ...good, created_at_ms: 2 ** 53 }, { ...good, params: { ...good.params, conversation_id: 'other' } }, { ...good, params: { ...good.params, attestation: { ...good.params.attestation, manage: true } } }, { ...good, uploads: [] }, { ...good, params: { ...good.params, attestation: { device_id: entry, signature: good.params.attestation.signature } } } ]
  for (const item of mutations) {
    const f = fixture(); f.raw.set('outbox:' + good.key, JSON.stringify(item)); const before = [...f.raw]
    await assert.rejects(f.store.all(), OutboxCorruptError); assert.deepEqual([...f.raw], before)
  }
  const f = fixture(); f.raw.set('outbox:wrong', JSON.stringify(good)); await assert.rejects(f.store.all(), OutboxCorruptError)
  f.store.reset(); f.native.getAllKeys = () => ['outbox:missing']; await assert.rejects(f.store.all(), OutboxCorruptError)
})

test('message receipt, mandatory delivery and uploaded bytes are checked before submit', async () => {
  const good = await message()
  const variants = [{ ...good, delivery: undefined }, { ...good, delivery: 'unknown' }, { ...good, uploads: [{ hash: 'd'.repeat(64), size: 1, name: 'x', data: '%%%' }] }, { ...good, receipt: { message_id: 'other', target: { kind: 'bot_main', bot_id: 'bot' }, stage: 'unknown', actual_turn_id: null, reason: null, retry_safe: false, control_event: null } }]
  for (const value of variants) { const f = fixture(); f.raw.set('outbox:' + good.key, JSON.stringify(value)); await assert.rejects(f.store.all(), OutboxCorruptError); assert.deepEqual(f.mutations, []) }
})

test('required storage preserves present null; display cache rejects a whole bad snapshot', () => {
  const raw = new Map<string, string>(), storage = storageOf({ getItem: key => raw.get(key) ?? null, setItem: (key, value) => { raw.set(key, value) } })
  assert.deepEqual(storage.readStorage('sequences', {}, true), {})
  raw.set('vibeface.sequences', 'null'); assert.equal(storage.readStorage('sequences', {}, true), null)
  const scope = { network_id: network, device_id: client, entry_device_id: entry }
  const cached = { ...scope, entry_label: '电脑', bots: [], primary: null, members: [{ device_id: client, name: '手机', kind: 'client', manage: false, bot: null }, { device_id: entry, name: '电脑', kind: 'node', manage: false, bot: 'ready' }] }
  const parsed = decodeLastSeen(cached, scope); assert.ok(parsed)
  assert.equal(displayLastSeen(parsed).members[0].current, true)
  for (const value of [null, [], { ...cached, network_id: entry }, { ...cached, primary: undefined }, { ...cached, members: [cached.members[0], { ...cached.members[1], manage: true }] }, { ...cached, members: [{ ...cached.members[0], manage: undefined }, cached.members[1]] }]) assert.equal(decodeLastSeen(value, scope), null)
  const polluted = JSON.parse(JSON.stringify(cached)); Object.assign(polluted.members[0], JSON.parse('{"admin":true,"current":false,"online":true,"__proto__":{"polluted":true}}'))
  const clean = decodeLastSeen(polluted, scope); assert.ok(clean); assert.equal(displayLastSeen(clean).members[0].admin, false); assert.equal(displayLastSeen(clean).members[0].online, undefined)
})
