import assert from 'node:assert/strict'
import test from 'node:test'
import { notificationOpenings } from './notification-opens.ts'
import { notificationForOpening } from '../../../../packages/client/src/lib/platform.ts'
import type { ReadyConnection } from '../../../../packages/client/src/lib/node.ts'

const target = { network_id: 'network', device_id: 'old-device', notification_id: 'note', conversation_id: 'conversation' }
const tick = () => new Promise<void>(resolve => setImmediate(resolve))
function fixture() {
  const saved = new Map<string, string>()
  const storage = { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value) } }
  return { storage, queue: notificationOpenings(storage) }
}

test('cold launch persists a tap before subscribing and deduplicates the initial response and listener', async () => {
  const { storage, queue } = fixture()
  queue.receive('tap', target)
  await tick()
  const relaunched = notificationOpenings(storage)
  const opened: string[] = []
  relaunched.watch(async destination => { opened.push(destination.conversation_id); return true })
  relaunched.receive('tap', target)
  await tick()
  const nextLaunch = notificationOpenings(storage)
  nextLaunch.watch(async () => { opened.push('duplicate'); return true })
  nextLaunch.receive('tap', target)
  await tick()
  assert.deepEqual(opened, ['conversation'])
})

test('a different network defers a tap until a later connection, and unsubscribed callbacks cannot navigate', async () => {
  const { queue } = fixture()
  let calls = 0
  const stop = queue.watch(async () => { calls++; return false })
  queue.receive('tap', target)
  await tick()
  stop()
  await queue.flush()
  assert.equal(calls, 1)
  const opened: string[] = []
  queue.watch(async destination => { opened.push(destination.network_id); return true })
  await tick()
  assert.deepEqual(opened, ['network'])
})

test('foreground replay retries transient read failure, malformed payloads never route', async () => {
  const { queue } = fixture()
  let calls = 0
  queue.watch(async () => { if (++calls === 1) throw new Error('offline'); return true })
  queue.receive('tap', target)
  queue.receive('bad', { ...target, network_id: 7 })
  await tick()
  await queue.flush()
  await queue.flush()
  assert.equal(calls, 2)
})

function connection(network = 'network'): ReadyConnection {
  return {
    generation: 2, entry: { kind: 'gateway', address: 'http://localhost', device_id: 'node' },
    principal: { source: 'membership', can_manage: true, network_id: network, device_id: 'new-authorized-device', epoch: 1 },
    info: { device_id: 'node', name: 'node', version: 'test', initialized: true, network_id: network },
    isCurrent: () => true, subscribe: () => () => {}, request: async () => null, rpc: async () => null,
  }
}

test('current authenticated inbox recovers the route after reconnect, including authorized same-network device switch', async () => {
  const notification = { notification_id: 'note', conversation_id: 'conversation', kind: 'reply', text: '', target: { kind: 'bot_main', bot_id: 'bot' } } satisfies Awaited<ReturnType<Parameters<typeof notificationForOpening>[0]['read']>>[number]
  const result = await notificationForOpening({ destination: target, connection: connection(), current: () => true, read: async () => [notification] })
  assert.deepEqual(result, { kind: 'target', notification })
})

test('cross-network, stale read completion and mismatching inbox records cannot open a route', async () => {
  let read = false
  const cross = await notificationForOpening({ destination: target, connection: connection('other'), current: () => true, read: async () => { read = true; return [] } })
  assert.equal(cross.kind, 'deferred')
  assert.equal(read, false)
  let active = true
  const stale = await notificationForOpening({ destination: target, connection: connection(), current: () => active, read: async () => { active = false; return [] } })
  assert.equal(stale.kind, 'deferred')
  const invalid = await notificationForOpening({ destination: target, connection: connection(), current: () => true, read: async () => [{ notification_id: 'note', conversation_id: 'different', kind: 'reply', text: '' }] })
  assert.equal(invalid.kind, 'invalid')
})

test('a catch-up miss stays queued until readiness replay finds its authenticated target', async () => {
  const { queue } = fixture()
  const transport = connection()
  let syncing = true
  transport.catchingUp = () => syncing
  const routed: string[] = []
  queue.watch(async destination => {
    const result = await notificationForOpening({ destination, connection: transport, current: () => true, read: async () => syncing ? [] : [{ notification_id: 'note', conversation_id: 'conversation', kind: 'reply', text: '' }] })
    if (result.kind === 'deferred') return false
    if (result.kind === 'target') routed.push(result.notification.conversation_id)
    return true
  })
  queue.receive('tap', target)
  await tick()
  assert.deepEqual(routed, [])
  syncing = false
  await queue.flush()
  assert.deepEqual(routed, ['conversation'])
})

test('clearing private data prevents an in-flight route completion from repopulating the queue', async () => {
  const { storage, queue } = fixture()
  let finish = (_consumed: boolean) => {}
  const gate = new Promise<boolean>(resolve => { finish = resolve })
  queue.watch(async () => gate)
  queue.receive('tap', target)
  queue.clear()
  finish(true)
  await tick()
  assert.deepEqual(JSON.parse(storage.getItem('notification-openings') ?? '{}'), { pending: [], completed: [] })
})
