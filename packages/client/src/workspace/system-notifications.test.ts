import assert from 'node:assert/strict'
import test from 'node:test'
import type { Notifier } from '../lib/platform.ts'
import { notificationBadgeCount, notificationPreferences, systemNotifications, watchNotificationPreferences } from './system-notifications.ts'

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
