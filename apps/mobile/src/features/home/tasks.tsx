import { Link } from 'expo-router'
import { ArrowUpRight, Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Laptop, LoaderCircle, ListChecks, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useEffect } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { nodeName, shownState, staleTaskLabel, taskActive, taskLabel, taskStale } from '@vibeface/client/workspace/labels.ts'
import { openWorksFromOverview, overviewFromWorkspace } from '@vibeface/client/workspace/model.ts'
import type { BotRecord, NodeInfo, OpenWork, OverviewTask } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { providerIcons, Skeleton } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { useLive } from '@/data/live'
import { cn } from '@/lib/utils'

/** The personal task projection; only open and unsettled work is available without loading every Bot conversation. */
export function TasksScreen() {
  const shared = useLive()
  const workspace = useWorkspace()
  const { directory } = useDevice()
  useEffect(() => { if (!shared) return; shared.setView('tasks'); return () => shared.setView('other') }, [shared?.generation])
  const works = shared?.openWorks ?? []
  const loading = Boolean(shared && !shared.synced && !works.length)
  const lookup = { nodes: shared?.nodes ?? [], directory }
  const localOverview = shared ? overviewFromWorkspace({ openWorks: works, bots: shared.bots, questions: shared.questions, nodes: lookup.nodes }) : null
  const overview = shared?.overview ?? localOverview
  type ProjectedWork = Extract<OverviewTask, { kind: 'work' | 'dispatch' }>
  const projected = new Map(overview?.tasks.filter((task): task is ProjectedWork => (task.kind === 'work' || task.kind === 'dispatch') && Boolean(task.work_id)).map((task) => [task.work_id!, task]))
  const overviewWorks = shared?.overview ? openWorksFromOverview(shared.overview) : works
  const visibleWorks = overview ? overviewWorks.filter((work) => projected.has(work.work_id)) : overviewWorks
  const working = visibleWorks.filter((work) => {
    const state = projected.get(work.work_id)?.state ?? shownState(work, lookup.nodes)
    return taskActive(state) && !taskStale(state, Boolean(shared?.connectionOnline))
  }).length
  const waiting = Math.max(0, visibleWorks.length - working)
  const attention = shared?.questions.filter((question) => !question.answer && !question.closed).length ?? 0
  const bot = workspace.bots[0]

  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10">
    <MobileMasthead attention={attention} working={working} waiting={waiting} online={Boolean(shared?.connectionOnline)} />
    {loading ? <View accessibilityLabel="正在读取任务" className="gap-2">
      {[0, 1, 2].map((item) => <View key={item} className="min-h-16 flex-row items-center gap-3 rounded-xl border border-border bg-card px-4 py-3">
        <Skeleton className="size-8 rounded-lg" />
        <View className="flex-1 gap-2"><Skeleton className="h-3.5 w-2/3 rounded-full" /><Skeleton className="h-3 w-1/2 rounded-full" /></View>
      </View>)}
    </View> : visibleWorks.length ? <View className="gap-2">{visibleWorks.map((work) => {
      const bot = workspace.bots.find((item) => item.conversation_id === work.conversation_id)
      return <TaskRow key={work.work_id} work={work} projected={projected.get(work.work_id)} detailAvailable={works.some((item) => item.work_id === work.work_id)} bot={bot} lookup={lookup} connected={Boolean(shared?.connectionOnline)} />
    })}</View> : <MobileEmpty bot={bot} />}
  </ScrollView>
}

function MobileMasthead({ attention, working, waiting, online }: { attention: number; working: number; waiting: number; online: boolean }) {
  return <View className="overflow-hidden rounded-2xl border border-brand/15 bg-brand-soft/60 p-4"><View className="gap-3">
    <View className="flex-row flex-wrap items-center gap-2"><Text className="text-meta font-semibold uppercase tracking-[1.5px] text-brand">个人 Agent 工作台</Text><View className="flex-row items-center gap-1 rounded-full border border-success-border bg-success-soft px-2 py-1"><Icon as={online ? Wifi : WifiOff} className={online ? 'size-3.5 text-success' : 'size-3.5 text-warning'} /><Text className={cn('text-meta font-medium', online ? 'text-success' : 'text-warning')}>{online ? '在线' : '待连接'}</Text></View></View>
    <View className="gap-1"><Text role="heading" className="text-title font-semibold">掌握每个 Bot 的下一步</Text><Text className="text-meta leading-5 text-muted-foreground">执行、等待和协作状态会在这里汇合。</Text></View>
    <View className="flex-row gap-2"><Metric label="需要留意" value={attention} tone={attention ? 'warning' : 'neutral'} /><Metric label="进行中" value={working} tone={working ? 'info' : 'neutral'} /><Metric label="等待中" value={waiting} tone="neutral" /></View>
  </View></View>
}

function Metric({ label, value, tone }: { label: string; value: number; tone: 'neutral' | 'warning' | 'info' }) {
  return <View className="min-w-0 flex-1 gap-0.5 rounded-xl border border-border/70 bg-background/55 px-3 py-2"><Text className="text-meta text-muted-foreground">{label}</Text><Text className={cn('text-lg font-semibold', tone === 'warning' ? 'text-warning' : tone === 'info' ? 'text-info' : 'text-foreground')}>{value}</Text></View>
}

