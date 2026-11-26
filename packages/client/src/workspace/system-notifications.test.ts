import assert from 'node:assert/strict'
import test from 'node:test'
import type { Notifier } from '../lib/platform.ts'
import { parseNotificationContent, parseNotifications } from './model.ts'
import type { NotificationRecord } from './types.ts'
import { notificationBadgeCount, notificationPreview, notificationPreferences, notificationUnreadCounts, systemNotifications, watchNotificationPreferences } from './system-notifications.ts'

const fixture = () => {
  const values = new Map<string, string>()
  const storage = { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => { values.set(key, value) } }
  const notifier: Notifier = { name: 'test', permission: async () => 'granted', request: async () => 'granted', show() {} }
  notificationPreferences(storage, 'network').write({ enabled: true, sound: false, badge: false, mutedConversations: [] })
  return { storage, notifier }
}
const item = { notification_id: 'notification', conversation_id: 'chat', kind: 'reply', text: 'reply' }

test('failed asynchronous notification scheduling is retried instead of being persisted as delivered', async () => {
  const context = fixture(), delivery = systemNotifications({ ...context, networkId: 'network' })
  let attempts = 0
  const request = { items: [item], current: () => true, visible: () => false, eligible: () => true, show: async () => { attempts++; if (attempts === 1) throw new Error('OS rejected scheduling') } }
  await assert.rejects(delivery.deliver(request), /OS rejected/)
  await delivery.deliver(request)
  await delivery.deliver(request)
  assert.equal(attempts, 2)
})

test('overlapping refreshes schedule each notification once and muted conversations remain quiet', async () => {
  const context = fixture(), delivery = systemNotifications({ ...context, networkId: 'network' })
  let attempts = 0
  const request = { items: [item], current: () => true, visible: () => false, eligible: () => true, show: async () => { attempts++; await Promise.resolve() } }
  await Promise.all([delivery.deliver(request), delivery.deliver(request)])
  assert.equal(attempts, 1)
  notificationPreferences(context.storage, 'network').write({ enabled: true, sound: true, badge: true, mutedConversations: ['chat'] })
  await delivery.deliver({ ...request, items: [{ ...item, notification_id: 'muted' }] })
  assert.equal(attempts, 1)
  assert.equal(notificationPreferences(context.storage, 'other').read().enabled, false)
})

test('an obsolete connection cannot schedule alerts after a permission read', async () => {
  const context = fixture()
  let active = true, attempts = 0
  context.notifier.permission = async () => { active = false; return 'granted' }
  await systemNotifications({ ...context, networkId: 'network' }).deliver({ items: [item], current: () => active, visible: () => false, eligible: () => true, show: async () => { attempts++ } })
  assert.equal(attempts, 0)
})

test('enabling badges or muting a Bot immediately recomputes existing unread counts', () => {
  const context = fixture(), preferences = notificationPreferences(context.storage, 'network')
  const unread = { first: 2, second: 5 }, counts: number[] = []
  const stop = watchNotificationPreferences('network', () => counts.push(notificationBadgeCount(unread, preferences.read())))
  preferences.write({ enabled: true, sound: false, badge: true, mutedConversations: [] })
  preferences.write({ ...preferences.read(), mutedConversations: ['second'] })
  preferences.write({ ...preferences.read(), enabled: false })
  notificationPreferences(context.storage, 'other').write({ enabled: true, sound: true, badge: true, mutedConversations: [] })
  stop()
  assert.deepEqual(counts, [7, 2, 0])
})

test('app badge includes unread task and channel messages and excludes non-push notices', () => {
  const records: NotificationRecord[] = [
    { ...item, notification_id: 'task-reply', conversation_id: 'task-bot', kind: 'manager_response', target: { kind: 'task', task_id: 'task' } },
    { ...item, notification_id: 'channel-reply', conversation_id: 'channel-bot', kind: 'bot_message' },
    { ...item, notification_id: 'question', conversation_id: 'task-bot', kind: 'question' },
    { ...item, notification_id: 'session', kind: 'session_attributed' },
    { ...item, notification_id: 'work', kind: 'work_finished' },
  ]
  const counts = notificationUnreadCounts(records)
  assert.deepEqual(counts, { 'task-bot': 2, 'channel-bot': 1 })
  assert.equal(notificationBadgeCount(counts, { enabled: true, badge: true, sound: false, mutedConversations: [] }), 3)
  assert.equal(notificationBadgeCount(counts, { enabled: true, badge: true, sound: false, mutedConversations: ['task-bot'] }), 1)
})


test('lock-screen previews never copy private result content', () => {
  const secret = { ...item, text: '/private/project/report.txt: confidential log and attachment' }
  assert.equal(notificationPreview(secret), 'Bot 有新进展，打开查看')
  assert.equal(notificationPreview({ ...secret, kind: 'question' }), 'Bot 有一个问题需要你查看')
})


test('notification focus is validated and retains its original question identity', () => {
  const notification = { ...item, kind: 'question', focus: 'question:original', target: { kind: 'work', work_id: 'original-work' } }
  assert.deepEqual(parseNotifications({ notifications: [notification] }), [notification])
  for (const focus of ['', {}, false, 'x'.repeat(1025)]) assert.throws(() => parseNotifications({ notifications: [{ ...notification, focus }] }))
})


test('precise routine content requires the original notice conversation and target', () => {
  const target = { kind: 'routine' as const, bot_id: 'bot', trigger: 'schedule:check:1000' }
  const expected = { notification_id: 'notice', conversation_id: 'chat', target }
  const question = { question_id: 'q-original', conversation_id: 'chat', target, bot: { bot_id: 'bot', name: 'Bot' }, severity: 'attention', text: '原例行问题', choices: [], about: { node: 'node', agent: 'codex', session_id: 'session', cwd: '', summary: '' }, author: 'node', created_at_ms: 1, answer: null, closed: null }
  const response = { notifications: [{ ...item, ...expected, kind: 'question', focus: 'question:q-original' }], record: { kind: 'question', question } }
  assert.equal(parseNotificationContent(response, expected).kind, 'question')
  assert.throws(() => parseNotificationContent(response, { ...expected, conversation_id: 'foreign' }))
  assert.throws(() => parseNotificationContent(response, { ...expected, notification_id: 'other' }))
  assert.throws(() => parseNotificationContent(response, { ...expected, target: { ...target, trigger: 'schedule:other:1000' } }))
  assert.throws(() => parseNotificationContent({ ...response, record: { kind: 'question', question: { ...question, target: { kind: 'bot_main', bot_id: 'bot' } } } }, expected))
  const result = { ...response, notifications: [{ ...response.notifications[0], kind: 'manager_response', focus: 'result' }], record: { kind: 'message', target, message: { message_id: 'result', event_hash: 'event', created_at_ms: 1, role: 'assistant', text: '例行结果' } } }
  assert.equal(parseNotificationContent(result, expected).kind, 'message')
})
