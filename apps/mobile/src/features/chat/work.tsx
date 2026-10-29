import { Link } from 'expo-router'
import { AlarmClock, ArrowRight, ArrowRightLeft, BookOpen, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Laptop, Link2, LoaderCircle, NotebookPen, Repeat, ScrollText, Smile, Unplug, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { failureText, taskError, type FailureKind } from '@vibeface/client/lib/errors.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { formatWhen, lineDiff, placeChanges, scheduleFrequency, scheduleTrigger } from '@vibeface/client/workspace/bot-settings.ts'
import { nodeName, shownState, taskActive, taskLabel } from '@vibeface/client/workspace/labels.ts'
import { pendingChecks, type WorkCheck } from '@vibeface/client/workspace/model.ts'
import { avatarChangeWords } from '@vibeface/client/avatar/pick.ts'
import type { AvatarEdit, BotChange, BotChangeRecord, ManagerJob, PendingCheck, ScheduleView, WorkRecord } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { ActionButton } from '@/components/action'
import { ActionCard, providerIcons } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { cn } from '@/lib/utils'

export type Row = { key: string; node: ReactNode }

/** Why a reply or task failed: its kind's words, else the first line of its original text. */
const cause = (kind: FailureKind | undefined, raw: string | null | undefined) => failureText(kind) || (raw ? taskError(raw).split('\n').at(-1) ?? '' : '')
const workCause = (work: WorkRecord) => cause(work.failure, work.failure_reason)

/** Plain-language reason a work needs the person; the device's own words stay out of the card. */
export function confirmText(check: Pick<WorkCheck, 'reason' | 'detail'>, device: string): string {
  if (check.reason === 'offline') return `${device} 离线了，报不回这个任务的结果。`
  if (check.reason === 'disconnected') return `${device} 上的 Codex 断开了，报不回这个任务的结果。`
  if (check.detail.startsWith('设备已移除')) return `${device} 已移除，报不回这个任务的结果。`
  if (/restart/i.test(check.detail)) return '设备在 Bot 工作时重启了，系统不会自动重做。'
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return '和 Bot 的连接断了，系统不会自动重做。'
  return '这次的结果没法自动确认，系统不会自动重做。'
}

/**
 * What the chat shows besides messages, as on the web: tasks after the message that started them, what a turn changed
 * after its reply, and everything waiting on the person (a device to take over, results to confirm) at the end.
 */
export function useChatExtras(conversationId: string, botId: string) {
  const shared = useLive()
  const { directory } = useDevice()
  const data = shared && shared.conversationId === conversationId ? shared.data : null
  const after = new Map<string, Row[]>()
  const rest: Row[] = []
  const cards: ReactNode[] = []
  if (!shared || !data) return { after, rest, cards, first: [] as Row[] }
  const lookup = { nodes: shared.nodes, directory }
  const add = (at: string | undefined, row: Row) => {
    if (at) after.set(at, [...after.get(at) ?? [], row])
    else rest.push(row)
  }
  const asking = new Set(data.questions.filter((question) => !question.answer && question.about.request_id).map((question) => question.about.session_id))
  const jobs = data.scheduler?.jobs ?? []
  const shown = new Set(data.messages.map((message) => message.message_id))
  data.works.forEach((work, index) => {
    const state = shownState(work, shared.nodes)
    const shownAs = state === 'running' && work.thread_id && asking.has(work.thread_id) ? 'waiting' : state
    const joined = work.turn_id ? data.works.slice(0, index).find((item) => item.thread_id === work.thread_id && item.turn_id === work.turn_id)?.title : undefined
    const scheduledReply = data.messages.find((message) => message.role !== 'user' && scheduleTrigger(message.in_reply_to) && message.in_reply_to === work.trigger)
    const cause = scheduledReply?.message_id ?? jobs.find((job) => job.trigger === work.trigger)?.message_id
    add(cause && shown.has(cause) ? cause : undefined, { key: `task:${work.work_id}`, node: <TaskCard work={work} state={shownAs} joined={joined} botId={botId} device={nodeName(work.target_node, lookup)} /> })
  })
  for (const job of jobs) {
    if ((job.state === 'failed' || job.state === 'rejected') && job.message_id && shown.has(job.message_id) && !shown.has(job.reply_message_id ?? '')) add(job.message_id, { key: `job:${job.trigger}`, node: <JobLine job={job} /> })
  }
  const changes = placeChanges(data.messages, data.changes)
  const schedules = data.bot_settings.schedules
  const changeRow = (records: BotChangeRecord[]) => ({ key: `change:${records[0].change_id}`, node: <ChangeCard records={records} schedules={schedules} /> })
  for (const [at, turns] of changes.after) for (const records of turns) add(at, changeRow(records))
  const first = changes.first.map(changeRow)

  const owner = data.conversation?.owner
  const here = shared.nodes.find((node) => node.device_id === data.local_node)
  if (owner && here?.online && here.capabilities?.manager && owner !== here.device_id && !shared.nodes.some((node) => node.device_id === owner && node.online && !node.revoked)) {
    cards.push(<ActionCard key="owner" icon={WifiOff} title={`${nodeName(owner, lookup)} 离线，对话在等它`}
      actions={<ActionButton icon={ArrowRightLeft} disabled={!shared.writable} action={() => shared.takeOver()}>换到 {nodeName(here.device_id, lookup)}</ActionButton>} />)
  }
  for (const check of pendingChecks(data, shared.nodes, conversationId)) cards.push(<ConfirmCard key={check.check_id} check={check} />)
  return { after, rest, cards, first }
}

/** Running tasks get a card; finished ones shrink to one line. Both open the details. */
function TaskCard({ work, state, joined, botId, device }: { work: WorkRecord; state: string; joined?: string; botId: string; device: string }) {
  const title = work.title || '任务'
  const active = taskActive(state)
  const open = active || state === 'waiting' || state === 'awaiting_confirmation'
  const failed = state === 'failed' || state === 'rejected' || state === 'dispatch_rejected'
  const href = `/b/${botId}/work/${work.work_id}` as const
  const agent = work.kind === 'task' ? null : work.kind ?? null
  const meta = <View className="min-w-0 flex-row items-center gap-1">
    {agent ? <>
      <Icon as={providerIcons[agent]} className="size-3.5 text-muted-foreground" />
      <Text className="text-meta text-muted-foreground">{providerNames[agent]} ·</Text>
    </> : null}
    <Icon as={Laptop} className="size-3.5 text-muted-foreground" />
    <Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{device}</Text>
    <Text className="text-meta text-muted-foreground">· {taskLabel(state)}</Text>
  </View>
  if (!open) {
    const result = work.result_text ? work.result_text.split('\n')[0] : workCause(work)
    const icon = state === 'interrupted' ? CircleStop : failed ? CircleX : CircleCheck
    return <Link href={href} asChild>
      <Pressable accessibilityRole="link" className="-mx-2 min-h-11 flex-row items-start gap-2 rounded-lg px-2 py-2 active:bg-muted">
        <Icon as={icon} className={cn('mt-0.5 size-4', failed ? 'text-danger' : icon === CircleCheck ? 'text-success' : 'text-muted-foreground')} />
        <View className="min-w-0 flex-1">
          <Text numberOfLines={1} className="text-meta text-foreground">{title}</Text>
          {meta}
          {result ? <Text className="text-meta text-muted-foreground">{result}</Text> : null}
        </View>
        <Icon as={ChevronRight} className="mt-0.5 size-4 text-muted-foreground" />
      </Pressable>
    </Link>
  }
  return <Link href={href} asChild>
    <Pressable accessibilityRole="link" className="min-h-14 flex-row items-center gap-3 rounded-xl border border-border bg-card px-4 py-2 shadow-xs active:bg-muted dark:border-line-strong">
      <Icon as={active ? LoaderCircle : CircleQuestionMark} className={cn('size-5', active ? 'text-foreground' : 'text-warning')} />
      <View className="min-w-0 flex-1 gap-0.5">
        <Text numberOfLines={1} className="font-medium">{title}</Text>
        {meta}
        {joined ? <View className="flex-row items-center gap-1"><Icon as={Link2} className="size-3.5 text-muted-foreground" /><Text numberOfLines={1} className="shrink text-meta text-muted-foreground">补充到「{joined}」</Text></View>
          : active && work.summary ? <Text numberOfLines={1} className="text-meta text-muted-foreground">{work.summary}</Text> : null}
      </View>
      <Icon as={ChevronRight} accessibilityLabel="详情" className="size-4 text-muted-foreground" />
    </Pressable>
  </Link>
}

function JobLine({ job }: { job: ManagerJob }) {
  const reason = job.failure ? cause(job.failure.kind, job.failure.detail) : ''
  return <View className="flex-row flex-wrap items-center gap-x-2 gap-y-1">
    <View className="flex-row items-center gap-1"><Icon as={CircleX} className="size-4 text-danger" /><Text className="text-meta text-danger">Bot 没能回复</Text></View>
    {reason ? <Text className="shrink text-meta text-muted-foreground">{reason}</Text> : null}
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
    actions={<ActionButton disabled={!shared.writable} action={() => shared.confirmStopped(check)}>确认已停止</ActionButton>}>
    <Text>{confirmText({ reason: 'unverified', detail: check.detail }, '')}相关程序还没退出，退出后会自动处理。</Text>
  </ActionCard>
  const device = nodeName(check.target_node, lookup)
  if (check.kind === 'device') return <ActionCard icon={confirmIcons[check.reason]} title={`${check.works.length} 个任务`}>
    <Text>{check.reason === 'offline' ? `${device} 离线了，报不回这些任务的结果。` : `${device} 上的 Codex 断开了，报不回这些任务的结果。`}它们实际做完了吗？</Text>
    {check.works.map((work) => <View key={work.work_id} className="gap-2">
      <Text numberOfLines={2} className="font-medium">{work.title || '任务结果'}</Text>
      <View className="flex-row gap-2">
        <ActionButton variant="outline" disabled={!shared.writable} action={() => shared.resolveCheck(work, 'interrupted', '')}>没做完</ActionButton>
        <ActionButton disabled={!shared.writable} action={() => shared.resolveCheck(work, 'completed', '')}>做完了</ActionButton>
      </View>
    </View>)}
  </ActionCard>
  const answer = (choice: 'completed' | 'interrupted') => () => shared.resolveCheck(check, choice, note)
  return <ActionCard icon={confirmIcons[check.reason]} title={check.title || '任务结果'} actions={<>
    <ActionButton variant="outline" disabled={!shared.writable} action={answer('interrupted')}>没做完</ActionButton>
    <ActionButton disabled={!shared.writable} action={answer('completed')}>做完了</ActionButton>
  </>}>
    <Text>{confirmText(check, device)}它实际做完了吗？</Text>
    <Input accessibilityLabel="补充说明" placeholder="补充说明（可选）" maxLength={2000} value={note} onChangeText={setNote} />
  </ActionCard>
}

function changeTitle(changes: BotChange[]) {
  const kind = changes[0]?.kind
  if (changes.some((change) => change.kind !== kind)) return '已更新'
  if (kind === 'profile') return '已更新 Bot'
  const items = changes as Extract<BotChange, { kind: 'skill' | 'schedule' | 'memory' }>[]
  const added = items.every((item) => !item.before), removed = items.every((item) => !item.after)
  if (kind === 'skill') return added ? '已存为 skill' : removed ? '已删除 skill' : '已更新 skill'
  if (kind === 'memory') return added ? '已记下' : removed ? '已忘记' : '已更新笔记'
  return added ? '已设定定时任务' : removed ? '已删除定时任务' : '已更新定时任务'
}

/** What one turn changed in the bot, item by item. */
function ChangeCard({ records, schedules }: { records: BotChangeRecord[]; schedules: readonly ScheduleView[] }) {
  return <View className="gap-3 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
    <View className="flex-row items-center gap-2">
      <Icon as={CircleCheck} className="size-4 text-success" />
      <Text role="heading" className="text-meta font-medium text-muted-foreground">{changeTitle(records.map((record) => record.change))}</Text>
    </View>
    {records.map((record) => <ChangeRows key={record.change_id} change={record.change} conversationId={record.conversation_id} schedules={schedules} />)}
  </View>
}

function ChangeRows({ change, conversationId, schedules }: { change: BotChange; conversationId: string; schedules: readonly ScheduleView[] }) {
  if (change.kind === 'profile') return <>
    {change.name ? <ChangeRow icon={Bot} label="名称"><BeforeAfter before={change.name.before} after={change.name.after} /></ChangeRow> : null}
    {change.instructions ? <ChangeRow icon={ScrollText} label="说明">
      {lineDiff(change.instructions.before, change.instructions.after).slice(0, 6).map((line, index) =>
        <Text key={index} className={cn('text-meta', line.kind === 'added' ? 'text-foreground' : 'text-muted-foreground line-through')}>{line.kind === 'added' ? '+ ' : '- '}{line.text}</Text>)}
    </ChangeRow> : null}
    {change.avatar ? <ChangeRow icon={Smile} label="长相"><AvatarChange edit={change.avatar} conversationId={conversationId} /></ChangeRow> : null}
  </>
  if (change.kind === 'skill') {
    const skill = change.after ?? change.before!
    return <ChangeRow icon={BookOpen} label={skill.name} tag={!change.before ? '新 skill' : !change.after ? '已删除' : '已更新'} removed={!change.after}>
      {change.after?.description ? <Text className="text-meta text-muted-foreground">{change.after.description}</Text> : null}
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
  const when = at !== null ? `${once ? '' : '下次 '}${formatWhen(at)}` : ''
  return <ChangeRow icon={icon} label={next.name} tag={tag}>
    <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{[scheduleFrequency(next), next.enabled ? when : '已暂停'].filter(Boolean).join(' · ')}</Text>
    <Text numberOfLines={2} className="text-meta text-muted-foreground">{next.prompt}</Text>
  </ChangeRow>
}

/** The face before and after, drawn and read out in words; spacing and tilt come from the bot's id, so a deleted bot's is a guess. */
function AvatarChange({ edit, conversationId }: { edit: AvatarEdit; conversationId: string }) {
  const botId = useLive()?.bots.find((bot) => bot.conversation_id === conversationId)?.bot_id ?? conversationId
  return <View accessible accessibilityRole="image" accessibilityLabel={avatarChangeWords(botId, edit.before, edit.after)} className="flex-row items-center gap-2">
    <BotFace bot={{ bot_id: botId, avatar: edit.before }} mood="idle" size={28} still />
    <Icon as={ArrowRight} className="size-4 text-muted-foreground" />
    <BotFace bot={{ bot_id: botId, avatar: edit.after }} mood="idle" size={28} still />
  </View>
}

function ChangeRow({ icon, label, tag, removed, children }: { icon: LucideIcon; label: string; tag?: string; removed?: boolean; children?: ReactNode }) {
  return <View className="flex-row gap-3">
    <Icon as={icon} className="mt-0.5 size-4 text-muted-foreground" />
    <View className="min-w-0 flex-1 gap-1">
      <Text><Text className={cn('font-medium', removed && 'text-muted-foreground line-through')}>{label}</Text>{tag ? <Text className="text-meta text-muted-foreground">  {tag}</Text> : null}</Text>
      {children}
    </View>
  </View>
}

function BeforeAfter({ before, after }: { before: string; after: string }) {
  return <View className="flex-row flex-wrap items-center gap-x-2">
    <Text className="text-muted-foreground">{before}</Text>
    <Icon as={ArrowRight} accessibilityLabel="改为" className="size-4 text-muted-foreground" />
    <Text>{after}</Text>
  </View>
}
