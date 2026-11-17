import assert from 'node:assert/strict'
import test from 'node:test'
import { parseMessageSearch, parseWorkspaceHistory } from './model.ts'
import { readWorkspaceHistoryPage, searchMessages } from './history-search.ts'
import type { MessageSearchPage, MessageSearchScope } from './types.ts'

const task: MessageSearchScope = { kind: 'task', task_id: 'task-one' }
const channel: MessageSearchScope = { kind: 'channel', channel_id: 'channel' }
const query = '中文 %_"\''
const hash = 'a'.repeat(64)
function result(scope: MessageSearchScope = task): MessageSearchPage {
  return {
    scope, query,
    items: [{ event_hash: hash, message_id: 'message', conversation_id: 'chat', rank: `${'1'.padStart(20, '0')}:${'2'.padStart(20, '0')}:${hash}`, created_at_ms: 2, text: `${query}正文`, speaker: { kind: 'bot', bot_id: 'bot' } }],
    complete: false, next_cursor: 'opaque', coverage: { source: 'committed_server_history', limit: 2 },
  }
}

test('server search validates exact scope and literal Unicode query echoes with stable message positions', () => {
  const page = result()
  assert.deepEqual(parseMessageSearch(page, { scope: task, query }), page)
  assert.throws(() => parseMessageSearch(page, { scope: channel, query }), /范围不一致/)
  assert.throws(() => parseMessageSearch(page, { scope: task, query: 'other' }), /范围不一致/)
  assert.throws(() => parseMessageSearch({ ...page, scope: { ...task, bot_id: 'other' } }), /范围格式/)
  for (const item of [
    { ...page.items[0], text: 'hidden field matches only' },
    { ...page.items[0], event_hash: 'invalid' },
    { ...page.items[0], rank: 'invalid' },
    { ...page.items[0], speaker: { kind: 'bot', device_id: 'node' } },
  ]) assert.throws(() => parseMessageSearch({ ...page, items: [item] }))
  assert.throws(() => parseMessageSearch({ ...page, items: [page.items[0], page.items[0]] }), /重复或顺序/)
})

test('history and search reject incomplete or contradictory continuation metadata', () => {
  const history = { events: [], next_cursor: null, complete: true, coverage: { limit: 2, truncated: false, scope: 'task' } }
  assert.equal(parseWorkspaceHistory(history).complete, true)
  for (const fields of [
    { next_cursor: undefined }, { next_cursor: '' }, { next_cursor: 'x', complete: true },
    { next_cursor: null, complete: false }, { complete: undefined },
  ]) {
    assert.throws(() => parseWorkspaceHistory({ ...history, ...fields }))
    assert.throws(() => parseMessageSearch({ ...result(), ...fields }))
  }
  assert.throws(() => parseWorkspaceHistory({ ...history, coverage: { ...history.coverage, truncated: true } }))
  for (const limit of [0, 513, 1.5]) assert.throws(() => parseWorkspaceHistory({ ...history, coverage: { ...history.coverage, limit } }))
  for (const limit of [0, 101, 1.5]) assert.throws(() => parseMessageSearch({ ...result(), coverage: { source: 'committed_server_history', limit } }))
  const empty = parseMessageSearch({ ...result(), items: [], complete: true, next_cursor: null })
  assert.equal(empty.items.length, 0)
  assert.equal(empty.complete, true)
})

test('shared search API forwards explicit server scope and continuation and parses at the boundary', async () => {
  const requests: { method: string; params?: Record<string, unknown> }[] = []
  const connection = { rpc: async (method: string, params?: Record<string, unknown>): Promise<unknown> => {
    requests.push({ method, params })
    return result()
  } }
  assert.deepEqual(await searchMessages({ connection, scope: task, query, cursor: 'previous', limit: 2 }), result())
  assert.deepEqual(requests, [{ method: 'message_search', params: { scope: task, query, cursor: 'previous', limit: 2 } }])
  await assert.rejects(searchMessages({ connection, scope: channel, query, limit: 2 }), /范围不一致/)
})

test('workspace API keeps scope filters on every older page and rejects a different server scope', async () => {
  const requests: { method: string; params?: Record<string, unknown> }[] = []
  const connection = { rpc: async (method: string, params?: Record<string, unknown>): Promise<unknown> => {
    requests.push({ method, params })
    return { events: [], next_cursor: null, complete: true, coverage: { limit: 2, truncated: false, scope: 'task' } }
  } }
  assert.equal((await readWorkspaceHistoryPage({ connection, filter: { task_id: 'task-one', limit: 2 }, cursor: 'older' })).complete, true)
  assert.deepEqual(requests, [{ method: 'workspace_history', params: { task_id: 'task-one', limit: 2, cursor: 'older' } }])
  await assert.rejects(readWorkspaceHistoryPage({ connection, filter: { channel_id: 'channel', limit: 2 } }), /范围不一致/)
})
