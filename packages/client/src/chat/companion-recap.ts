import { inputTargetKey } from './protocol.ts'
import { chatUpdates } from './timeline.ts'
import { taskActive } from '../workspace/labels.ts'
import type { InboxItem } from '../workspace/inbox.ts'
import type { BotRecord, ConversationPayload, InputTarget, NotificationRecord, OpenWork, WorkspaceDomainState } from '../workspace/types.ts'

export const recapGroups = [
  { group: 'decision', label: '等我决定' },
  { group: 'result', label: '离开后有结果' },
  { group: 'working', label: '还在进行' },
] as const
export type BotRecapItem = {
  key: string; group: typeof recapGroups[number]['group']; text: string; state: string
  target: InputTarget; focus?: string; notificationId?: string
}

const line = (text: string, fallback: string) => text.trim().split('\n')[0]?.slice(0, 160) || fallback

/** An existing fact with its original source; this view cannot acknowledge, approve or repeat work. */
export function botRecap(input: {
  bot: Pick<BotRecord, 'bot_id' | 'conversation_id'>; data: ConversationPayload | null
  inbox: readonly InboxItem[]; notifications: readonly NotificationRecord[]; ignoredNotifications: ReadonlySet<string>
  workspaceState: WorkspaceDomainState | null; openWorks: readonly OpenWork[]; fresh: ReadonlySet<string>; connected: boolean
}): BotRecapItem[] {
  const { bot } = input, data = input.data?.conversation?.conversation_id === bot.conversation_id ? input.data : null
  const current = input.connected && data?.scheduler?.remote?.reachable !== false
  const rows = new Map<string, { item: BotRecapItem; priority: number }>(), decisions = new Set<string>()
  const main: InputTarget = { kind: 'bot_main', bot_id: bot.bot_id }
  const tasks = input.workspaceState?.tasks.filter(task => task.owner_bot === bot.bot_id) ?? []
  const targetOfTask = (id: string): InputTarget => {
    const task = tasks.find(task => task.task_id === id)
    return task?.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: id, bot_id: bot.bot_id } : { kind: 'task', task_id: id }
  }
  const works = new Map(input.openWorks.filter(work => work.conversation_id === bot.conversation_id).map(work => [work.work_id, work]))
  for (const work of data?.works ?? []) works.set(work.work_id, { ...work, conversation_id: bot.conversation_id })
  const canonical = (target: InputTarget): InputTarget => {
    if (target.kind === 'work') {
      const work = works.get(target.work_id)
      if (work?.target?.kind === 'task' || work?.target?.kind === 'channel_task') return targetOfTask(work.target.task_id)
      const assignment = input.workspaceState?.assignments.find(assignment => assignment.work_id === target.work_id && assignment.bot_id === bot.bot_id)
      if (assignment) return targetOfTask(assignment.task_id)
    }
    return target.kind === 'task' || target.kind === 'channel_task' ? targetOfTask(target.task_id) : target
  }
  const add = (item: Omit<BotRecapItem, 'key'>, source: string, priority: number) => {
    const target = canonical(item.target), base = inputTargetKey(target)
    const scoped = target.kind !== 'bot_main' && target.kind !== 'channel_member', question = source.startsWith('question:')
    if (scoped && decisions.has(base) && !question) return
    if (scoped && question) { decisions.add(base); rows.delete(base) }
    const reply = target.kind === 'bot_main' && source.startsWith('message:') ? data?.scheduler?.jobs.find(job => job.reply_message_id === source.slice(8) && job.message_id) : undefined
    const key = base + (!scoped || question ? ':' + (reply ? 'input:' + reply.message_id : source) : ''), previous = rows.get(key)
    if (!previous || priority < previous.priority) rows.set(key, { item: { ...item, key }, priority })
    else if (priority === previous.priority && item.notificationId && !previous.item.notificationId) rows.set(key, { item: { ...previous.item, target: item.target, focus: item.focus, notificationId: item.notificationId }, priority })
  }
  const notices = input.notifications.filter(notice => notice.conversation_id === bot.conversation_id && !input.ignoredNotifications.has(notice.notification_id))
  const source = (target: InputTarget | undefined, focus: string) => {
    if (!target) return null
    const notice = notices.find(notice => notice.focus === focus && notice.target && inputTargetKey(notice.target) === inputTargetKey(target))
    if (target.kind === 'routine' && !notice) return null
    return { target, focus, ...(notice ? { notificationId: notice.notification_id } : {}) }
  }
  for (const item of input.inbox) {
    if (item.kind === 'question' && !item.done && !item.question.answer && !item.question.closed && item.question.conversation_id === bot.conversation_id) {
      const question = item.question, original = source(question.target, 'question:' + question.question_id)
      if (original) add({ group: 'decision', state: input.connected ? '等你回应' : '上次待回应', text: line(question.text, '查看原问题'), ...original }, 'question:' + question.question_id, 0)
    } else if (item.kind === 'undelivered' && item.message.conversation_id === bot.conversation_id) {
      const message = item.message
      if (message.target.kind === 'routine') continue
      add({ group: 'decision', state: item.title, text: line(message.text, '查看原输入'), target: message.target, focus: message.message_id }, 'input:' + message.message_id, 1)
    } else if (item.kind === 'confirm' && item.check.kind === 'work' && item.check.conversation_id === bot.conversation_id) {
      const work = item.check
      add({ group: 'decision', state: '结果待确认', text: work.title || '查看原工作', target: { kind: 'work', work_id: work.work_id } }, 'work:' + work.work_id, 1)
    } else if (item.kind === 'failed' && item.work.conversation_id === bot.conversation_id) {
      add({ group: 'decision', state: '未完成', text: item.work.title || '查看原工作', target: { kind: 'work', work_id: item.work.work_id } }, 'work:' + item.work.work_id, 2)
    }
  }
  for (const task of tasks) {
    const unknown = task.state === 'needs_reconciliation', waiting = unknown || task.state === 'waiting_user'
    if (!waiting && !['not_started', 'running', 'waiting_dependencies', 'update_pending', 'stop_pending'].includes(task.state)) continue
    add({ group: waiting ? 'decision' : 'working', state: unknown ? '结果待确认' : waiting ? input.connected ? '等你回应' : '上次待回应' : current ? '尚未结束' : '状态待确认', text: task.title, target: targetOfTask(task.task_id) }, 'task:' + task.task_id, waiting ? 1 : 4)
  }
  for (const work of works.values()) {
    const unknown = work.state === 'unverified' || work.state === 'dispatch_unverified' || work.outcome === 'unverified'
    if (!unknown && !taskActive(work.state)) continue
    add({ group: unknown ? 'decision' : 'working', state: unknown ? '结果待确认' : current ? '尚未结束' : '状态待确认', text: work.title || '查看原工作', target: { kind: 'work', work_id: work.work_id } }, 'work:' + work.work_id, unknown ? 1 : 4)
  }
  const jobs = data?.scheduler?.jobs ?? []
  const unsettled = (target: InputTarget, messageId?: string) => {
    const original = canonical(target)
    if ((original.kind === 'task' || original.kind === 'channel_task') && tasks.some(task => task.task_id === original.task_id && task.state === 'needs_reconciliation')) return true
    if (target.kind === 'work') { const work = works.get(target.work_id); if (work?.state === 'unverified' || work?.state === 'dispatch_unverified' || work?.outcome === 'unverified') return true }
    return Boolean(messageId && jobs.some(job => job.reply_message_id === messageId && (job.pending_check || job.uncertain || ['unknown', 'unverified', 'reconciling'].includes(job.state))))
  }
  for (const update of chatUpdates(data?.messages ?? [], jobs, input.fresh)) {
    const unknown = unsettled(main, update.id)
    add({ group: unknown ? 'decision' : 'result', state: unknown ? '结果待确认' : input.connected ? update.state : '最近回复', text: update.text, target: main, focus: update.id }, 'message:' + update.id, unknown ? 1 : 3)
  }
  for (const notice of notices) {
    if (!notice.target || notice.kind === 'question' || !['manager_attention', 'manager_response', 'bot_message', 'work_finished'].includes(notice.kind)) continue
    const waiting = notice.kind === 'manager_attention', unknown = unsettled(notice.target, notice.focus)
    add({ group: waiting || unknown ? 'decision' : 'result', state: unknown ? '结果待确认' : waiting ? '需要你查看' : input.connected ? '新记录' : '最近记录', text: line(notice.text, '查看原记录'), target: notice.target, ...(notice.focus ? { focus: notice.focus } : {}), notificationId: notice.notification_id }, notice.focus ? 'message:' + notice.focus : notice.notification_id, unknown ? 1 : waiting ? 2 : 3)
  }
  for (const job of jobs) {
    if (!job.message_id || !['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling', 'publishing', 'reconciling', 'unknown', 'unverified'].includes(job.state)) continue
    const unknown = job.pending_check || job.uncertain || ['reconciling', 'unknown', 'unverified'].includes(job.state)
    add({ group: unknown ? 'decision' : 'working', state: unknown ? '结果待确认' : current ? '尚未结束' : '状态待确认', text: line(data?.messages.find(message => message.message_id === job.message_id)?.text ?? '', '查看原输入'), target: main, focus: job.message_id }, 'input:' + job.message_id, unknown ? 1 : 4)
  }
  for (const job of data?.scopeJobs ?? []) {
    if (job.scope.kind === 'main' || !['pending_delivery', 'queued', 'steering', 'claimed', 'running', 'cancelling', 'publishing', 'reconciling', 'unknown', 'unverified'].includes(job.state)) continue
    const target: InputTarget = job.scope.kind === 'workspace_task' ? targetOfTask(job.scope.task_id) : job.scope.kind === 'task' ? { kind: 'work', work_id: job.scope.work_id } : { kind: 'routine', bot_id: bot.bot_id, trigger: job.scope.trigger }
    const notice = [...notices].reverse().find(notice => notice.target && inputTargetKey(notice.target) === inputTargetKey(target))
    const unknown = ['reconciling', 'unknown', 'unverified'].includes(job.state)
    add({ group: unknown ? 'decision' : 'working', state: unknown ? '结果待确认' : current ? '尚未结束' : '状态待确认', text: target.kind === 'routine' ? '例行任务' : target.kind === 'work' ? works.get(target.work_id)?.title || '查看原工作' : tasks.find(task => 'task_id' in target && task.task_id === target.task_id)?.title || '查看原任务', target, ...(notice ? { focus: notice.focus, notificationId: notice.notification_id } : {}) }, job.trigger, unknown ? 1 : 4)
  }
  return [...rows.values()].sort((a, b) => a.priority - b.priority || (a.item.key < b.item.key ? -1 : a.item.key > b.item.key ? 1 : 0)).map(row => row.item)
}

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
