import { Link } from 'expo-router'
import { ArrowUpRight, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Inbox, Laptop, LoaderCircle, ListChecks, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useEffect } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { workFailure } from '@vibeface/client/lib/errors.ts'
import { cardState, needsLook, nodeName, overviewScope, staleTaskLabel, taskActive, taskAgent, taskGroup, taskGroups, taskLabel, taskProgress, taskStale, unrepresentedOverviewChecks, type TaskGroup } from '@vibeface/client/workspace/labels.ts'
import { workApproval } from '@vibeface/client/workspace/questions.ts'
import { openWorksFromOverview, overviewFromWorkspace } from '@vibeface/client/workspace/model.ts'
import type { BotRecord, NodeInfo, OpenWork, OverviewTask } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { providerIcons, Skeleton } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useInbox, useLive } from '@/data/live'
import { cn } from '@/lib/utils'

/** The personal task projection; only open and unsettled work is available without loading every Bot conversation. */
export function TasksScreen() {
  const shared = useLive()
  const inbox = useInbox()
  const attention = inbox.filter((item) => item.kind === 'question' && !item.done || item.kind === 'confirm' && item.check.kind === 'process')
  const pending = inbox.filter((item) => !(item.kind === 'question' && item.done)).length
  const workspace = useWorkspace()
  const { directory } = useDevice()
  useEffect(() => { if (!shared) return; shared.setView('tasks'); return () => shared.setView('other') }, [shared?.generation])
  const works = shared?.openWorks ?? []
  const read = Boolean(shared?.synced)
  const lookup = { nodes: shared?.nodes ?? [], directory }
  const localOverview = shared ? overviewFromWorkspace({ openWorks: works, bots: shared.bots, questions: shared.questions, nodes: lookup.nodes }) : null
  const overview = shared?.overview ?? localOverview
  type ProjectedWork = Extract<OverviewTask, { kind: 'work' | 'dispatch' }>
  const projected = new Map([...(localOverview?.tasks ?? []), ...(overview?.tasks ?? [])].filter((task): task is ProjectedWork => (task.kind === 'work' || task.kind === 'dispatch') && Boolean(task.work_id)).map((task) => [JSON.stringify([task.conversation_id, task.work_id]), task]))
  const projection = (work: OpenWork) => projected.get(JSON.stringify([work.conversation_id, work.work_id]))
  const visibleWorks = overview ? openWorksFromOverview(overview, works) : works
  const loading = !read && Boolean(shared?.connectionOnline) && !visibleWorks.length
  const bot = workspace.bots[0]
  const groups: Record<TaskGroup, OpenWork[]> = { attention: [], working: [], waiting: [] }
  for (const work of visibleWorks) {
    const state = cardState(work, lookup.nodes, projection(work))
    const asking = state === 'running' && workApproval({ work, conversationId: work.conversation_id, questions: shared?.questions ?? [], answered: shared?.answered ?? {} })
    groups[taskGroup(taskStale(state, Boolean(shared?.connectionOnline && read)) ? 'unknown' : state, Boolean(asking))].push(work)
  }
  const otherChecks = overview ? unrepresentedOverviewChecks(overview, visibleWorks, lookup.nodes, read ? shared?.bots ?? [] : []).filter((task) => task.conversation_id !== shared?.conversationId) : []

  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10">
    <MobileMasthead attention={read ? groups.attention.length + attention.length + otherChecks.length : null} working={read ? groups.working.length : null} waiting={read ? groups.waiting.length : null} online={Boolean(shared?.connectionOnline)} />
    <Text className="text-meta text-muted-foreground">{[...(overview ? overviewScope(overview.coverage) : ['尚未读取任务记录']), ...(!read ? ['当前连接尚未完成首读'] : !shared?.connectionOnline ? ['连接已断开，显示最近记录'] : [])].join(' · ')}</Text>
    <Link href="/inbox" asChild><Pressable accessibilityRole="link" accessibilityLabel={pending ? `${pending} 件事等你决定` : '查看待处理'} className={`min-h-11 flex-row items-center gap-3 rounded-xl border px-4 py-3 active:bg-muted ${pending ? 'border-warning/25 bg-warning-soft' : 'border-border bg-card'}`}><Icon as={pending ? CircleQuestionMark : Inbox} className={`size-5 ${pending ? 'text-warning' : 'text-muted-foreground'}`} /><Text className="min-w-0 flex-1">{pending ? `${pending} 件事等你决定` : '查看待处理'}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" /></Pressable></Link>
    {otherChecks.map((check) => <View key={`${check.conversation_id}:${check.item_id}`} className="gap-2 rounded-xl border border-border bg-card p-4"><Text className="font-medium">{check.work_id ? '任务状态待确认' : '程序状态待确认'} · {check.bot?.name ?? (read ? '未找到所属 Bot' : 'Bot 身份未加载')}</Text>{check.bot ? <Link href={`/b/${check.bot.bot_id}`} asChild><Pressable accessibilityRole="link" className="min-h-11 justify-center"><Text>打开 {check.bot.name}</Text></Pressable></Link> : null}</View>)}
    {loading ? <View accessibilityLabel="正在读取任务" className="gap-2">
      {[0, 1, 2].map((item) => <View key={item} className="min-h-16 flex-row items-center gap-3 rounded-xl border border-border bg-card px-4 py-3">
        <Skeleton className="size-8 rounded-lg" />
        <View className="flex-1 gap-2"><Skeleton className="h-3.5 w-2/3 rounded-full" /><Skeleton className="h-3 w-1/2 rounded-full" /></View>
      </View>)}
    </View> : visibleWorks.length ? taskGroups.map(({ group, title }) => groups[group].length ? <View key={group} className="gap-2">
      <Text role="heading" className="px-1 text-heading font-semibold">{title} <Text className="text-muted-foreground">{groups[group].length}</Text></Text>
      {groups[group].map((work) => <TaskRow key={JSON.stringify([work.conversation_id, work.work_id])} work={work} projected={projection(work)} waiting={Boolean(workApproval({ work, conversationId: work.conversation_id, questions: shared?.questions ?? [], answered: shared?.answered ?? {} }))} bot={shared?.bots.find((item) => item.conversation_id === work.conversation_id)} lookup={lookup} connected={Boolean(shared?.connectionOnline && read)} progress={taskProgress(work, shared?.liveProgress ?? [])} />)}
    </View> : null) : attention.length || otherChecks.length ? null : <MobileEmpty bot={bot} read={read} />}
  </ScrollView>
}