function MobileEmpty({ bot }: { bot?: Pick<BotRecord, 'bot_id' | 'name'> }) {
  return <View className="items-center gap-3 rounded-2xl border border-dashed border-brand/25 bg-brand-soft/35 px-6 py-12"><View className="size-16 items-center justify-center rounded-2xl bg-brand-soft ring-1 ring-brand/20"><Icon as={ListChecks} className="size-8 text-brand" /></View><Text className="text-meta font-semibold uppercase tracking-[1.5px] text-brand">随时开始</Text><Text role="heading" className="text-center text-heading font-semibold">还没有需要盯住的任务</Text><Text className="max-w-[300px] text-center text-meta leading-5 text-muted-foreground">从 Bot 对话里发出第一条请求后，执行步骤和协作消息会在这里汇合。</Text>{bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`打开 ${bot.name}`} className="mt-2 min-h-11 flex-row items-center gap-2 rounded-xl bg-brand px-4 py-2.5 active:opacity-80"><Text className="font-medium text-brand-foreground">打开 {bot.name}</Text><Icon as={ArrowUpRight} className="size-4 text-brand-foreground" /></Pressable></Link> : null}</View>
}

type Lookup = { nodes: NodeInfo[]; directory: ReturnType<typeof useDevice>['directory'] }
type TaskBot = Pick<BotRecord, 'bot_id' | 'conversation_id' | 'name' | 'avatar'>

function TaskRow({ work, projected, detailAvailable, bot, lookup, connected }: { work: OpenWork; projected?: Extract<OverviewTask, { kind: 'work' | 'dispatch' }>; detailAvailable: boolean; bot?: TaskBot; lookup: Lookup; connected: boolean }) {
  const rawState = projected?.state ?? shownState(work, lookup.nodes)
  const stale = taskStale(rawState, connected)
  const state = stale ? staleTaskLabel : taskLabel(rawState)
  const active = taskActive(rawState) && !stale
  const unknown = stale || rawState === 'unverified' || rawState === 'awaiting_confirmation' || rawState === 'unknown' || rawState === 'dispatch_rejected' || rawState === 'dispatch_unverified' || rawState === 'accepted_waiting_sync'
  const icon: LucideIcon = unknown ? CircleQuestionMark : rawState === 'failed' || rawState === 'rejected' ? CircleX : rawState === 'interrupted' ? CircleStop : active ? LoaderCircle : CircleCheck
  const agent = work.kind && work.kind !== 'task' ? work.kind : null
  const device = nodeName(work.target_node, lookup)
  const title = work.title || '任务'
  const href = bot && detailAvailable ? `/b/${bot.bot_id}/work/${work.work_id}` as const : undefined
  return <View className={cn('min-h-16 flex-row items-center gap-3 rounded-xl border bg-card px-4 py-2.5 shadow-xs dark:border-line-strong', active ? 'border-brand/25' : 'border-border')}>
    {bot ? <BotFace bot={bot} mood="idle" size={28} still /> : <View className="size-8 items-center justify-center rounded-lg bg-muted"><Icon as={Bot} className="text-muted-foreground" /></View>}
    <View className="min-w-0 flex-1 gap-0.5">
      <View className="flex-row items-center gap-2">
        {href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={`查看任务 ${title}`} className="min-h-9 min-w-0 flex-1 justify-center rounded-md active:opacity-60"><Text numberOfLines={1} className="font-medium">{title}</Text></Pressable></Link> : <Text numberOfLines={1} className="min-w-0 flex-1 font-medium">{title}</Text>}
        <View className="flex-row items-center gap-1">
          <Icon as={icon} className={cn('size-3.5', unknown ? 'text-warning' : active ? 'text-foreground' : 'text-muted-foreground')} />
          <Text numberOfLines={1} className={cn('text-meta', unknown ? 'text-warning' : 'text-muted-foreground')}>{state}</Text>
        </View>
      </View>
      <View className="flex-row flex-wrap items-center gap-x-1.5 gap-y-0.5">
        {bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`去对话 ${bot.name}`} className="min-h-7 max-w-[45%] flex-row items-center gap-1 rounded-md active:opacity-60"><Text numberOfLines={1} className="shrink text-meta text-muted-foreground underline">{bot.name}</Text></Pressable></Link> : <Text className="text-meta text-muted-foreground">未知 Bot</Text>}
        {agent ? <><Text className="text-meta text-muted-foreground">·</Text><Icon as={providerIcons[agent]} className="size-3.5 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{providerNames[agent]}</Text></> : null}
        <Text className="text-meta text-muted-foreground">·</Text><Icon as={Laptop} className="size-3.5 text-muted-foreground" /><Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{device}</Text>
      </View>
      {work.summary ? <Text numberOfLines={1} className="text-meta text-muted-foreground">{work.summary}</Text> : null}
    </View>
    {href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={`查看任务详情 ${title}`} className="size-11 items-center justify-center rounded-lg active:bg-muted"><Icon as={ChevronRight} className="size-4 text-muted-foreground" /></Pressable></Link> : null}
  </View>
}
