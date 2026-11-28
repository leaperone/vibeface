import type { ConversationPayload } from '../workspace/types.ts'

/** Execution metadata can exist before a routine has an original result or question notification. */
export function routineState(data: ConversationPayload | null, trigger: string, connected: boolean): string | null {
  const job = [...data?.scopeJobs ?? []].reverse().find(job => job.scope.kind === 'routine' && job.scope.trigger === trigger)
  if (!job) return null
  if (!connected || data?.scheduler?.remote?.reachable === false || ['unknown', 'unverified', 'reconciling'].includes(job.state)) return '状态待确认'
  if (job.state === 'completed') return '本轮已结束'
  if (['failed', 'rejected'].includes(job.state)) return '本轮未完成'
  if (['cancelled', 'interrupted'].includes(job.state)) return '本轮已停止'
  return '尚未结束'
}
