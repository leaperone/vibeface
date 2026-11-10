import assert from 'node:assert/strict'
import test from 'node:test'
import { completeHistory } from './history-fixture.ts'
import { HistoryRequests, mergeConversationSnapshot, mergeHistoryPage, readConversationHistory } from './history.ts'
import { historySections, parseConversation, parseHistoryPage, parseRecords } from './model.ts'
import { mergeConversation } from './records.ts'
import type { HistoryPage, HistorySection } from './types.ts'

const message = (id: string, rank: number, text = id) => ({ message_id: id, rank: String(rank).padStart(20, '0'), role: 'user', text, event_hash: id, created_at_ms: rank })
const snapshot = (extra: Record<string, unknown> = {}) => parseConversation({
  conversation: { conversation_id: 'chat', owner: 'owner', owner_epoch: 1, title: '' }, history_authority: 'authority', history: completeHistory(),
  messages: [message('newest', 10)], works: [], deliveries: [], checks: [], local_node: 'entry', changes: [], overwritten: [],
  scheduler: { jobs: [], scope_jobs: [], unanswered: [] },
  bot_settings: { skills: [], schedules: [], memory: [], memory_conflicts: [], memory_sync_conflicts: { items: [], has_more: false } },
  sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [], sessions: [], ...extra,
}, 'chat')
const rawPage = (section: HistorySection, items: unknown[], next_cursor: string | null = null) => ({ conversation_id: 'chat', history_authority: 'authority', section, items, next_cursor, complete: next_cursor === null })
const page = (section: HistorySection, items: unknown[], cursor: string | null = null) => parseHistoryPage(rawPage(section, items, cursor), 'chat', section)
const bounded = () => snapshot({ history: { ...completeHistory(), messages: { next_cursor: 'before-newest', complete: false } } })

// A literal inventory catches a missing parser/control whenever the final protocol changes.
test('every snapshot section requires typed coverage and has a typed page parser', () => {
  assert.deepEqual(historySections, ['messages', 'works', 'changes', 'questions', 'sessions', 'jobs', 'scope_jobs', 'owner_jobs', 'owner_scope_jobs', 'owner_memory_conflicts', 'checks', 'deliveries', 'unanswered', 'skills', 'schedules', 'memory', 'memory_conflicts', 'memory_sync_conflicts', 'overwritten'])
  for (const section of historySections) {
    assert.equal(page(section, []).section, section)
    assert.throws(() => snapshot({ history: { ...completeHistory(), [section]: undefined } }))
  }
  assert.throws(() => snapshot({ history: { ...completeHistory(), invented: { next_cursor: null, complete: true } } }))
  assert.throws(() => snapshot({ history_authority: undefined }))
  assert.throws(() => page('messages', [{ message_id: 'missing-fields' }]))
  assert.throws(() => parseHistoryPage(rawPage('messages', []), 'other', 'messages'))
  assert.throws(() => parseHistoryPage(rawPage('messages', []), 'chat', 'works'))
  assert.throws(() => parseHistoryPage({ ...rawPage('messages', []), complete: false }, 'chat', 'messages'))
})

test('older pages merge in shared order, deduplicate stable IDs and cannot overwrite newer pushes', () => {
  const initial = bounded()
  const changed = mergeConversation(initial, parseRecords({ conversation_id: 'chat', history_authority: 'authority', refresh_metadata: true, messages: [message('newest', 10, 'pushed update')], works: [] }))
  const loaded = mergeHistoryPage(changed, page('messages', [message('old', 1), message('newest', 10, 'stale')], 'older'))
  assert.deepEqual(loaded.messages.map((item) => [item.message_id, item.text]), [['old', 'old'], ['newest', 'pushed update']])
  const repeated = mergeHistoryPage(loaded, page('messages', [message('old', 1), message('older', 0)]))
  assert.deepEqual(repeated.messages.map((item) => item.message_id), ['older', 'old', 'newest'])
  assert.equal(repeated.history.messages.complete, true)
  assert.throws(() => mergeHistoryPage(loaded, { ...page('messages', []), history_authority: 'fork' }))
})

