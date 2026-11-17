import assert from 'node:assert/strict'
import test from 'node:test'
import type { NotificationRecord, WorkspaceHistoryItem } from '@vibeface/client/workspace/types.ts'
import { createViewportRead, visibleWorkspaceNotifications } from './viewport-read.ts'

const note = (id: string, kind = 'manager_response'): NotificationRecord => ({ notification_id: id, conversation_id: 'conversation', kind, text: id })
const event = (id: string): WorkspaceHistoryItem => ({ event_hash: id, created_at_ms: 1, scope_id: 'conversation', kind: 'message_posted', speaker: { kind: 'bot', bot_id: 'bot' }, body: { type: 'message_posted', message_id: `message-${id}`, role: 'assistant', text: id } })
function harness() {
  let time = 0
  const timers: { cancelled: boolean; delay: number; work(): void }[] = []
  const reader = createViewportRead({ now: () => time, schedule(work, delay) { const timer = { work, delay, cancelled: false }; timers.push(timer); return () => { timer.cancelled = true } } })
  return { reader, timers, at(value: number) { time = value } }
}

test('only proven ordinary message events substantially inside the viewport can select notifications', () => {
  const channel: WorkspaceHistoryItem = { ...event('channel'), kind: 'workspace_commanded', body: { type: 'workspace_commanded', command_id: 'command', command: { kind: 'post_channel_message', channel_id: 'channel', message_id: 'post', text: 'message', attachments: [] } } }
  const input: WorkspaceHistoryItem = { ...event('input'), kind: 'input_posted', body: { type: 'input_posted', message_id: 'input-message', target: { kind: 'task', task_id: 'task' }, intent: 'continue', sequence: 1, text: 'input', report_results: true } }
  const work: WorkspaceHistoryItem = { ...event('work'), kind: 'work_finished', body: { type: 'work_finished', work_id: 'work', outcome: 'completed', result_text: 'result' } }
  const notifications = [note('visible'), note('edge'), note('below'), note('tall'), note('channel', 'bot_message'), note('question', 'question'), note('unknown'), note('work'), note('attention', 'manager_attention'), note('input')]
  const rows = new Map(['visible', 'tall', 'channel', 'question', 'unknown', 'work', 'attention', 'input'].map(id => [id, { y: 100, height: id === 'tall' ? 1200 : 100 }]))
  rows.set('edge', { y: 590, height: 100 }); rows.set('below', { y: 700, height: 100 })
  const selected = visibleWorkspaceNotifications({ events: [event('visible'), event('edge'), event('below'), event('tall'), channel, event('question'), work, event('attention'), input], rows, viewport: { top: 100, height: 500 }, notifications })
  assert.deepEqual(selected.map(item => item.notification_id), ['visible', 'tall', 'channel', 'attention', 'input'])
  assert.deepEqual(visibleWorkspaceNotifications({ events: [event('visible')], rows, viewport: { top: 0, height: 0 }, notifications }), [])
})

test('600ms dwell survives duplicate scroll callbacks without repeated scheduling or acknowledgment', async () => {
  const { reader, timers, at } = harness(), calls: string[] = []
  const observation = { scope: 'network/device/generation/task', notifications: [note('a')], current: () => true, acknowledge: async (item: NotificationRecord) => { calls.push(item.notification_id) } }
  reader.observe(observation); at(300); reader.observe(observation)
  assert.equal(timers.length, 1)
  await reader.flush(); assert.deepEqual(calls, [])
  at(600); await reader.flush(); reader.observe(observation); await reader.flush()
  assert.deepEqual(calls, ['a'])
})

test('leaving visibility resets the dwell and excludes old rows from a later callback', async () => {
  const { reader, at } = harness(), calls: string[] = []
  const base = { scope: 'scope', current: () => true, acknowledge: async (item: NotificationRecord) => { calls.push(item.notification_id) } }
  reader.observe({ ...base, notifications: [note('old')] }); at(500)
  reader.observe({ ...base, notifications: [] }); at(700)
  reader.observe({ ...base, notifications: [note('new')] }); at(1000)
  await reader.flush(); assert.deepEqual(calls, [])
  at(1300); await reader.flush(); assert.deepEqual(calls, ['new'])
})

test('blur/background/generation invalidation cancels dwell and stops after an awaited acknowledgment', async () => {
  const { reader, at } = harness(), calls: string[] = []
  let current = true, release = () => {}
  const pending = new Promise<void>(resolve => { release = resolve })
  reader.observe({ scope: 'old-generation', notifications: [note('a'), note('b')], current: () => current, acknowledge: async item => { calls.push(item.notification_id); await pending } })
  at(600); const first = reader.flush()
  assert.deepEqual(calls, ['a'])
  current = false; reader.invalidate(); release(); await first
  assert.deepEqual(calls, ['a'])
  reader.observe({ scope: 'new-generation', notifications: [note('b')], current: () => true, acknowledge: async item => { calls.push(item.notification_id) } })
  at(1199); await reader.flush(); assert.deepEqual(calls, ['a'])
  at(1200); await reader.flush(); assert.deepEqual(calls, ['a', 'b'])
})

test('concurrent callbacks share one in-flight acknowledgment and do not acknowledge rows scrolled away during await', async () => {
  const { reader, at } = harness(), calls: string[] = []
  let release = () => {}
  const pending = new Promise<void>(resolve => { release = resolve })
  const base = { scope: 'scope', current: () => true, acknowledge: async (item: NotificationRecord) => { calls.push(item.notification_id); await pending } }
  reader.observe({ ...base, notifications: [note('a'), note('b')] }); at(600)
  const first = reader.flush(); await reader.flush()
  reader.observe({ ...base, notifications: [note('a')] }); release(); await first
  assert.deepEqual(calls, ['a'])
})

test('failed automatic acknowledgments retry only after visibility or notification content changes', async () => {
  const { reader, at } = harness()
  let calls = 0
  const base = { scope: 'scope', current: () => true, acknowledge: async () => { calls++; throw new Error('offline') } }
  reader.observe({ ...base, notifications: [note('a')] }); at(600)
  await reader.flush(); reader.observe({ ...base, notifications: [note('a')] }); await reader.flush()
  assert.equal(calls, 1)
  reader.observe({ ...base, notifications: [{ ...note('a'), text: 'updated' }] }); await reader.flush()
  assert.equal(calls, 2)
  reader.observe({ ...base, notifications: [] }); reader.observe({ ...base, notifications: [note('a')] }); at(1200); await reader.flush()
  assert.equal(calls, 3)
})
