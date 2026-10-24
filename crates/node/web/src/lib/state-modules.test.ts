import assert from 'node:assert/strict'
import test from 'node:test'
import { nextLink, type Link } from '@vibeface/client/lib/link.ts'
import { enqueue, settle, type OutboxItem, type OutboxStore } from '@vibeface/client/lib/outbox.ts'
import { joinTabs } from './tabs.ts'
import { candidates, isPublicUrl, markReached, mergeEntrances, verifiedEntrances } from '@vibeface/client/lib/entrances.ts'
import { humanError, rawError } from '@vibeface/client/lib/errors.ts'

test('the link moves through join, connect, catch-up, loss and revocation', () => {
  let link: Link = { kind: 'booting' }
  link = nextLink(link, { type: 'ready', joined: false })
  assert.deepEqual(link, { kind: 'unjoined' })
  link = nextLink(link, { type: 'connected', behind: false })
  assert.deepEqual(link, { kind: 'unjoined' }, 'an unjoined browser cannot be online')
  link = nextLink(link, { type: 'ready', joined: true })
  link = nextLink(link, { type: 'connected', behind: true })
  assert.deepEqual(link, { kind: 'online', catchingUp: true })
  link = nextLink(link, { type: 'caught-up' })
  link = nextLink(link, { type: 'lost', reason: 'closed' })
  assert.deepEqual(link, { kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' })
  link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'again' })
  assert.deepEqual(link, { kind: 'offline', attempt: 2, retryInMs: 1000, reason: 'again' })
  for (let i = 0; i < 10; i++) link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'down' })
  assert.equal(link.kind === 'offline' && link.retryInMs, 10_000)
  link = nextLink(link, { type: 'revoked' })
  assert.deepEqual(nextLink(link, { type: 'connect' }), { kind: 'revoked' })
  assert.deepEqual(nextLink(link, { type: 'left' }), { kind: 'unjoined' })
})

function memory(): OutboxStore & { items: Map<string, OutboxItem> } {
  const items = new Map<string, OutboxItem>()
  return { items, all: async () => [...items.values()], put: async item => { items.set(item.key, structuredClone(item)) }, delete: async key => { items.delete(key) } }
}
const write = (key: string, at: number, text = key): OutboxItem => ({ key, method: 'message', params: { message_id: key, text, attestation: { device_id: 'd', signature: `sig-${text}` } }, created_at_ms: at })

test('the outbox confirms recorded writes, resubmits the rest unchanged, and stops at an unknown result', async () => {
  const store = memory()
  await enqueue(store, write('m1', 1))
  await enqueue(store, write('m2', 2))
  await enqueue(store, write('m3', 3))
  await enqueue(store, write('m4', 4))
  assert.equal((await enqueue(store, write('m1', 9))).created_at_ms, 1, 'the same request keeps its first copy')
  await assert.rejects(enqueue(store, write('m1', 9, 'changed')), /不同内容/)
  const submitted: unknown[] = []
  const result = await settle(store, {
    recorded: async item => item.key === 'm1',
    submit: async item => {
      submitted.push(item.params)
      if (item.key === 'm3') throw Object.assign(new Error('conversation not found'), { responded: true })
      if (item.key === 'm4') throw new Error('connection lost')
    },
  })
  assert.deepEqual(result, { confirmed: ['m1'], resubmitted: ['m2'], rejected: ['m3'], unknown: 'm4' })
  assert.deepEqual(submitted, [write('m2', 2).params, write('m3', 3).params, write('m4', 4).params], 'resubmitted byte-for-byte')
  assert.deepEqual([...store.items.keys()], ['m3', 'm4'])
  assert.equal(store.items.get('m3')?.rejected, 'conversation not found')
  const again = await settle(store, { recorded: async () => true, submit: async () => assert.fail('nothing to send') })
  assert.deepEqual(again, { confirmed: ['m4'], resubmitted: [], rejected: [], unknown: null }, 'a refused write is not retried')
})

test('one tab leads and the next takes over when it closes', { skip: !globalThis.navigator?.locks && 'Web Locks need Node 24 or later' }, async () => {
  const name = `vibeface-test-${Math.random()}`
  const events: string[] = []
  const settle = () => new Promise(resolve => setTimeout(resolve, 20))
  const first = joinTabs<string>(name, { lead: () => { events.push('first leads'); return () => events.push('first resigns') }, message: value => events.push(`first got ${value}`) })
  await settle()
  const second = joinTabs<string>(name, { lead: () => { events.push('second leads') }, message: value => events.push(`second got ${value}`) })
  await settle()
  first.post('changed')
  await settle()
  first.close()
  await settle()
  assert.deepEqual(events, ['first leads', 'second got changed', 'first resigns', 'second leads'])
  second.close()
})

const node = (id: string) => ({ device_id: id.repeat(64), kind: 'node' as const, name: id, manage: false })
const view = { network_id: 'n'.repeat(64), epoch: 1, recovery_key: 'n'.repeat(64), heads: [], members: [node('a'), node('b'), node('c'), { ...node('d'), kind: 'client' as const }], revoked: [{ device_id: 'c'.repeat(64), cutoff_seq: 0, cutoff_hash: null }], attesters: [] }
const address = (id: string, gateway: string | null, network = view.network_id, seq = 1) => ({ payload: JSON.stringify({ version: 1, network_id: network, device_id: id.repeat(64), seq, endpoint: { id: id.repeat(64), addrs: [] }, gateway_url: gateway }), signature: `signed-by-${id}` })
const signedBySelf = (deviceId: string, _payload: string, signature: string) => signature === `signed-by-${deviceId[0]}`

