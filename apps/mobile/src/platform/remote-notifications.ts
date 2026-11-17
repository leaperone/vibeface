import Constants from 'expo-constants'
import * as Notifications from 'expo-notifications'
import type { ReadyConnection } from '@vibeface/client/lib/node.ts'
import type { NotificationPreferences } from '@vibeface/client/workspace/system-notifications.ts'

export type RemoteNotificationState =
  | { kind: 'off' }
  | { kind: 'unconfigured' }
  | { kind: 'registered' }
  | { kind: 'error'; message: string }

const states = new Map<string, RemoteNotificationState>()
const listeners = new Set<() => void>()
const work = new Map<string, Promise<RemoteNotificationState>>()
const erasing = new Set<string>()
const keyOf = (connection: ReadyConnection) => JSON.stringify([connection.principal.network_id, connection.principal.device_id, connection.entry.device_id])
function publish(key: string, value: RemoteNotificationState) {
  states.set(key, value)
  for (const listener of listeners) listener()
  return value
}
function projectId(): string | null {
  const value: unknown = Constants.expoConfig?.extra?.eas?.projectId ?? Constants.easConfig?.projectId
  return typeof value === 'string' && /^[a-f\d]{8}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{12}$/i.test(value) ? value : null
}

export const remoteNotifications = {
  state(connection: ReadyConnection): RemoteNotificationState { return states.get(keyOf(connection)) ?? { kind: 'off' } },
  watch(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener) } },
  sync(connection: ReadyConnection, preferences: NotificationPreferences) {
    const key = keyOf(connection)
    const previous = work.get(key) ?? Promise.resolve<RemoteNotificationState>({ kind: 'off' })
    const next = previous.catch(() => ({ kind: 'off' as const })).then(async (): Promise<RemoteNotificationState> => {
      if (!connection.isCurrent() || preferences.enabled && erasing.has(key)) return { kind: 'off' }
      try {
        if (!preferences.enabled || (await Notifications.getPermissionsAsync()).status !== 'granted') {
          await connection.rpc('unregister_push')
          return publish(key, { kind: 'off' })
        }
        const project = projectId()
        if (!project) return publish(key, { kind: 'unconfigured' })
        const token = await Notifications.getExpoPushTokenAsync({ projectId: project })
        if (!connection.isCurrent() || erasing.has(key)) return { kind: 'off' }
        await connection.rpc('register_push', { token: token.data, sound: preferences.sound, badge: preferences.badge, muted_conversations: preferences.mutedConversations })
        return publish(key, { kind: 'registered' })
      } catch {
        return publish(key, { kind: 'error', message: '后台提醒还没连上。检查网络后重试。' })
      }
    })
    work.set(key, next)
    void next.finally(() => { if (work.get(key) === next) work.delete(key) })
    return next
  },
  async prepareErase(connection: ReadyConnection) {
    const key = keyOf(connection)
    erasing.add(key)
    const state = await remoteNotifications.sync(connection, { enabled: false, sound: false, badge: false, mutedConversations: [] })
    if (state.kind === 'error') {
      erasing.delete(key)
      throw new Error('旧设备的后台提醒尚未关闭。请检查网络后重试清除。')
    }
  },
}
