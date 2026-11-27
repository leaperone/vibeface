import { Card } from '@/components/arc/surface'
import { Elapsed } from '@/features/chat/elapsed'
import { Metric } from '@/components/arc/metric'
import { Link } from 'expo-router'
import { ArrowUpRight, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Laptop, LoaderCircle, ListChecks, type LucideIcon } from 'lucide-react-native'
import { memo, useEffect, useMemo, useState } from 'react'
import { FlatList, Pressable, ScrollView, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { workFailure } from '@vibeface/client/lib/errors.ts'
import { cardState, needsLook, nodeName, overviewScope, schedulerStatus, staleTaskLabel, taskActive, taskAgent, taskGroups, taskLabel, taskProgress, taskStale, type TaskGroup } from '@vibeface/client/workspace/labels.ts'
import { prepareOverviewInputs } from '@vibeface/client/workspace/overview-policy.ts'
import { selectTasksOverview, workspaceTaskGroup } from '@vibeface/client/workspace/presentation.ts'
import type { BotRecord, NodeInfo, OpenWorkCard, OverviewTask, WorkspaceActivity } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { providerIcons, Skeleton } from '@/components/kit'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { Button } from '@/components/arc/button'
import { useWorkspace } from '@/data/context'
import { useInbox, useLive } from '@/data/live'
import { cn } from '@/lib/utils'
import { WorkspaceTasks } from '@/features/workspace/workspace'
import { PendingSection } from './pending'

/**
 * 待处理 on top, then the personal task projection; only open and unsettled work is available without loading every Bot
 * conversation. Questions and checks act only in 待处理; a task held on one shows it as waiting.
 */
export function TasksScreen() {
  const shared = useLive()
  const [filter, setFilter] = useState<TaskGroup | 'all'>('all')
  const [botFilter, setBotFilter] = useState('all')
  const inbox = useInbox()
  const failedIds = new Set(inbox.flatMap((item) => item.kind === 'failed' ? [item.work.work_id] : []))
  const workspace = useWorkspace()
  const { directory } = useDevice()
  useEffect(() => { if (!shared) return; shared.setView('tasks'); return () => shared.setView('other') }, [shared?.generation])
  const works = shared?.openWorks ?? []
  const read = Boolean(shared?.synced)
  const openQuestions = inbox.filter((item): item is Extract<typeof inbox[number], { kind: 'question' }> => item.kind === 'question' && !item.done && Boolean(item.question)).map((item) => item.question)
  const pendingChecks = inbox.filter((item): item is Extract<typeof inbox[number], { kind: 'confirm' }> => item.kind === 'confirm').map((item) => item.check)
  const nodes = shared?.nodes
  const lookup = useMemo(() => ({ nodes: nodes ?? [], directory }), [nodes, directory])
  const prepared = prepareOverviewInputs({
    openWorks: works,
    bots: shared?.bots ?? [],
    questions: openQuestions,
    nodes: lookup.nodes,
    checks: pendingChecks,
    overview: shared?.overview, dismissedQuestions: shared?.dismissedQuestions,
    answered: shared?.answered ?? {},
    connected: Boolean(shared?.connectionOnline),
    read,
    terminalCheckPolicy: 'mobile-inclusive',
    jobs: (shared?.data?.scheduler?.jobs ?? []).map((job) => ({ conversation_id: shared?.conversationId ?? null, job })),
  })
  const overview = shared ? prepared.overview : null
  const activityView = prepared.activities
  const chosenConversation = shared?.bots.find((bot) => bot.bot_id === botFilter)?.conversation_id
  const selection = selectTasksOverview({
    workspace: shared?.workspaceState,
    activities: activityView,
    botId: botFilter,
    failedWorkIds: failedIds,
    includeActivity: (activity) => {
      if (botFilter === 'all' || activity.conversation_id === chosenConversation) return true
      return activity.kind === 'attention' && 'kind' in activity.attention && activity.attention.kind === 'device' && activity.attention.works.some((work) => work.conversation_id === chosenConversation)
    },
  })
  const businessTasks = selection.businessTasks
  const taskCounts = selection.taskCounts
  const heldGroups = selection.heldGroups
  const selectedActivities = selection.visibleActivities
  const visible = selectedActivities.filter((activity) => filter === 'all' || activity.group === filter)
  const workActivities = visible.filter((activity): activity is Extract<WorkspaceActivity, { kind: 'work' }> => activity.kind === 'work')
  const turns = visible.filter((activity): activity is Extract<WorkspaceActivity, { kind: 'turn' }> => activity.kind === 'turn')
  const questionActivities = visible.filter((activity): activity is Extract<WorkspaceActivity, { kind: 'attention' }> => activity.kind === 'attention' && 'question_id' in activity.attention)
  const loading = !read && Boolean(shared?.connectionOnline) && !activityView.activities.length
  const bot = workspace.bots.find((item) => item.bot_id === botFilter) ?? workspace.bots[0]
  const groups: Record<TaskGroup, Extract<WorkspaceActivity, { kind: 'work' }>[] > = {
    attention: workActivities.filter((activity) => activity.group === 'attention'),
    working: workActivities.filter((activity) => activity.group === 'working'),
    waiting: workActivities.filter((activity) => activity.group === 'waiting'),
  }
  const otherChecks = visible.flatMap((activity) => {
    if (activity.kind !== 'attention' || activity.conversation_id === shared?.conversationId || !('kind' in activity.attention) || activity.attention.kind !== 'check') return []
    return [activity.attention]
  })

  const count = (group: TaskGroup) => read ? selection.groups[group].length + heldGroups[group] + taskCounts[group] : null
  const lines: Line[] = loading ? [] : taskGroups.flatMap(({ group, title }) => groups[group].length ? [{ key: `group:${group}`, title, count: groups[group].length }, ...groups[group].map((activity) => ({ key: activity.id, activity }))] : [])
  return <FlatList data={lines} keyExtractor={(line) => line.key} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center p-6 pb-12"
    renderScrollComponent={(props) => <KeyboardAwareScrollView {...props} bottomOffset={24} />}
    renderItem={({ item }) => 'title' in item ? <Text role="heading" className="mt-6 px-1 text-lg font-medium">{item.title} <Text className="text-muted">{item.count}</Text></Text>
      : <View className="mt-2"><TaskRow work={item.activity.work!} projected={item.activity.projection} waiting={item.activity.facts.questions.length > 0} bot={shared?.bots.find((bot) => bot.conversation_id === item.activity.conversation_id)} lookup={lookup} connected={Boolean(shared?.connectionOnline && read)} progress={taskProgress(item.activity.work!, shared?.liveProgress ?? [])} /></View>}
    ListHeaderComponentClassName="gap-6" ListHeaderComponent={<>
    <PendingSection />
    <MobileMasthead attention={count('attention')} working={count('working')} waiting={count('waiting')} filter={filter} onFilter={setFilter} />
    <Text className="text-sm text-muted">{[...(overview ? overviewScope(overview.coverage) : ['尚未读取任务记录']), ...(!read ? ['当前连接尚未完成首读'] : !shared?.connectionOnline ? ['连接已断开，显示最近记录'] : [])].join(' · ')}</Text>
    <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2"><Button size="sm" hitSlop={4} variant={botFilter === 'all' ? 'secondary' : 'ghost'} accessibilityState={{ selected: botFilter === 'all' }} onPress={() => setBotFilter('all')}><Text>所有 Bot</Text></Button>{(read ? shared?.bots ?? [] : []).map((bot) => <Button key={bot.bot_id} size="sm" hitSlop={4} variant={botFilter === bot.bot_id ? 'secondary' : 'ghost'} accessibilityState={{ selected: botFilter === bot.bot_id }} onPress={() => setBotFilter(bot.bot_id)}><Text>{bot.name}</Text></Button>)}</ScrollView>
    <WorkspaceTasks botFilter={botFilter} filter={filter} />
    {otherChecks.map((check) => <Card key={`${check.conversation_id}:${check.item_id}`} className="gap-2"><Text className="font-medium">{check.work_id ? '任务状态待确认' : '程序状态待确认'} · {check.bot?.name ?? (read ? '未找到所属 Bot' : 'Bot 身份未加载')}</Text>{check.bot ? <Link href={`/b/${check.bot.bot_id}`} asChild><Pressable accessibilityRole="link" className="min-h-11 justify-center"><Text>打开 {check.bot.name}</Text></Pressable></Link> : null}</Card>)}
    {questionActivities.map((activity) => <MobileQuestionAttention key={activity.id} activity={activity} bots={shared?.bots ?? []} read={read} />)}
    {turns.map((activity) => <MobileTurn key={activity.id} activity={activity} />)}
    {loading ? <View accessibilityLabel="正在读取任务" className="gap-2">
      {[0, 1, 2].map((item) => <View key={item} className="min-h-16 flex-row items-center gap-3 rounded-panel border border-border bg-surface px-4 py-3">
        <Skeleton className="size-8 rounded-control" />
        <View className="flex-1 gap-2"><Skeleton className="h-3.5 w-2/3 rounded-pill" /><Skeleton className="h-3 w-1/2 rounded-pill" /></View>
      </View>)}
    </View> : workActivities.length ? null : selection.groups.attention.length + heldGroups.attention > 0 && (filter === 'all' || filter === 'attention') || otherChecks.length || turns.length || businessTasks.some((task) => filter === 'all' || workspaceTaskGroup(task) === filter) ? null : filter !== 'all' || botFilter !== 'all' ? <Text className="text-sm text-muted">没有符合筛选的执行工作。</Text> : <MobileEmpty bot={bot} read={read} />}
  </>} />
}

function MobileQuestionAttention({ activity, bots, read }: { activity: Extract<WorkspaceActivity, { kind: 'attention' }>; bots: BotRecord[]; read: boolean }) {
  const question = activity.attention
  if (!('question_id' in question)) return null
  const bot = 'bot' in question && question.bot ? question.bot : bots.find((item) => item.conversation_id === activity.conversation_id)
  return <View className="gap-2 rounded-panel border border-warning-border bg-warning-subtle p-4">
    <Text className="font-medium">问题待确认</Text>
    <Text className="text-sm text-muted">{question.text}</Text>
    {bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" className="min-h-11 justify-center"><Text>打开 {bot.name}</Text></Pressable></Link> : <Text className="text-sm text-muted">{read ? '未找到所属 Bot' : 'Bot 身份未加载'}</Text>}
  </View>
}

function MobileTurn({ activity }: { activity: Extract<WorkspaceActivity, { kind: 'turn' }> }) {
  const status = schedulerStatus(activity.job.state)
  return <Card className="gap-1">
    <Text className="font-medium">后台工作 · {status.label}</Text>
    <Text className="text-sm text-muted">{activity.job.trigger || '未提供触发信息'}{'source_node' in activity.job && activity.job.source_node ? ` · 来源设备 ${activity.job.source_node}` : ''}</Text>
  </Card>
}

/** The counts are the status filter: a tap shows that group, a second tap shows all again. */
function MobileMasthead({ filter, onFilter, ...counts }: Record<TaskGroup, number | null> & { filter: TaskGroup | 'all'; onFilter(filter: TaskGroup | 'all'): void }) {
  return <View className="flex-row gap-2" accessibilityLabel="任务筛选">
    {taskGroups.map(({ group, title }) => <Metric key={group} label={title} value={counts[group]} tone={group === 'attention' ? 'warning' : group === 'working' ? 'info' : 'neutral'} selected={filter === group} onPress={() => onFilter(filter === group ? 'all' : group)} />)}
  </View>
}


function MobileEmpty({ bot, read }: { bot?: Pick<BotRecord, 'bot_id' | 'name'>; read: boolean }) {
  return <View className="items-center gap-3 px-6 py-12"><Icon as={ListChecks} className="size-6 text-muted" /><Text role="heading" className="text-center text-lg font-medium">{read ? '当前记录没有未完成的任务' : '任务状态尚未读取'}</Text><Text className="max-w-[300px] text-center text-sm text-muted">{read ? 'Bot 的协作记录在上下文和协作中查看。' : '连接并完成首读后，才能确认是否有任务。'}</Text>{bot ? <Link href={`/b/${bot.bot_id}`} asChild><Button className="mt-2"><Text>打开 {bot.name}</Text><Icon as={ArrowUpRight} /></Button></Link> : null}</View>
}

type Lookup = { nodes: NodeInfo[]; directory: ReturnType<typeof useDevice>['directory'] }
type TaskBot = Pick<BotRecord, 'bot_id' | 'conversation_id' | 'name' | 'avatar'>
type Line = { key: string; title: string; count: number } | { key: string; activity: Extract<WorkspaceActivity, { kind: 'work' }> }

const TaskRow = memo(function TaskRow({ work, projected, waiting, bot, lookup, connected, progress }: { work: OpenWorkCard; waiting: boolean; projected?: Extract<OverviewTask, { kind: 'work' | 'dispatch' }>; bot?: TaskBot; lookup: Lookup; connected: boolean; progress: string }) {
  const shown = cardState(work, lookup.nodes, projected)
  const rawState = shown === 'running' && waiting ? 'waiting' : shown
  const failed = needsLook(rawState) && rawState !== 'dispatch_unverified'
  const stale = taskStale(rawState, connected)
  const state = stale ? staleTaskLabel : taskLabel(rawState)
  const active = taskActive(rawState) && !stale
  const unknown = !failed && (stale || rawState === 'unverified' || rawState === 'awaiting_confirmation' || rawState === 'unknown' || rawState === 'dispatch_unverified' || rawState === 'accepted_waiting_sync')
  const icon: LucideIcon = failed ? CircleX : unknown ? CircleQuestionMark : rawState === 'interrupted' ? CircleStop : active ? LoaderCircle : CircleCheck
  const agent = taskAgent(work)
  const device = nodeName(work.target_node, lookup)
  const title = work.title || '任务'
  const href = bot ? `/b/${bot.bot_id}/work/${work.work_id}` as const : undefined
  const failure = work.outcome === rawState || work.state === rawState ? work : { failure: projected?.failure, failure_reason: projected?.reason ?? null }
  const line = failed || rawState === 'dispatch_unverified' ? workFailure(failure) : progress
  // The whole row opens the task, so the touch target is the row, not a line of text; the bot's name is only a label.
  const frame = 'min-h-16 flex-row items-center gap-3 rounded-panel border border-border bg-surface px-4 py-2.5  '
  const body = <>
    {bot ? <BotFace bot={bot} mood="idle" size={28} still /> : <View className="size-8 items-center justify-center"><Icon as={Bot} className="text-muted" /></View>}
    <View className="min-w-0 flex-1 gap-0.5">
      <View className="flex-row items-center gap-2">
        <Text numberOfLines={2} className="min-w-0 flex-1 font-medium">{title}</Text>
        <View className="flex-row items-center gap-1">
          <Icon as={icon} className={cn('size-3.5', failed ? 'text-danger' : unknown ? 'text-warning' : active ? 'text-foreground' : 'text-muted')} />
          <Text numberOfLines={1} className={cn('text-sm', failed ? 'text-danger' : unknown ? 'text-warning' : 'text-muted')}>{state}</Text>
        </View>
      </View>
      <View className="flex-row flex-wrap items-center gap-x-1.5 gap-y-0.5">
        <Text numberOfLines={2} className="max-w-[45%] shrink text-sm text-muted">{bot ? bot.name : '已删除的 Bot'}</Text>
        {agent ? <><Text className="text-sm text-muted">·</Text><Icon as={providerIcons[agent]} className="size-3.5 text-muted" /><Text className="text-sm text-muted">{providerNames[agent]}</Text></> : null}
        <Text className="text-sm text-muted">·</Text><Icon as={Laptop} className="size-3.5 text-muted" /><Text numberOfLines={1} className="shrink text-sm text-muted">{device}</Text>
        <Elapsed dot start={work.dispatched_at_ms} end={work.finished_at_ms} active={taskActive(shown) && !stale} />
      </View>
      {line ? <Text numberOfLines={2} className={cn('text-sm', failed ? 'text-danger' : 'text-muted')}>{line}</Text> : null}
    </View>
    {href ? <Icon as={ChevronRight} className="size-4 text-muted" /> : null}
  </>
  return href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={[title, state, bot?.name, device, line].filter(Boolean).join('，')} className={cn(frame, 'active:bg-surface-muted')}>{body}</Pressable></Link>
    : <View className={frame}>{body}</View>
})