test('refresh preserves loaded rows and cursor while protecting pushes that arrived during its read', () => {
  const initial = bounded()
  const loaded = mergeHistoryPage(initial, page('messages', [message('old', 1)], 'older'))
  const pushed = mergeConversation(loaded, parseRecords({ conversation_id: 'chat', history_authority: 'authority', refresh_metadata: true, messages: [message('newest', 10, 'changed after refresh began'), message('live', 11)], works: [] }))
  const refreshed = mergeConversationSnapshot(pushed, bounded(), loaded)
  assert.deepEqual(refreshed.messages.map((item) => [item.message_id, item.text]), [['old', 'old'], ['newest', 'changed after refresh began'], ['live', 'live']])
  assert.equal(refreshed.history.messages.next_cursor, 'older')
  const fork = snapshot({ history_authority: 'fork', messages: [message('fork-only', 2)] })
  assert.deepEqual(mergeConversationSnapshot(refreshed, fork, refreshed).messages.map((item) => item.message_id), ['fork-only'])
})

test('a disjoint refreshed window exposes its gap instead of claiming the loaded history is complete', () => {
  const initial = snapshot({ messages: [message('old', 1)] })
  const refreshed = mergeConversationSnapshot(initial, bounded(), initial)
  assert.deepEqual(refreshed.messages.map((item) => item.message_id), ['old', 'newest'])
  assert.equal(refreshed.history.messages.next_cursor, 'before-newest')
})

test('sessions keep separate devices, and conflict records keep duplicate occurrences by history_id', () => {
  const session = { node: 'node-a', agent: 'codex', session_id: 'session', cwd: '/', summary: '', muted: false, created_at_ms: 1, rank: 'rank' }
  const conflict = { history_id: 'conflict-a', call_ref: 'remember', memory_id: 'memory', expected_epoch: 1, actual_epoch: 1, expected_revision: '', actual_revision: '', reason: 'changed' }
  let loaded = mergeHistoryPage(snapshot(), page('sessions', [session, { ...session, node: 'node-b' }]))
  loaded = mergeHistoryPage(loaded, page('memory_conflicts', [conflict, { ...conflict, history_id: 'conflict-b' }]))
  loaded = mergeHistoryPage(loaded, page('memory_conflicts', [conflict]))
  assert.deepEqual(loaded.sessions.map((item) => item.node), ['node-a', 'node-b'])
  assert.deepEqual(loaded.bot_settings.memory_conflicts?.map((item) => item.history_id), ['conflict-a', 'conflict-b'])
  assert.throws(() => page('memory_conflicts', [{ ...conflict, history_id: undefined }]))
})

test('one explicit read advances empty incomplete pages and stops on a visible page without fetching all history', async () => {
  const calls: Record<string, unknown>[] = [], received: HistoryPage[] = []
  const replies = [rawPage('messages', [], 'second'), rawPage('messages', [], 'third'), rawPage('messages', [message('old', 1)], 'fourth')]
  await readConversationHistory({ conversationId: 'chat', section: 'messages', cursor: 'first', authority: 'authority', current: () => true, apply: (value) => received.push(value), rpc: async (method, params) => {
    assert.equal(method, 'conversation_history'); calls.push(params)
    const reply = replies.shift(); assert.ok(reply); return reply
  } })
  assert.deepEqual(calls, ['first', 'second', 'third'].map((cursor) => ({ conversation_id: 'chat', section: 'messages', cursor, limit: 50 })))
  assert.deepEqual(received.map((item) => item.next_cursor), ['second', 'third', 'fourth'])
})

test('cancelled requests and prior connection visits cannot apply late results or erase newer requests', async () => {
  const requests = new HistoryRequests(), old = requests.start('messages'); assert.ok(old)
  assert.equal(requests.start('messages'), null)
  let release: (value: unknown) => void = () => {}, applied = 0
  const result = readConversationHistory({ conversationId: 'chat', section: 'messages', cursor: 'first', authority: 'authority', current: old.current, apply: () => { applied++ }, rpc: () => new Promise((resolve) => { release = resolve }) })
  requests.reset()
  const next = requests.start('messages'); assert.ok(next)
  release(rawPage('messages', [message('old', 1)])); await result
  old.finish()
  assert.equal(applied, 0)
  assert.equal(next.current(), true)
  requests.cancel('messages')
  assert.equal(next.current(), false)
})

test('history reads reject authority changes and nonadvancing cursors before applying a row', async () => {
  for (const reply of [{ ...rawPage('messages', []), history_authority: 'fork' }, rawPage('messages', [], 'first')]) {
    await assert.rejects(readConversationHistory({ conversationId: 'chat', section: 'messages', cursor: 'first', authority: 'authority', current: () => true, apply: () => assert.fail('invalid page applied'), rpc: async () => reply }))
  }
})
