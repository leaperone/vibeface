import assert from 'node:assert/strict'
import test from 'node:test'
import { nextLink, type Link } from './link.ts'
import { enqueue, settle, type OutboxItem, type OutboxStore } from './outbox.ts'
import { joinTabs } from './tabs.ts'

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
  assert.deepEqual(link, { kind: 'offline', attempt: 1, retryInMs: 1000, reason: 'closed' })
  link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'again' })
  assert.deepEqual(link, { kind: 'offline', attempt: 2, retryInMs: 2000, reason: 'again' })
  for (let i = 0; i < 10; i++) link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'down' })
  assert.equal(link.kind === 'offline' && link.retryInMs, 30_000)
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

test('one tab leads and the next takes over when it closes', async () => {
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
