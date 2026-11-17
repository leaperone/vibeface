import assert from 'node:assert/strict'
import test from 'node:test'
import type { WorkspaceHistory } from '@vibeface/client/workspace/types.ts'
import { createWorkspaceHistoryCache, type WorkspaceHistoryCacheScope } from './workspace-history-cache.ts'

const scope: WorkspaceHistoryCacheScope = { networkId: 'network', deviceId: 'device', target: { kind: 'task', id: 'task' } }
function fixture(ids: string[], complete = true): WorkspaceHistory {
  return { events: ids.map(id => ({ event_hash: id, created_at_ms: 1, scope_id: 'conversation', kind: 'message_posted', speaker: { kind: 'human', device_id: 'device' }, body: { type: 'message_posted', message_id: `message-${id}`, role: 'user', text: `你好 ${id}`, attachments: [{ hash: 'a'.repeat(64), name: 'notes.txt', size: 1 }] } })), next_cursor: complete ? null : 'server-cursor', complete, coverage: { limit: 256, truncated: !complete, scope: 'task' } }
}
function setup(options: { maxBytes?: number; maxScopes?: number; maxEvents?: number } = {}) {
  const records = new Map<string, string>()
  let time = 1
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  return { records, cache: createWorkspaceHistoryCache({ storage, now: () => time++, ...options }), storage }
}
const input = (history = fixture(['a', 'b'])) => ({ history, title: '任务标题', botIds: ['bot'] })

test('cold cache reads validate pages, preserve attachments and scope the display to network, device and target', () => {
  const { cache, storage } = setup()
  assert.equal(cache.write(scope, input(fixture(['a', 'b'], false))), true)
  const cold = createWorkspaceHistoryCache({ storage }).read(scope)
  assert.equal(cold?.title, '任务标题')
  assert.deepEqual(cold?.events.map(event => event.event_hash), ['a', 'b'])
  assert.equal(cold?.complete, false)
  assert.equal(cold?.events[0]?.body.type === 'message_posted' && cold.events[0].body.attachments?.[0]?.name, 'notes.txt')
  for (const other of [{ ...scope, networkId: 'other' }, { ...scope, deviceId: 'other' }, { ...scope, target: { kind: 'channel', id: 'task' } satisfies WorkspaceHistoryCacheScope['target'] }, { ...scope, target: { kind: 'task', id: 'other' } satisfies WorkspaceHistoryCacheScope['target'] }]) assert.equal(cache.read(other), null)
  assert.equal(cold && 'next_cursor' in cold, false)
})

test('trimmed local coverage remains incomplete without inventing a resumable server boundary', () => {
  const { cache } = setup({ maxEvents: 2 })
  assert.equal(cache.write(scope, input(fixture(['a', 'b', 'c']))), true)
  const saved = cache.read(scope)
  assert.deepEqual(saved?.events.map(event => event.event_hash), ['b', 'c'])
  assert.equal(saved?.complete, false)
  assert.equal(saved && 'next_cursor' in saved, false)
})

test('scope eviction and byte trimming keep the cache bounded', () => {
  const { cache, records } = setup({ maxScopes: 2, maxBytes: 1800 })
  cache.write(scope, input(fixture(['a', 'b', 'c', 'd', 'e', 'f'])))
  assert.ok(new TextEncoder().encode([...records.values()][0]).length <= 1800)
  cache.write({ ...scope, target: { kind: 'task', id: 'second' } }, input(fixture(['x'])))
  cache.write({ ...scope, target: { kind: 'task', id: 'third' } }, input(fixture(['y'])))
  assert.equal(cache.read(scope), null)
  assert.ok(cache.read({ ...scope, target: { kind: 'task', id: 'third' } }))
})

test('corrupt payloads, wrong coverage and forged page shape never become visible cache', () => {
  const { cache, records } = setup()
  cache.write(scope, input())
  const key = [...records.keys()][0]
  const page = fixture(['a', 'b'])
  for (const history of [
    { ...page, complete: false },
    { ...page, coverage: { ...page.coverage, scope: 'channel' } },
    { ...page, events: page.events.map(event => ({ ...event, body: { ...event.body, type: 'unknown' } })) },
    { ...page, events: [...page.events, ...page.events] },
  ]) {
    records.set(key, JSON.stringify([{ ...scope, savedAt: 1, title: 'task', botIds: ['bot'], pages: [history], retainedAll: true }]))
    assert.equal(cache.read(scope), null)
  }
  records.set(key, 'not json')
  assert.equal(cache.read(scope), null)
})

test('offline search returns real message locations and matches visible text only', () => {
  const { cache } = setup()
  cache.write(scope, input())
  assert.deepEqual(cache.search(scope, '你好 a'), [{ event_hash: 'a', message_id: 'message-a', conversation_id: 'conversation', text: '你好 a', created_at_ms: 1, speaker: { kind: 'human', device_id: 'device' } }])
  assert.deepEqual(cache.search(scope, 'notes.txt'), [])
  assert.deepEqual(cache.search(scope, 'message_posted'), [])
  assert.deepEqual(cache.search(scope, '   '), [])
})


test('merged multi-page windows survive cold reading without losing complete coverage', () => {
  const { cache, storage } = setup()
  const events = Array.from({ length: 1100 }, (_, index) => `event-${index}`)
  assert.equal(cache.write(scope, input(fixture(events))), true)
  const cold = createWorkspaceHistoryCache({ storage })
  assert.deepEqual(cold.read(scope)?.events.map(event => event.event_hash), events)
  assert.equal(cold.read(scope)?.complete, true)
  assert.equal(cold.all({ networkId: scope.networkId, deviceId: scope.deviceId }).length, 1)
  assert.deepEqual(cold.all({ networkId: 'other', deviceId: scope.deviceId }), [])
})

test('invalid cache capacities fail immediately rather than admitting unbounded or non-terminating eviction', () => {
  for (const options of [{ maxScopes: 0 }, { maxEvents: -1 }, { maxBytes: 0 }]) assert.throws(() => setup(options), /容量无效/)
})
