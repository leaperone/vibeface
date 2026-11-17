import { unreadReply } from '@vibeface/client/chat/timeline.ts'
import type { NotificationRecord, WorkspaceHistoryItem } from '@vibeface/client/workspace/types.ts'

export type MessageRowLayout = { y: number; height: number }
export type MessageViewport = { top: number; height: number }

/** Require half of the row, or half of the viewport for a message taller than the screen. */
export function visibleWorkspaceNotifications(input: {
  events: readonly WorkspaceHistoryItem[]
  rows: ReadonlyMap<string, MessageRowLayout>
  viewport: MessageViewport
  notifications: readonly NotificationRecord[]
}): NotificationRecord[] {
  const { top, height } = input.viewport
  if (!Number.isFinite(top) || !Number.isFinite(height) || height <= 0) return []
  const visible = new Set<string>()
  for (const event of input.events) {
    const body = event.body
    if (body.type !== 'input_posted' && body.type !== 'message_posted' && !(body.type === 'workspace_commanded' && body.command.kind === 'post_channel_message')) continue
    const row = input.rows.get(event.event_hash)
    if (!row || !Number.isFinite(row.y) || !Number.isFinite(row.height) || row.height <= 0) continue
    const overlap = Math.min(top + height, row.y + row.height) - Math.max(top, row.y)
    if (overlap >= Math.min(row.height, height) / 2) visible.add(event.event_hash)
  }
  return input.notifications.filter(notification => unreadReply(notification) && visible.has(notification.notification_id))
}

type Observation = {
  scope: string
  notifications: readonly NotificationRecord[]
  current(): boolean
  acknowledge(notification: NotificationRecord): Promise<void>
}
const identity = (scope: string, notification: NotificationRecord) => JSON.stringify([scope, notification.conversation_id, notification.notification_id])
const fingerprint = (notification: NotificationRecord) => JSON.stringify([notification.kind, notification.text, notification.target])

/** Dwell and signed-ack ownership live outside scroll callbacks; callbacks only update the observation. */
export function createViewportRead(options: {
  dwellMs?: number
  now?: () => number
  schedule?: (work: () => void, delay: number) => () => void
} = {}) {
  const dwell = options.dwellMs ?? 600, now = options.now ?? Date.now
  const schedule = options.schedule ?? ((work, delay) => { const timer = setTimeout(work, delay); return () => clearTimeout(timer) })
  let observation: Observation | null = null, version = 0, running = false
  let cancelTimer: (() => void) | null = null, deadline: number | null = null
  const since = new Map<string, number>(), fingerprints = new Map<string, string>()
  const attempted = new Set<string>(), finished = new Set<string>(), inFlight = new Set<string>()
  function cancel() { cancelTimer?.(); cancelTimer = null; deadline = null }
  function invalidate() {
    version++; observation = null; since.clear(); fingerprints.clear(); attempted.clear(); cancel()
  }
  function plan() {
    const read = observation
    if (!read || !read.current()) { cancel(); return }
    let next = Infinity
    for (const notification of read.notifications) {
      const key = identity(read.scope, notification), started = since.get(key)
      if (started !== undefined && !attempted.has(key) && !finished.has(key) && !inFlight.has(key)) next = Math.min(next, started + dwell)
    }
    if (!Number.isFinite(next)) { cancel(); return }
    if (deadline === next) return
    cancel(); deadline = next
    cancelTimer = schedule(() => { cancelTimer = null; deadline = null; void flush() }, Math.max(0, next - now()))
  }
  async function flush() {
    if (running) return
    running = true
    const startedVersion = version
    try {
      const initial = observation
      if (!initial || !initial.current()) return
      for (const notification of initial.notifications) {
        const read = observation
        if (startedVersion !== version || !read || read.scope !== initial.scope || !read.current()) return
        const key = identity(read.scope, notification), started = since.get(key)
        if (!read.notifications.some(candidate => identity(read.scope, candidate) === key) || started === undefined || now() - started < dwell || attempted.has(key) || finished.has(key) || inFlight.has(key)) continue
        attempted.add(key); inFlight.add(key)
        try {
          // This is the final check immediately before each signed operation, including after a prior await.
          if (startedVersion !== version || !read.current()) return
          await read.acknowledge(notification)
          finished.add(key)
        } catch { /* Retry only after visibility or notification content changes; never spin a signed-write loop. */ }
        finally { inFlight.delete(key) }
      }
    } finally { running = false; plan() }
  }
  return {
    invalidate, flush,
    observe(next: Observation) {
      if (!next.current()) { invalidate(); return }
      if (observation && observation.scope !== next.scope) invalidate()
      observation = next
      const visible = new Set(next.notifications.map(notification => identity(next.scope, notification)))
      for (const key of since.keys()) if (!visible.has(key)) { since.delete(key); fingerprints.delete(key); attempted.delete(key) }
      for (const notification of next.notifications) {
        const key = identity(next.scope, notification), content = fingerprint(notification)
        if (!since.has(key)) since.set(key, now())
        if (fingerprints.get(key) !== content) attempted.delete(key)
        fingerprints.set(key, content)
      }
      plan()
    },
  }
}
