import assert from 'node:assert/strict'
import test from 'node:test'
import { conversationCacheStorageKey, createConversationCache } from './conversation-cache.ts'
import { ConversationReadOwner, markVisibleMessagesRead, readConversationSnapshot } from './conversation-read.ts'
import { completeHistory } from './history-fixture.ts'
import { mergeHistoryPage } from './history.ts'
import { parseConversation, parseHistoryPage } from './model.ts'
import type { ConversationPayload, NotificationRecord } from './types.ts'

const scope = { networkId: 'network', deviceId: 'phone' }
const key = (conversationId: string) => ({ ...scope, conversationId })
function storage() {
  const values = new Map<string, string>()
  return { getItem: (name: string) => values.get(name) ?? null, setItem: (name: string, value: string) => { values.set(name, value) } }
}
const message = (id: string, rank = 1) => ({ message_id: id, event_hash: `event-${id}`, role: 'assistant', text: id, created_at_ms: rank, rank: String(rank).padStart(20, '0') })
function raw(conversationId = 'A', extra: Record<string, unknown> = {}) {
  return {
    conversation: { conversation_id: conversationId, owner: 'owner', owner_epoch: 1, title: '' }, history_authority: 'authority', history: completeHistory(),
    messages: [message(conversationId)], works: [], deliveries: [], checks: [], local_node: 'entry', changes: [], overwritten: [],
    scheduler: { jobs: [], scope_jobs: [], unanswered: [] },
    bot_settings: { skills: [], schedules: [], memory: [], memory_conflicts: [], memory_sync_conflicts: { items: [], has_more: false } },
    sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [], sessions: [], ...extra,
  }
}
const snapshot = (id = 'A', extra: Record<string, unknown> = {}) => parseConversation(raw(id, extra), id)
const deferred = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (caught: unknown) => void = () => {}
  const promise = new Promise<unknown>((done, fail) => { resolve = done; reject = fail })
  return { promise, resolve, reject }
}

test('cold startup restores validated snapshots, authority, pagination and save time without confirming a read', async () => {
  const local = storage(), first = createConversationCache({ storage: local, now: () => 1234 })
  const bounded = snapshot('A', { history: { ...completeHistory(), messages: { next_cursor: 'before-A', complete: false } } })
  first.save({ ...key('A'), payload: bounded })
  first.save({ ...key('B'), payload: snapshot('B') })
  const cache = createConversationCache({ storage: local }), owner = new ConversationReadOwner()
  assert.deepEqual(cache.read(key('A')), { ...key('A'), savedAt: 1234, payload: bounded })
  assert.deepEqual(cache.all(scope).map((item) => item.conversationId), ['A', 'B'])
  assert.equal(owner.confirmed(), false)
  const writes: string[] = []
  await markVisibleMessagesRead({ owner, messageIds: ['A'], messages: bounded.messages, notifications: [{ notification_id: 'event-A', conversation_id: 'A', kind: 'manager_response', text: 'reply' }], conversationId: 'A', current: () => true, acknowledge: async (item) => { writes.push(item.notification_id) } })
  assert.deepEqual(writes, [])
  assert.equal(owner.confirmed(), false)
})

test('network, device and conversation keys isolate equal IDs and scoped clearing survives restart', () => {
  const local = storage(), cache = createConversationCache({ storage: local })
  cache.save({ ...key('A'), payload: snapshot('A') })
  cache.save({ ...key('A'), networkId: 'other-network', payload: snapshot('A', { messages: [message('other-network')] }) })
  cache.save({ ...key('A'), deviceId: 'other-phone', payload: snapshot('A', { messages: [message('other-phone')] }) })
  cache.save({ ...key('B'), payload: snapshot('B') })
  assert.equal(cache.read({ ...key('A'), networkId: 'missing' }), null)
  assert.equal(cache.read({ ...key('A'), deviceId: 'other-phone' })?.payload.messages[0]?.message_id, 'other-phone')
  cache.clear(key('A'))
  assert.deepEqual(cache.all(scope).map((item) => item.conversationId), ['B'])
  cache.clear(scope)
  assert.deepEqual(createConversationCache({ storage: local }).all(scope), [])
  assert.equal(cache.all({ ...scope, networkId: 'other-network' }).length, 1)
  assert.equal(cache.all({ ...scope, deviceId: 'other-phone' }).length, 1)
})

