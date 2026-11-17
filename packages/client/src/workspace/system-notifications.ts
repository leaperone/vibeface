import type { KeyValue, Notifier } from '../lib/platform.ts'
import type { NotificationRecord } from './types.ts'
import { networkKey, storageOf } from './storage.ts'

export type NotificationPreferences = { enabled: boolean; sound: boolean; badge: boolean; mutedConversations: string[] }

export function notificationPreferences(storage: KeyValue, networkId: string) {
  const { readStorage, writeStorage } = storageOf(storage)
  const key = networkKey('notification-preferences', networkId)
  const read = (): NotificationPreferences => {
    const value = readStorage<unknown>(key, null)
    const settings = value && typeof value === 'object' ? value : {}
    return {
      enabled: readStorage<unknown>(networkKey('notify', networkId), false) === true,
      sound: 'sound' in settings && settings.sound === true,
      badge: 'badge' in settings && settings.badge === true,
      mutedConversations: 'mutedConversations' in settings && Array.isArray(settings.mutedConversations)
        ? settings.mutedConversations.filter((id): id is string => typeof id === 'string' && id.length > 0 && id.length <= 512).slice(-200) : [],
    }
  }
  return {
    read,
    write(value: NotificationPreferences) {
      if (!networkId) throw new Error('先加入网络，再设置提醒。')
      writeStorage(key, { sound: value.sound, badge: value.badge, mutedConversations: [...new Set(value.mutedConversations)].slice(-200) }, true)
      writeStorage(networkKey('notify', networkId), value.enabled, true)
      return read()
    },
  }
}

export function systemNotifications(input: { storage: KeyValue; networkId: string; notifier: Notifier }) {
  const { readStorage, writeStorage } = storageOf(input.storage)
  const seenKey = networkKey('seen', input.networkId)
  const seen = () => {
    const stored = readStorage<unknown>(seenKey, [])
    return new Set(Array.isArray(stored) ? stored.filter((id): id is string => typeof id === 'string') : [])
  }
  let tail: Promise<void> = Promise.resolve()
  return {
    deliver(request: {
      items: NotificationRecord[]
      current(): boolean
      visible(item: NotificationRecord): boolean
      eligible(item: NotificationRecord): boolean
      show(item: NotificationRecord): ReturnType<Notifier['show']>
    }) {
      const work = tail.catch(() => {}).then(async () => {
        const preferences = notificationPreferences(input.storage, input.networkId).read()
        if (!preferences.enabled || !request.current() || await input.notifier.permission() !== 'granted') return
        const delivered = seen()
        for (const item of request.items) {
          if (!request.current()) return
          if (delivered.has(item.notification_id) || !request.eligible(item)) continue
          if (!request.visible(item) && !preferences.mutedConversations.includes(item.conversation_id)) {
            await request.show(item)
          }
          delivered.add(item.notification_id)
          writeStorage(seenKey, [...new Set([...seen(), ...delivered])].slice(-1000), true)
        }
      })
      tail = work
      return work
    },
  }
}