function MobileMasthead({ attention, working, waiting, online }: { attention: number | null; working: number | null; waiting: number | null; online: boolean }) {
  return <View className="overflow-hidden rounded-2xl border border-brand/15 bg-brand-soft/60 p-4"><View className="gap-3">
    <View className="flex-row flex-wrap items-center gap-2"><Text className="text-meta font-semibold uppercase tracking-[1.5px] text-brand">个人任务总览</Text><View className="flex-row items-center gap-1 rounded-full border border-border bg-background/60 px-2 py-1"><Icon as={online ? Wifi : WifiOff} className={online ? 'size-3.5 text-success' : 'size-3.5 text-warning'} /><Text className={cn('text-meta font-medium', online ? 'text-success' : 'text-warning')}>{online ? '在线' : '待连接'}</Text></View></View>
    <View className="gap-1"><Text role="heading" className="text-title font-semibold">掌握每个 Bot 的下一步</Text><Text className="text-meta leading-5 text-muted-foreground">{attention === null ? '当前连接的任务数量尚未确认。' : '查看已知任务的执行、等待和待确认状态。'}</Text></View>
    <View className="flex-row gap-2"><MobileMetric label="需要留意" value={attention} tone={attention ? 'warning' : 'neutral'} /><MobileMetric label="进行中" value={working} tone={working ? 'info' : 'neutral'} /><MobileMetric label="等待中" value={waiting} tone="neutral" /></View>
  </View></View>
}

function MobileMetric({ label, value, tone }: { label: string; value: number | null; tone: 'neutral' | 'warning' | 'info' }) {
  return <View className="min-w-0 flex-1 gap-0.5 rounded-xl border border-border/70 bg-background/55 px-3 py-2"><Text className="text-meta text-muted-foreground">{label}</Text><Text className={cn('text-lg font-semibold', tone === 'warning' ? 'text-warning' : tone === 'info' ? 'text-info' : 'text-foreground')}>{value ?? '待确认'}</Text></View>
}

function MobileEmpty({ bot, read }: { bot?: Pick<BotRecord, 'bot_id' | 'name'>; read: boolean }) {
  return <View className="items-center gap-3 rounded-2xl border border-dashed border-brand/25 bg-brand-soft/35 px-6 py-12"><View className="size-16 items-center justify-center rounded-2xl bg-brand-soft"><Icon as={ListChecks} className="size-8 text-brand" /></View><Text role="heading" className="text-center text-heading font-semibold">{read ? '当前记录没有未完成的任务' : '任务状态尚未读取'}</Text><Text className="max-w-[300px] text-center text-meta leading-5 text-muted-foreground">{read ? '这里展示当前设备已知的任务。Bot 的协作记录在上下文和协作中查看。' : '连接并完成首读后，才能确认是否有任务。'}</Text>{bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`打开 ${bot.name}`} className="mt-2 min-h-11 flex-row items-center gap-2 rounded-xl bg-brand px-4 py-2.5 active:opacity-80"><Text className="font-medium text-brand-foreground">打开 {bot.name}</Text><Icon as={ArrowUpRight} className="size-4 text-brand-foreground" /></Pressable></Link> : null}</View>
}