test('corrupt JSON, invalid timestamps, identity mismatches and invalid payloads never become display records', () => {
  const local = storage(), cache = createConversationCache({ storage: local })
  for (const value of ['{', '{}', JSON.stringify([{ ...key('A'), savedAt: -1, payload: snapshot() }]), JSON.stringify([{ ...key('A'), savedAt: 0, payload: snapshot('B') }]), JSON.stringify([{ ...key('A'), savedAt: 0, payload: { ...snapshot(), history: {} } }])]) {
    local.setItem(conversationCacheStorageKey, value)
    assert.equal(cache.read(key('A')), null)
  }
  cache.save({ ...key('A'), payload: snapshot() })
  const good: unknown = JSON.parse(local.getItem(conversationCacheStorageKey) ?? 'null')
  assert.ok(Array.isArray(good))
  local.setItem(conversationCacheStorageKey, JSON.stringify([null, {}, ...good]))
  assert.equal(cache.all(scope).length, 1)
  assert.throws(() => cache.save({ ...key('A'), payload: snapshot('B') }))
})

test('capacity eviction drops whole conversations and preserves truthful history coverage', () => {
  const local = storage(), cache = createConversationCache({ storage: local, maxConversations: 2 })
  for (const id of ['A', 'B', 'C']) cache.save({ ...key(id), payload: snapshot(id) })
  assert.deepEqual(cache.all(scope).map((item) => item.conversationId), ['B', 'C'])
  const size = new TextEncoder().encode(local.getItem(conversationCacheStorageKey) ?? '').length
  const bounded = createConversationCache({ storage: local, maxBytes: size, maxConversations: 2 })
  bounded.save({ ...key('huge'), payload: snapshot('huge', { messages: [message('x'.repeat(size))] }) })
  bounded.save({ ...key('B'), payload: snapshot('B', { messages: [message('x'.repeat(size))] }) })
  assert.deepEqual(bounded.all(scope).map((item) => item.conversationId), ['B', 'C'])
  assert.ok(bounded.stats(scope).bytes <= size)
  assert.equal(bounded.read(key('C'))?.payload.history.messages.complete, true)
  bounded.clear(scope)
  assert.equal(bounded.stats(scope).conversations, 0)
  assert.throws(() => createConversationCache({ storage: local, maxConversations: 0 }))
})

test('a failed disk write retains bounded in-memory conversations for immediate switching', () => {
  const local = storage()
  let unavailable = false
  const cache = createConversationCache({ storage: { getItem: local.getItem, setItem: (name, value) => { if (unavailable) throw new Error('disk unavailable'); local.setItem(name, value) } } })
  cache.save({ ...key('A'), payload: snapshot('A') })
  unavailable = true
  assert.throws(() => cache.save({ ...key('B'), payload: snapshot('B') }), /disk unavailable/)
  assert.deepEqual(cache.all(scope).map((item) => item.conversationId), ['A', 'B'])
  assert.deepEqual(createConversationCache({ storage: local }).all(scope).map((item) => item.conversationId), ['A'])
})

test('cache reparses local and owner scheduler jobs without turning merged owner jobs into local jobs', () => {
  const local = storage(), cache = createConversationCache({ storage: local })
  const job = (trigger: string, state: string) => ({ trigger, state, scope: { kind: 'main' } })
  const payload = snapshot('A', { scheduler: { jobs: [job('shared', 'queued')], scope_jobs: [], unanswered: [], remote: { owner: 'owner', reachable: true, jobs: [job('shared', 'running'), job('owner-only', 'completed')], scope_jobs: [] } } })
  cache.save({ ...key('A'), payload })
  const restored = createConversationCache({ storage: local }).read(key('A'))?.payload
  assert.deepEqual(restored, payload)
  assert.deepEqual(restored?.scheduler?.local_jobs.map((item) => [item.trigger, item.state]), [['shared', 'queued']])
  assert.deepEqual(restored?.scheduler?.remote?.jobs.map((item) => item.trigger), ['shared', 'owner-only'])
})