test('only entrance records signed by a current member Node of this network survive', () => {
  const records = [
    address('a', 'https://a.example.com/'), address('b', 'http://192.168.1.4:4790/'),
    address('c', 'https://revoked.example.com/'), address('d', 'https://client.example.com/'),
    address('a', 'https://a.example.com/', 'o'.repeat(64)), { ...address('b', 'https://forged.example.com/'), signature: 'signed-by-a' },
    address('e', 'https://stranger.example.com/'), address('a', 'https://a.example.com/path'), 'junk',
  ]
  const found = verifiedEntrances(records, view, signedBySelf)
  assert.deepEqual(found.map(item => [item.device_id[0], item.gateway_url, item.public]), [['a', 'https://a.example.com', true], ['b', 'http://192.168.1.4:4790', false]])
})

test('switching tries the entry that dropped, then entrances by last success; revoked ones leave the list', () => {
  const saved = [
    { device_id: 'a'.repeat(64), seq: 1, gateway_url: 'https://a.example.com', public: true, last_ok_ms: 5 },
    { device_id: 'b'.repeat(64), seq: 1, gateway_url: 'https://b.example.com', public: true, last_ok_ms: 9 },
    { device_id: 'c'.repeat(64), seq: 1, gateway_url: 'https://c.example.com', public: true, last_ok_ms: 99 },
  ]
  const merged = markReached(mergeEntrances(saved, [], view), { address: 'https://a.example.com', device_id: 'a'.repeat(64) }, 1)
  assert.deepEqual(merged.map(item => [item.device_id[0], item.last_ok_ms]), [['a', 1], ['b', 9]], 'c was revoked')
  const order = candidates(merged, { address: 'http://127.0.0.1:5101', device_id: 'a'.repeat(64) })
  assert.deepEqual(order.map(entry => entry.address), ['http://127.0.0.1:5101', 'https://b.example.com', 'https://a.example.com'])
  for (const local of ['https://localhost', 'https://10.0.0.2', 'https://box.local', 'http://a.example.com', 'https://intranet']) assert.equal(isPublicUrl(local), false, local)
})

test('after a reload an older address record never replaces a newer one, withdrawn addresses included', () => {
  const a = 'a'.repeat(64), b = 'b'.repeat(64)
  const saved = mergeEntrances([], verifiedEntrances([address('a', 'https://new.example.com/', view.network_id, 7), address('b', null, view.network_id, 4)], view, signedBySelf), view)
  const reloaded = JSON.parse(JSON.stringify(saved))
  const stale = verifiedEntrances([address('a', 'https://old.example.com/', view.network_id, 6), address('b', 'https://withdrawn.example.com/', view.network_id, 3)], view, signedBySelf)
  const merged = mergeEntrances(reloaded, stale, view)
  assert.deepEqual(merged.map(item => [item.device_id[0], item.seq, item.gateway_url]), [['a', 7, 'https://new.example.com'], ['b', 4, null]])
  assert.deepEqual(candidates(merged, { address: 'http://127.0.0.1:5101', device_id: a }).map(entry => entry.address), ['http://127.0.0.1:5101', 'https://new.example.com'], 'a withdrawn address is not tried')
  const newer = mergeEntrances(merged, verifiedEntrances([address('b', 'https://back.example.com/', view.network_id, 5)], view, signedBySelf), view)
  assert.equal(newer.find(item => item.device_id === b)?.gateway_url, 'https://back.example.com')
})

test('backend English and gateway refusal codes reach the page as plain Chinese', () => {
  assert.equal(humanError('unknown member', 'target_not_member'), '这台设备不在我的设备里，访问地址不会转发给它。')
  assert.match(humanError('conversation not found'), /还没有这个对话/)
  assert.match(humanError('Codex SDK failed: Missing environment variable: `CPA_API_KEY`.'), /缺少密钥/)
  assert.match(humanError('CURSOR_API_KEY is required for the Cursor SDK manager'), /缺少密钥/)
  assert.match(humanError('client response timed out; outcome may be unknown'), /结果未知/)
  assert.match(humanError("You've hit your usage limit. Try again at 4:12 PM.", undefined, '这个任务'), /用量到上限/)
  assert.equal(humanError('已断开设备连接。'), '已断开设备连接。')
  assert.match(humanError('peer did not answer in time'), /另一台设备没有回应/)
  assert.match(humanError('Codex rejected request: {"code":-32600,"message":"no rollout found for thread id 01a15c84"}'), /新对话还没准备好/)
  assert.equal(humanError('something nobody mapped'), '这次操作没有成功，请再试一次。')
  assert.equal(humanError('something nobody mapped', undefined, '读取最新内容'), '读取最新内容没有成功，请再试一次。')
  assert.equal(rawError(new Error('conversation not found')), 'conversation not found')
  assert.equal(rawError(new Error('已断开')), '')
})
