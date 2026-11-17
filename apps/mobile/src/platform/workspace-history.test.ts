import assert from 'node:assert/strict'
import test from 'node:test'
import type { WorkspaceHistory } from '@vibeface/client/workspace/types.ts'
import { createWorkspaceSendActivity, prependWorkspaceHistory, refreshWorkspaceHistory, workspaceHistoryMatchesMessage } from './workspace-history.ts'

function page(ids: string[], cursor: string | null): WorkspaceHistory {
  return {
    events: ids.map(id => ({ event_hash: id, created_at_ms: 0, scope_id: 'conversation', kind: 'message_posted', speaker: { kind: 'bot', bot_id: 'bot' }, body: { type: 'message_posted', message_id: id, role: 'assistant', text: id } })),
    next_cursor: cursor, complete: cursor === null, coverage: { limit: 2, truncated: cursor !== null, scope: 'task' },
  }
}
const ids = (history: WorkspaceHistory | null) => history?.events.map(event => event.event_hash)

test('older pages preserve server order, remove overlaps and retain the oldest cursor', () => {
  const merged = prependWorkspaceHistory(page(['b', 'c'], 'older'), page(['a', 'b'], null))
  assert.deepEqual(ids(merged), ['a', 'b', 'c'])
  assert.equal(merged.complete, true)
  assert.equal(merged.next_cursor, null)
})

test('refresh bridges an interval larger than one page without discarding loaded older history', async () => {
  const cursors: string[] = []
  const current = page(['a', 'b'], 'oldest')
  const result = await refreshWorkspaceHistory({ current, first: page(['e', 'f'], 'middle'), readOlder: async cursor => { cursors.push(cursor); return page(['b', 'c', 'd'], 'overlap') }, isCurrent: () => true })
  assert.deepEqual(ids(result), ['a', 'b', 'c', 'd', 'e', 'f'])
  assert.deepEqual(cursors, ['middle'])
  assert.equal(result?.next_cursor, 'oldest')
  assert.equal(result?.complete, false)
})

test('a fully loaded refresh replaces invalidated events and updates completion', async () => {
  const result = await refreshWorkspaceHistory({ current: page(['removed', 'a'], 'old'), first: page(['a', 'b'], null), readOlder: async () => { throw new Error('unexpected') }, isCurrent: () => true })
  assert.deepEqual(ids(result), ['a', 'b'])
  assert.equal(result?.complete, true)
})

test('scope invalidation while awaiting a page never publishes an obsolete history', async () => {
  let active = true
  const result = await refreshWorkspaceHistory({ current: page(['a'], 'old'), first: page(['c'], 'bridge'), readOlder: async () => { active = false; return page(['a', 'b'], null) }, isCurrent: () => active })
  assert.equal(result, null)
})

test('invalid or non-advancing authority cursors fail for an explicit reload', async () => {
  await assert.rejects(refreshWorkspaceHistory({ current: page(['a'], 'old'), first: page(['c'], 'bridge'), readOlder: async () => { throw new Error('authority changed') }, isCurrent: () => true }), /authority changed/)
  await assert.rejects(refreshWorkspaceHistory({ current: page(['a'], 'old'), first: page(['c'], 'bridge'), readOlder: async () => page(['b'], 'bridge'), isCurrent: () => true }), /重新加载/)
})

test('send ownership blocks duplicate scopes, allows other scopes, and ignores stale release', () => {
  const activity = createWorkspaceSendActivity(), updates: number[] = []
  const unsubscribe = activity.subscribe(() => updates.push(updates.length))
  const original = activity.acquire('network/device/conversation/task')
  assert.ok(original)
  assert.equal(activity.acquire('network/device/conversation/task'), null)
  const other = activity.acquire('other-network/device/conversation/task')
  assert.ok(other)
  original.saving()
  assert.equal(activity.phase('network/device/conversation/task'), 'saving')
  original.release()
  const reopened = activity.acquire('network/device/conversation/task')
  assert.ok(reopened)
  original.release(); original.saving()
  assert.equal(activity.phase('network/device/conversation/task'), 'preparing')
  assert.equal(activity.phase('other-network/device/conversation/task'), 'preparing')
  reopened.release(); other.release(); unsubscribe()
  assert.equal(updates.length, 7)
})


test('search locations match actual message identity, including channel posts, without matching system work IDs', () => {
  const event = page(['hash'], null).events[0]
  assert.equal(workspaceHistoryMatchesMessage(event, 'hash'), true)
  assert.equal(workspaceHistoryMatchesMessage(event, ''), false)
  const channel = { ...event, kind: 'workspace_commanded', body: { type: 'workspace_commanded', command_id: 'command', command: { kind: 'post_channel_message', channel_id: 'channel', message_id: 'channel-message', text: 'quote', attachments: [] } } } satisfies WorkspaceHistory['events'][number]
  assert.equal(workspaceHistoryMatchesMessage(channel, 'channel-message'), true)
  assert.equal(workspaceHistoryMatchesMessage(channel, 'command'), false)
  const input = { ...event, kind: 'input_posted', body: { type: 'input_posted', message_id: 'input-message', target: { kind: 'task', task_id: 'task' }, intent: 'continue', sequence: 1, text: 'input', report_results: true } } satisfies WorkspaceHistory['events'][number]
  assert.equal(workspaceHistoryMatchesMessage(input, 'input-message'), true)
  assert.equal(workspaceHistoryMatchesMessage(input, 'task'), false)
  const work = { ...event, kind: 'work_finished', body: { type: 'work_finished', work_id: 'work', outcome: 'completed', result_text: 'result' } } satisfies WorkspaceHistory['events'][number]
  assert.equal(workspaceHistoryMatchesMessage(work, 'work'), false)
})