function reader() {
  const cache = createConversationCache({ storage: storage() }), owner = new ConversationReadOwner()
  let activeId = '', generation = 1, network = 'network', displayed: ConversationPayload | null = null, loading = false, error: unknown = null, authorityChanges = 0
  const select = (id: string) => { activeId = id; owner.invalidate(); displayed = cache.read({ ...key(id), networkId: network })?.payload ?? null; error = null; loading = false }
  const read = (reply: Promise<unknown>) => {
    const id = activeId, expectedGeneration = generation, expectedNetwork = network
    return readConversationSnapshot({
      conversationId: id, owner, current: () => activeId === id && generation === expectedGeneration && network === expectedNetwork,
      read: () => reply, shown: () => displayed, loading: (value) => { loading = value }, failed: (caught) => { error = caught }, authorityChanged: () => { authorityChanges += 1 },
      apply: async (payload) => { displayed = payload; cache.save({ ...key(id), networkId: network, payload }); return payload },
    })
  }
  return { cache, owner, select, read, shown: () => displayed, loading: () => loading, error: () => error, authorityChanges: () => authorityChanges, reconnect: () => { generation += 1; owner.invalidate() }, changeNetwork: () => { network = 'other'; owner.invalidate(); displayed = null } }
}

test('cached A/B switching starts B independently and late A cannot overwrite B or its cache', async () => {
  const state = reader()
  for (const id of ['A', 'B']) state.cache.save({ ...key(id), payload: snapshot(id) })
  state.select('A')
  const a = deferred(), oldRead = state.read(a.promise)
  assert.equal(state.shown()?.messages[0]?.message_id, 'A')
  state.select('B')
  assert.equal(state.shown()?.messages[0]?.message_id, 'B')
  assert.equal(state.owner.confirmed(), false)
  const b = deferred(), currentRead = state.read(b.promise)
  b.resolve(raw('B', { messages: [message('B-new')] }))
  await currentRead
  assert.equal(state.loading(), false)
  assert.equal(state.owner.confirmed(), true)
  a.resolve(raw('A', { messages: [message('late-A')] }))
  assert.equal(await oldRead, null)
  assert.equal(state.shown()?.conversation?.conversation_id, 'B')
  assert.equal(state.shown()?.messages[0]?.message_id, 'B-new')
  assert.equal(state.cache.read(key('A'))?.payload.messages[0]?.message_id, 'A')
  assert.equal(state.error(), null)
})

test('newer reads own loading and errors; stale failures and prior connection generations have no effect', async () => {
  const state = reader(); state.select('A')
  const old = deferred(), current = deferred(), first = state.read(old.promise), second = state.read(current.promise)
  old.reject(new Error('old request failed'))
  await first
  assert.equal(state.error(), null)
  assert.equal(state.loading(), true)
  current.resolve(raw())
  await second
  assert.equal(state.loading(), false)
  const reconnecting = deferred(), stale = state.read(reconnecting.promise)
  state.reconnect()
  reconnecting.resolve(raw('A', { messages: [message('old-generation')] }))
  await stale
  assert.equal(state.owner.confirmed(), false)
  assert.equal(state.shown()?.messages[0]?.message_id, 'A')
  const crossing = deferred(), crossNetwork = state.read(crossing.promise)
  state.changeNetwork()
  crossing.resolve(raw())
  await crossNetwork
  assert.equal(state.shown(), null)
  assert.equal(state.cache.all({ ...scope, networkId: 'other' }).length, 0)
})

test('a required current read failure remains explicit and retains the cold cached snapshot without permission', async () => {
  const state = reader(); state.cache.save({ ...key('A'), payload: snapshot() }); state.select('A')
  await assert.rejects(state.read(Promise.reject(new Error('offline'))), /offline/)
  assert.equal(state.shown()?.messages[0]?.message_id, 'A')
  assert.equal(state.loading(), false)
  assert.ok(state.error() instanceof Error)
  assert.equal(state.owner.confirmed(), false)
})

