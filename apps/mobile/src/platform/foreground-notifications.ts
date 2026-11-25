import type { NotificationRecord } from '@vibeface/client/workspace/types.ts'

export function latestBanner(previous: ReadonlySet<string> | null, items: readonly NotificationRecord[], eligible: (item: NotificationRecord) => boolean) {
  if (!previous) return null
  return items.findLast(item => !previous.has(item.notification_id) && eligible(item)
    && ['question', 'manager_attention', 'manager_response', 'bot_message'].includes(item.kind)) ?? null
}

export function notificationOnScreen(item: NotificationRecord, path: string, routine: string | undefined, botId: string | undefined) {
  const target = item.target
  if (target?.kind === 'task' || target?.kind === 'channel_task') return path === '/task/' + target.task_id
  if (target?.kind === 'channel_member') return path === '/channel/' + target.channel_id
  if (!botId) return false
  if (target?.kind === 'work') return path === '/b/' + botId + '/work/' + target.work_id
  return path === '/b/' + botId && (target?.kind === 'routine' ? routine === target.trigger : !routine)
}