type Lookup = { nodes: NodeInfo[]; directory: ReturnType<typeof useDevice>['directory'] }
type TaskBot = Pick<BotRecord, 'bot_id' | 'conversation_id' | 'name' | 'avatar'>

function TaskRow({ work, projected, waiting, bot, lookup, connected, progress }: { work: OpenWork; waiting: boolean; projected?: Extract<OverviewTask, { kind: 'work' | 'dispatch' }>; bot?: TaskBot; lookup: Lookup; connected: boolean; progress: string }) {
  const shown = cardState(work, lookup.nodes, projected)
  const rawState = shown === 'running' && waiting ? 'waiting' : shown
  const failed = needsLook(rawState) && rawState !== 'dispatch_unverified'
  const stale = taskStale(rawState, connected)
  const state = stale ? staleTaskLabel : taskLabel(rawState)
  const active = taskActive(rawState) && !stale
  const unknown = !failed && (stale || rawState === 'unverified' || rawState === 'awaiting_confirmation' || rawState === 'unknown' || rawState === 'dispatch_unverified' || rawState === 'accepted_waiting_sync')
  const icon: LucideIcon = failed ? CircleX : unknown ? CircleQuestionMark : rawState === 'interrupted' ? CircleStop : active ? LoaderCircle : CircleCheck
  const agent = taskAgent(work, lookup.nodes)
  const device = nodeName(work.target_node, lookup)
  const title = work.title || '任务'
  const href = bot ? `/b/${bot.bot_id}/work/${work.work_id}` as const : undefined
  const failure = work.outcome === rawState || work.state === rawState ? work : { failure: projected?.failure, failure_reason: projected?.reason ?? null }
  const line = failed || rawState === 'dispatch_unverified' ? workFailure(failure) : progress
  // The whole row opens the task, so the touch target is the row, not a line of text; the bot's name is only a label.
  const frame = cn('min-h-16 flex-row items-center gap-3 rounded-xl border bg-card px-4 py-2.5 shadow-xs dark:border-line-strong', active ? 'border-brand/25' : 'border-border')
  const body = <>
    {bot ? <BotFace bot={bot} mood="idle" size={28} still /> : <View className="size-8 items-center justify-center rounded-lg bg-muted"><Icon as={Bot} className="text-muted-foreground" /></View>}
    <View className="min-w-0 flex-1 gap-0.5">
      <View className="flex-row items-center gap-2">
        <Text numberOfLines={1} className="min-w-0 flex-1 font-medium">{title}</Text>
        <View className="flex-row items-center gap-1">
          <Icon as={icon} className={cn('size-3.5', failed ? 'text-danger' : unknown ? 'text-warning' : active ? 'text-foreground' : 'text-muted-foreground')} />
          <Text numberOfLines={1} className={cn('text-meta', failed ? 'text-danger' : unknown ? 'text-warning' : 'text-muted-foreground')}>{state}</Text>
        </View>
      </View>
      <View className="flex-row flex-wrap items-center gap-x-1.5 gap-y-0.5">
        <Text numberOfLines={1} className="max-w-[45%] shrink text-meta text-muted-foreground">{bot ? bot.name : '已删除的 Bot'}</Text>
        {agent ? <><Text className="text-meta text-muted-foreground">·</Text><Icon as={providerIcons[agent]} className="size-3.5 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{providerNames[agent]}</Text></> : null}
        <Text className="text-meta text-muted-foreground">·</Text><Icon as={Laptop} className="size-3.5 text-muted-foreground" /><Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{device}</Text>
      </View>
      {line ? <Text numberOfLines={1} className={cn('text-meta', failed ? 'text-danger' : 'text-muted-foreground')}>{line}</Text> : null}
    </View>
    {href ? <Icon as={ChevronRight} className="size-4 text-muted-foreground" /> : null}
  </>
  return href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={[title, state, bot?.name, device, line].filter(Boolean).join('，')} className={cn(frame, 'active:bg-muted')}>{body}</Pressable></Link>
    : <View className={frame}>{body}</View>
}