test('reconnect merges cached pages, exposes a disjoint gap, and replaces a changed authority', async () => {
  const state = reader(), bounded = snapshot('A', { history: { ...completeHistory(), messages: { next_cursor: 'before-A', complete: false } } })
  const loaded = mergeHistoryPage(bounded, parseHistoryPage({ conversation_id: 'A', history_authority: 'authority', section: 'messages', items: [message('old', 0)], next_cursor: 'oldest', complete: false }, 'A', 'messages'))
  state.cache.save({ ...key('A'), payload: loaded }); state.select('A')
  await state.read(Promise.resolve(raw('A', { history: bounded.history })))
  assert.deepEqual(state.shown()?.messages.map((item) => item.message_id), ['old', 'A'])
  assert.equal(state.shown()?.history.messages.next_cursor, 'oldest')
  state.reconnect(); state.select('A')
  await state.read(Promise.resolve(raw('A', { messages: [message('disjoint', 20)], history: { ...completeHistory(), messages: { next_cursor: 'gap', complete: false } } })))
  assert.equal(state.shown()?.history.messages.next_cursor, 'gap')
  assert.equal(state.shown()?.history.messages.complete, false)
  assert.equal(state.owner.observedMessage(message('old', 0)), false, 'retained cache rows cannot settle an outbox input')
  assert.equal(state.owner.observedMessage(message('disjoint', 20)), true)
  await state.read(Promise.resolve(raw('A', { history_authority: 'new-authority', messages: [message('new-authority')] })))
  assert.equal(state.authorityChanges(), 1)
  assert.deepEqual(state.shown()?.messages.map((item) => item.message_id), ['new-authority'])
  assert.equal(state.cache.read(key('A'))?.payload.history_authority, 'new-authority')
})

test('viewport acknowledgments require a visible exact event from this connection and never acknowledge questions', async () => {
  const owner = new ConversationReadOwner(), messages = [message('visible'), message('offscreen'), message('cached')]
  const notifications: NotificationRecord[] = [
    ...messages.map((item) => ({ notification_id: item.event_hash, conversation_id: 'A', kind: 'manager_response', text: '' })),
    { notification_id: 'event-visible', conversation_id: 'A', kind: 'question', text: 'answer me' },
    { notification_id: 'event-visible', conversation_id: 'B', kind: 'manager_response', text: '' },
  ]
  owner.confirm(messages.slice(0, 2))
  const acked: NotificationRecord[] = []
  const input = { owner, messageIds: ['visible', 'cached'], messages, notifications, conversationId: 'A', current: () => true, acknowledge: async (item: NotificationRecord) => { acked.push(item) } }
  await markVisibleMessagesRead(input)
  assert.deepEqual(acked.map((item) => [item.notification_id, item.kind]), [['event-visible', 'manager_response']])
  acked.length = 0
  await markVisibleMessagesRead({ ...input, current: () => false })
  await markVisibleMessagesRead({ ...input, messages: [{ ...message('visible'), event_hash: 'unread-replacement' }] })
  assert.deepEqual(acked, [])
  owner.invalidate()
  await markVisibleMessagesRead(input)
  assert.deepEqual(acked, [])
})

test('a foreground or route change during an acknowledgment stops the remaining viewport receipts', async () => {
  const owner = new ConversationReadOwner(), messages = [message('first'), message('second')], acked: string[] = []
  owner.confirm(messages)
  let foreground = true
  await markVisibleMessagesRead({ owner, messageIds: messages.map((item) => item.message_id), messages, notifications: messages.map((item) => ({ notification_id: item.event_hash, conversation_id: 'A', kind: 'manager_response', text: '' })), conversationId: 'A', current: () => foreground, acknowledge: async (item) => { acked.push(item.notification_id); foreground = false } })
  assert.deepEqual(acked, ['event-first'])
})
