import type { NotificationRecord } from '@vibeface/client/workspace/types.ts'

export function foregroundInbox() {
  let previous: Set<string> | null = null
  let interrupted: string | undefined
  return (state: string | null, items: readonly NotificationRecord[], eligible: (item: NotificationRecord) => boolean) => {
    const seen = previous
    previous = new Set(items.map(item => item.notification_id))
    if (!seen || state === 'background') return null
    const latest = items.findLast(item => (!seen.has(item.notification_id) || item.notification_id === interrupted)
      && ['question', 'manager_attention', 'manager_response', 'bot_message'].includes(item.kind) && eligible(item)) ?? null
    interrupted = state === 'active' ? undefined : latest?.notification_id
    return state === 'active' ? latest : null
  }
}

export function notificationOnScreen(item: NotificationRecord, path: string, routine: string | undefined, botId: string | undefined) {
  const target = item.target
  if (target?.kind === 'task' || target?.kind === 'channel_task') return path === '/task/' + target.task_id
  if (target?.kind === 'channel_member') return path === '/channel/' + target.channel_id
  if (!botId) return false
  if (target?.kind === 'work') return path === '/b/' + botId + '/work/' + target.work_id
  return path === '/b/' + botId && (target?.kind === 'routine' ? routine === target.trigger : !routine)
}
