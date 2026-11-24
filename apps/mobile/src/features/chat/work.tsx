import { Card } from '@/components/arc/surface'
import { Link } from 'expo-router'
import { AlarmClock, ArrowRight, ArrowRightLeft, BookOpen, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Cpu, Hand, Hourglass, Laptop, Link2, LoaderCircle, NotebookPen, Repeat, ScrollText, Smile, TriangleAlert, Undo2, Unplug, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useMemo, useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { failureText, taskError, workFailure, type FailureKind } from '@vibeface/client/lib/errors.ts'
import { replyUnsettled, runtimeLabel } from '@vibeface/client/chat/protocol.ts'
import { providerNames, type ProviderId } from '@vibeface/client/lib/node.ts'
import { changeTitle, firedAt, formatWhen, lineDiff, overwriteCopy, placeChanges, restoreRequest, scheduleFrequency } from '@vibeface/client/workspace/bot-settings.ts'
import { chatTaskState, interruptedCopy, nodeName, staleTaskLabel, taskActive, taskAgent, taskCopy, taskLabel, taskProgress, taskStale } from '@vibeface/client/workspace/labels.ts'
import { pendingChecks, type WorkCheck } from '@vibeface/client/workspace/model.ts'
import { questionOpen } from '@vibeface/client/workspace/questions.ts'
import { avatarChangeWords } from '@vibeface/client/avatar/pick.ts'
import type { AvatarEdit, BotChange, BotChangeRecord, ManagerJob, Overwritten, PendingCheck, QueueHoldReason, ScheduleView, SkillSet, WorkRecord } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { ActionButton } from '@/components/action'
import { ActionCard, providerIcons } from '@/components/kit'
import { Icon } from '@/components/arc/icon'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'
import { useLive } from '@/data/live'
import { touchRow } from '@/lib/layout'
import { cn } from '@/lib/utils'
import { InputStatus } from '@/features/workspace/workspace'

export type Row = { key: string; node: ReactNode }

/** Why a reply or task failed: its kind's words, else the first line of its original text. */
const cause = (kind: FailureKind | undefined, raw: string | null | undefined) => failureText(kind) || (raw ? taskError(raw).split('\n').at(-1) ?? '' : '')

/** Short reason a work needs the person. */
function confirmText(check: Pick<WorkCheck, 'reason' | 'detail'>, device: string): string {
  if (check.reason === 'offline') return `${device} 离线`
  if (check.reason === 'disconnected') return `${device} 上的 Codex 断开`
  if (check.detail.startsWith('设备已移除')) return `${device} 已移除`
  if (/restart/i.test(check.detail)) return '设备重启了'
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return '连接断了'
  return '结果没法自动确认'
}

type Live = NonNullable<ReturnType<typeof useLive>>
type Data = NonNullable<Live['data']>
type Summary = { failures: ReadonlyMap<string, Row[]>; attention: number; tasks: number; hold: QueueHoldReason | null }
const quiet: Summary = { failures: new Map(), attention: 0, tasks: 0, hold: null }

/** Each work in the state the chat shows; `going`: counted by the status line as a task in progress. */
function shownTasks(shared: Pick<Live, 'nodes' | 'connectionOnline'>, data: Data) {
  const asking = new Set(data.questions.filter((question) => questionOpen(question) && question.about.request_id).map((question) => question.about.session_id))
  return data.works.map((work) => {
    const state = chatTaskState(work, data.works, shared.nodes)
    const shownAs = state === 'running' && work.thread_id && asking.has(work.thread_id) ? 'waiting' : state
    const away = shownAs === 'dispatch_waiting' && shared.nodes.some((node) => node.device_id === work.target_node && !node.online)
    return { work, state: shownAs, stale: taskStale(shownAs, shared.connectionOnline), away, going: shared.connectionOnline && !away && (taskActive(shownAs) || shownAs === 'awaiting_children') }
  })
}

/** What needs the person, in the order the activity page lists it, and the inputs still on their way. */
function needsPerson(shared: Pick<Live, 'nodes' | 'pendingMessages'>, data: Data, conversationId: string) {
  const owner = data.conversation?.owner
  const here = shared.nodes.find((node) => node.device_id === data.local_node)
  const inputs = shared.pendingMessages.filter((item) => item.conversation_id === conversationId && item.target.kind === 'bot_main')
  const lost = (pending: (typeof inputs)[number]) => Boolean(pending.rejected || pending.delivery === 'unknown' || pending.receipt?.stage === 'unknown')
  return {
    ownerAway: owner && here?.online && here.capabilities?.manager && owner !== here.device_id && !shared.nodes.some((node) => node.device_id === owner && node.online && !node.revoked) ? { owner, here: here.device_id } : null,
    reconciling: (data.scheduler?.jobs ?? []).filter((job) => job.state === 'reconciling' || job.pending_check),
    checks: pendingChecks(data, shared.nodes, conversationId),
    lost: inputs.filter(lost),
    sending: inputs.filter((pending) => !lost(pending)),
  }
}

/**
 * What the chat knows besides messages. The list keeps only `failures`, a one-line JobLine after the message that got no
 * reply; everything else lives on the activity page, counted by `attention`, `tasks` and `hold` for the status line.
 */
export function useChatSummary(conversationId: string): Summary {
  const shared = useLive()
  const data = shared && shared.conversationId === conversationId ? shared.data : null
  const nodes = shared?.nodes, connectionOnline = shared?.connectionOnline, pendingMessages = shared?.pendingMessages
  return useMemo(() => {
    if (!data || !nodes || connectionOnline === undefined || !pendingMessages) return quiet
    const jobs = data.scheduler?.jobs ?? []
    const shown = new Set(data.messages.map((message) => message.message_id))
    const failures = new Map<string, Row[]>()
    for (const job of jobs) {
      const at = job.reply_message_id && shown.has(job.reply_message_id) ? job.reply_message_id : job.message_id
      if ((job.state === 'failed' || job.state === 'rejected') && job.message_id && shown.has(job.message_id) && !shown.has(job.reply_message_id ?? '') && at) failures.set(at, [...failures.get(at) ?? [], { key: `job:${job.trigger}`, node: <JobLine job={job} /> }])
    }
    const need = needsPerson({ nodes, pendingMessages }, data, conversationId)
    return {
      failures,
      attention: (need.ownerAway ? 1 : 0) + need.reconciling.length + need.checks.length + need.lost.length,
      tasks: shownTasks({ nodes, connectionOnline }, data).filter((task) => task.going).length,
      hold: jobs.find((job) => job.state === 'queued')?.hold_reason ?? null,
    }
  }, [data, nodes, connectionOnline, pendingMessages, conversationId])
}

/** The activity page: what needs the person, inputs on their way, then tasks, changes and runtimes, newest first. */
export function useChatActivity(conversationId: string, botId: string) {
  const shared = useLive()
  const { directory } = useDevice()
  const data = shared && shared.conversationId === conversationId ? shared.data : null
  if (!shared || !data) return null
  const lookup = { nodes: shared.nodes, directory }
  const jobs = data.scheduler?.jobs ?? []
  const need = needsPerson(shared, data, conversationId)
  const turns = new Map<string, string | undefined>()
  const tasks = shownTasks(shared, data).map(({ work, state, stale, away }): Row => {
    const turn = JSON.stringify([work.thread_id, work.turn_id])
    const joined = work.turn_id ? turns.get(turn) : undefined
    if (work.turn_id && !turns.has(turn)) turns.set(turn, work.title)
    return { key: `task:${work.work_id}`, node: <TaskCard work={work} state={state} stale={stale} away={away} joined={joined} botId={botId} device={nodeName(work.target_node, lookup)} agent={taskAgent(work)} progress={taskProgress(work, shared.liveProgress)} /> }
  })
  const changes = placeChanges(data.messages, data.changes)
  const schedules = data.bot_settings.schedules
  const { ownerAway } = need
  return {
    todo: [
      ...ownerAway ? [{ key: 'owner', node: <ActionCard icon={WifiOff} title={`${nodeName(ownerAway.owner, lookup)} 离线，对话在等它`}
        actions={<ActionButton icon={ArrowRightLeft} disabled={!shared.writable} action={() => shared.takeOver()}>换到 {nodeName(ownerAway.here, lookup)}</ActionButton>} /> }] : [],
      ...need.reconciling.map((job): Row => ({ key: `reconcile:${job.trigger}`, node: <ActionCard icon={CircleQuestionMark} title="这次回复的结果待确认" actions={<ActionButton action={() => shared.refresh()}>再查一次</ActionButton>} /> })),
      ...need.checks.map((check): Row => ({ key: check.check_id, node: <ConfirmCard check={check} /> })),
      ...need.lost.map((pending): Row => ({ key: `delivery:${pending.message_id}`, node: <InputStatus pending={pending} /> })),
    ],
    sending: need.sending.map((pending): Row => ({ key: `delivery:${pending.message_id}`, node: <InputStatus pending={pending} /> })),
    tasks: tasks.reverse(),
    changes: [...changes.first, ...[...changes.after.values()].flat()].map((records): Row => ({ key: `change:${records[0].change_id}`, node: <ChangeCard records={records} schedules={schedules} overwritten={data.overwritten} /> })).reverse(),
    runtime: jobs.flatMap((job): Row[] => job.runtime ? [{ key: `runtime:${job.trigger}`, node: <Text className="text-sm text-muted">{runtimeLabel(job.runtime)}</Text> }] : []).reverse(),
  }
}

/** Running tasks get a card; finished ones shrink to one line. Both open the details. */
/** `away`: its device is offline, so it waits for that device rather than spinning. */
function TaskCard({ work, state, stale, away, joined, botId, device, agent, progress }: { work: WorkRecord; state: string; stale: boolean; away: boolean; joined?: string; botId: string; device: string; agent: ProviderId | null; progress: string }) {
  const title = work.title || '任务'
  const active = taskActive(state)
  const open = active || state === 'waiting' || state === 'awaiting_confirmation' || state === 'awaiting_children'
  const failed = state === 'failed' || state === 'rejected' || state === 'dispatch_rejected'
  const href = `/b/${botId}/work/${work.work_id}` as const
  const meta = <View className="min-w-0 flex-row items-center gap-1">
    {agent ? <>
      <Icon as={providerIcons[agent]} className="size-3.5 text-muted" />
      <Text className="text-sm text-muted">{providerNames[agent]} ·</Text>
    </> : null}
    <Icon as={Laptop} className="size-3.5 text-muted" />
    <Text numberOfLines={1} className="shrink text-sm text-muted">{device}</Text>
    <Text className="text-sm text-muted">· {stale ? staleTaskLabel : away ? taskCopy.away : taskLabel(state)}</Text>
  </View>
  if (!open) {
    // A failed task's text is what it said before failing, so its cause leads.
    const result = failed ? workFailure(work) : work.result_text ? work.result_text.split('\n')[0] : workFailure(work) || (state === 'interrupted' ? interruptedCopy.reason : '')
    const icon = state === 'interrupted' ? CircleStop : failed ? CircleX : CircleCheck
    return <Link href={href} asChild>
      <Pressable accessibilityRole="link" className={cn(touchRow, '-mx-2 flex-row items-start gap-2 rounded-control px-2 py-2 active:bg-surface-muted')}>
        <Icon as={icon} className={cn('mt-0.5 size-4', failed ? 'text-danger' : icon === CircleCheck ? 'text-success' : 'text-muted')} />
        <View className="min-w-0 flex-1">
          <Text numberOfLines={1} className="text-sm text-foreground">{title}</Text>
          {meta}
          {result ? <Text className="text-sm text-muted">{result}</Text> : null}
        </View>
        <Icon as={ChevronRight} className="mt-0.5 size-4 text-muted" />
      </Pressable>
    </Link>
  }
  return <Link href={href} asChild>
    <Pressable accessibilityRole="link" className="min-h-14 flex-row items-center gap-3 rounded-panel border border-border bg-surface px-4 py-2 active:bg-surface-muted">
      {/* Waiting on an answer, it only says so: the question card below asks. */}
      <Icon as={state === 'waiting' ? Hand : state === 'awaiting_children' ? Hourglass : away ? WifiOff : active && !stale ? LoaderCircle : CircleQuestionMark} className={cn('size-5', stale || state === 'awaiting_children' ? 'text-muted' : active ? 'text-foreground' : 'text-warning')} />
      <View className="min-w-0 flex-1 gap-0.5">
        <Text numberOfLines={1} className="font-medium">{title}</Text>
        {meta}
        {joined ? <View className="flex-row items-center gap-1"><Icon as={Link2} className="size-3.5 text-muted" /><Text numberOfLines={1} className="shrink text-sm text-muted">补充到「{joined}」</Text></View>
          : active && !stale && progress ? <Text numberOfLines={1} className="text-sm text-muted">{progress}</Text> : null}
      </View>
      <Icon as={ChevronRight} accessibilityLabel="详情" className="size-4 text-muted" />
    </Pressable>
  </Link>
}

function JobLine({ job }: { job: ManagerJob }) {
  const shared = useLive()
  const reason = job.failure ? cause(job.failure.kind, job.failure.detail) : ''
  const newest = shared?.data?.messages.at(-1)
  const retry = shared && newest?.role === 'user' && newest.message_id === job.message_id && !replyUnsettled(job)
    ? () => shared.retryReply(newest.message_id) : undefined
  return <View className="flex-row flex-wrap items-center gap-x-2 gap-y-1">
    <View className="flex-row items-center gap-1"><Icon as={CircleX} className="size-4 text-danger" /><Text className="text-sm text-danger">没能回复</Text></View>
    {reason ? <Text className="shrink text-sm text-muted">{reason}</Text> : null}
    {shared ? retry ? <ActionButton variant="ghost" disabled={!shared.writable} action={retry}>重试</ActionButton> : <ActionButton variant="ghost" action={() => shared.refresh()}>再查一次</ActionButton> : null}
  </View>
}

const confirmIcons: Record<WorkCheck['reason'], LucideIcon> = { offline: WifiOff, disconnected: Unplug, unverified: CircleQuestionMark }

/** The person says whether a work its device cannot report finished, or that a leftover program stopped. Nothing is re-run. */
export function ConfirmCard({ check }: { check: PendingCheck }) {
  const shared = useLive()
  const { directory } = useDevice()
  const [note, setNote] = useState('')
  if (!shared) return null
  const lookup = { nodes: shared.nodes, directory }
  if (check.kind === 'process') return <ActionCard icon={CircleQuestionMark} title="上次回复的程序还在运行"
    actions={<ActionButton disabled={!shared.writable} action={() => shared.confirmStopped(check)}>确认已停止</ActionButton>} />
  const device = nodeName(check.target_node, lookup)
  if (check.kind === 'device') return <ActionCard icon={confirmIcons[check.reason]} title={`${check.works.length} 个任务`}>
    <Text>{check.reason === 'offline' ? `${device} 离线` : `${device} 上的 Codex 断开`} · 它们做完了吗？</Text>
    {check.works.map((work) => <View key={work.work_id} className="gap-2">
      <Text numberOfLines={2} className="font-medium">{work.title || '任务结果'}</Text>
      <View className="flex-row gap-2">
        <ActionButton variant="secondary" disabled={!shared.writable} action={() => shared.resolveCheck(work, 'interrupted', '')}>没做完</ActionButton>
        <ActionButton disabled={!shared.writable} action={() => shared.resolveCheck(work, 'completed', '')}>做完了</ActionButton>
      </View>
    </View>)}
  </ActionCard>
  const answer = (choice: 'completed' | 'interrupted') => () => shared.resolveCheck(check, choice, note)
  return <ActionCard icon={confirmIcons[check.reason]} title={check.title || '任务结果'} actions={<>
    <ActionButton variant="secondary" disabled={!shared.writable} action={answer('interrupted')}>没做完</ActionButton>
    <ActionButton disabled={!shared.writable} action={answer('completed')}>做完了</ActionButton>
  </>}>
    <Text>{confirmText(check, device)} · 它做完了吗？</Text>
    <Input accessibilityLabel="补充说明" placeholder="补充说明（可选）" maxLength={2000} value={note} onChangeText={setNote} />
  </ActionCard>
}

/** What one turn changed in the bot, item by item; `overwritten` the changes another device replaced without seeing them. */
function ChangeCard({ records, schedules, overwritten }: { records: BotChangeRecord[]; schedules: readonly ScheduleView[]; overwritten: readonly Overwritten[] }) {
  return <Card className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={CircleCheck} className="size-4 text-success" />
      <Text role="heading" className="text-sm font-medium text-muted">{changeTitle(records.map((record) => record.change))}</Text>
    </View>
    {records.map((record) => <ChangeRows key={record.change_id} change={record.change} conversationId={record.conversation_id} schedules={schedules}
      overwritten={overwritten.filter((item) => item.change_id === record.change_id)} />)}
  </Card>
}

function ChangeRows({ change, conversationId, schedules, overwritten }: { change: BotChange; conversationId: string; schedules: readonly ScheduleView[]; overwritten: readonly Overwritten[] }) {
  const messages = useLive()?.data?.messages ?? []
  if (change.kind === 'profile') {
    const note = (field: Overwritten['field']) => {
      const item = overwritten.find((entry) => entry.field === field)
      return item ? <OverwrittenNote item={item} change={change} conversationId={conversationId} /> : null
    }
    return <>
      {change.name ? <ChangeRow icon={Bot} label="名称"><BeforeAfter before={change.name.before} after={change.name.after} />{note('name')}</ChangeRow> : null}
      {change.instructions ? <ChangeRow icon={ScrollText} label="说明">
        {lineDiff(change.instructions.before, change.instructions.after).slice(0, 6).map((line, index) =>
          <Text key={index} className={cn('text-sm', line.kind === 'added' ? 'text-foreground' : 'text-muted line-through')}>{line.kind === 'added' ? '+ ' : '- '}{line.text}</Text>)}
        {note('instructions')}
      </ChangeRow> : null}
      {change.model ? <ChangeRow icon={Cpu} label="模型" tag={providerNames[change.model.provider]}><BeforeAfter before={change.model.before || '设备默认'} after={change.model.after || '设备默认'} />{note('model')}</ChangeRow> : null}
      {change.reasoning_effort ? <ChangeRow icon={Cpu} label="推理强度" tag={providerNames[change.reasoning_effort.provider]}><BeforeAfter before={change.reasoning_effort.before || '设备默认'} after={change.reasoning_effort.after || '设备默认'} />{note('reasoning_effort')}</ChangeRow> : null}
      {change.skills ? <ChangeRow icon={BookOpen} label="Skill"><BeforeAfter before={skillCount(change.skills.before)} after={skillCount(change.skills.after)} />{note('skills')}</ChangeRow> : null}
      {change.avatar ? <ChangeRow icon={Smile} label="长相"><AvatarChange edit={change.avatar} conversationId={conversationId} />{note('avatar')}</ChangeRow> : null}
    </>
  }
  if (change.kind === 'skill') {
    const skill = change.after ?? change.before!
    return <ChangeRow icon={BookOpen} label={skill.name} tag={!change.before ? '新 Skill' : !change.after ? '已删除' : '已更新'} removed={!change.after}>
      {change.after?.description ? <Text className="text-sm text-muted">{change.after.description}</Text> : null}
    </ChangeRow>
  }
  if (change.kind === 'memory') {
    const { before, after } = change
    if (!after) return <ChangeRow icon={NotebookPen} label={before!} tag="已忘记" removed />
    if (!before) return <ChangeRow icon={NotebookPen} label={after} tag="新笔记" />
    return <ChangeRow icon={NotebookPen} label="笔记" tag="已更新"><BeforeAfter before={before} after={after} /></ChangeRow>
  }
  const { before, after: next } = change
  const once = (next ?? before!).every_minutes === undefined
  const icon = once ? AlarmClock : Repeat
  const tag = !before ? (once ? '新提醒' : '新定时任务') : !next ? '已删除' : before.enabled !== next.enabled ? (next.enabled ? '已恢复' : '已暂停') : '已更新'
  if (!next) return <ChangeRow icon={icon} label={before!.name} tag={tag} removed />
  const current = schedules.find((schedule) => schedule.schedule_id === change.schedule_id)
  const at = once ? next.first_run_at_ms : current?.enabled ? current.next_run_at_ms : null
  const fired = once ? firedAt(messages, change.schedule_id, (next ?? before!).first_run_at_ms) : null
  const when = fired !== null ? `已提醒 ${formatWhen(fired)}` : at !== null ? `${once ? '' : '下次 '}${formatWhen(at)}` : ''
  return <ChangeRow icon={icon} label={next.name} tag={tag}>
    <Text className="text-sm text-muted" style={{ fontVariant: ['tabular-nums'] }}>{[scheduleFrequency(next), next.enabled ? when : '已暂停'].filter(Boolean).join(' · ')}</Text>
    <Text numberOfLines={2} className="text-sm text-muted">{next.prompt}</Text>
  </ChangeRow>
}

const skillCount = (set: SkillSet) => set === null ? '全部' : `${set.length} 个`

/** Another device's write replaced this setting without seeing it: what it is now, and setting it back the usual way. */
function OverwrittenNote({ item, change, conversationId }: { item: Overwritten; change: BotChange; conversationId: string }) {
  const shared = useLive()
  const { directory } = useDevice()
  const bot = shared?.bots.find((entry) => entry.conversation_id === conversationId)
  const request = restoreRequest(item, change)
  const restore = async () => {
    if (!shared || !bot || !request) return
    if ('avatar' in request) await shared.setBotAvatar(conversationId, bot.avatar_version, request.avatar)
    else await shared.sendMessage(request.text)
  }
  return <View className="gap-1">
    <View className="flex-row gap-1.5">
      <Icon as={TriangleAlert} className="mt-0.5 size-4 text-warning" />
      <Text className="min-w-0 flex-1 text-sm text-warning">{overwriteCopy.line(item, bot?.name ?? '这个 Bot', nodeName(item.by, { nodes: shared?.nodes ?? [], directory }))}</Text>
    </View>
    {bot && request ? <ActionButton variant="ghost" size="sm" className="self-start" icon={Undo2} disabled={!shared?.writable} action={restore}>{overwriteCopy.restore}</ActionButton> : null}
  </View>
}

/** The face before and after, drawn and read out in words; spacing and tilt come from the bot's id, so a deleted bot's is a guess. */
function AvatarChange({ edit, conversationId }: { edit: AvatarEdit; conversationId: string }) {
  const botId = useLive()?.bots.find((bot) => bot.conversation_id === conversationId)?.bot_id ?? conversationId
  return <View accessible accessibilityRole="image" accessibilityLabel={avatarChangeWords(botId, edit.before, edit.after)} className="flex-row items-center gap-2">
    <BotFace bot={{ bot_id: botId, avatar: edit.before }} mood="idle" size={28} still />
    <Icon as={ArrowRight} className="size-4 text-muted" />
    <BotFace bot={{ bot_id: botId, avatar: edit.after }} mood="idle" size={28} still />
  </View>
}

function ChangeRow({ icon, label, tag, removed, children }: { icon: LucideIcon; label: string; tag?: string; removed?: boolean; children?: ReactNode }) {
  return <View className="flex-row gap-3">
    <Icon as={icon} className="mt-0.5 size-4 text-muted" />
    <View className="min-w-0 flex-1 gap-1">
      <Text><Text className={cn('font-medium', removed && 'text-muted line-through')}>{label}</Text>{tag ? <Text className="text-sm text-muted">  {tag}</Text> : null}</Text>
      {children}
    </View>
  </View>
}

function BeforeAfter({ before, after }: { before: string; after: string }) {
  return <View className="flex-row flex-wrap items-center gap-x-2">
    <Text className="text-muted">{before}</Text>
    <Icon as={ArrowRight} accessibilityLabel="改为" className="size-4 text-muted" />
    <Text>{after}</Text>
  </View>
}
