import type { KeyValue, NotificationDestination } from '@vibeface/client/lib/platform.ts'

type Opening = { key: string; destination: NotificationDestination }
const storageKey = 'notification-openings'

function destination(value: unknown): NotificationDestination | null {
  if (!value || typeof value !== 'object' || !('network_id' in value) || !('device_id' in value) || !('notification_id' in value) || !('conversation_id' in value)) return null
  const { network_id, device_id, notification_id, conversation_id } = value
  if (typeof network_id !== 'string' || typeof device_id !== 'string' || typeof notification_id !== 'string' || typeof conversation_id !== 'string') return null
  if (![network_id, device_id, notification_id, conversation_id].every(value => value.length > 0 && value.length <= 256)) return null
  return { network_id, device_id, notification_id, conversation_id }
}

/** Cold launches and listener replays feed the same durable queue. Only the current workspace can consume it. */
export function notificationOpenings(storage: KeyValue) {
  let listener: ((target: NotificationDestination) => Promise<boolean>) | null = null
  let epoch = 0
  let running = false
  let again = false
  let pending: Opening[] = []
  let completed: string[] = []
  try {
    const saved: unknown = JSON.parse(storage.getItem(storageKey) ?? '{}')
    if (saved && typeof saved === 'object' && 'pending' in saved && Array.isArray(saved.pending)) for (const item of saved.pending) {
      if (!item || typeof item !== 'object' || !('key' in item) || typeof item.key !== 'string' || !('destination' in item)) continue
      const target = destination(item.destination)
      if (target) pending.push({ key: item.key, destination: target })
    }
    if (saved && typeof saved === 'object' && 'completed' in saved && Array.isArray(saved.completed)) {
      completed = saved.completed.filter((key): key is string => typeof key === 'string').slice(-1000)
    }
  } catch { /* Malformed local state cannot supply a navigation target. */ }
  const persist = () => storage.setItem(storageKey, JSON.stringify({ pending, completed }))
  async function flush() {
    if (running) { again = true; return }
    running = true
    const version = epoch
    try {
      for (const item of [...pending]) {
        const consumer = listener
        if (!consumer) break
        let consumed = false
        try { consumed = await consumer(item.destination) } catch {}
        if (version !== epoch) break
        if (!consumed) continue
        pending = pending.filter(candidate => candidate.key !== item.key)
        completed = [...completed, item.key].slice(-1000)
        persist()
      }
    } finally {
      running = false
      if (again) { again = false; void flush() }
    }
  }
  return {
    receive(key: string, data: unknown) {
      const target = destination(data)
      if (!target || completed.includes(key) || pending.some(item => item.key === key)) return
      pending.push({ key, destination: target })
      persist()
      void flush()
    },
    watch(consumer: (target: NotificationDestination) => Promise<boolean>) {
      listener = consumer
      void flush()
      return () => { if (listener === consumer) listener = null }
    },
    flush,
    clear() { epoch++; listener = null; pending = []; completed = []; persist() },
  }
}
