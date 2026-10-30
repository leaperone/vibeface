import { Link } from 'expo-router'
import { Bot, ChevronRight, CircleCheck, CircleQuestionMark, CircleStop, CircleX, Laptop, LoaderCircle, ListChecks, MessageCircle, type LucideIcon } from 'lucide-react-native'
import { useEffect } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { workFailure } from '@vibeface/client/lib/errors.ts'
import { cardState, needsLook, nodeName, staleTaskLabel, taskActive, taskAgent, taskGroup, taskGroups, taskLabel, taskStale, type TaskGroup } from '@vibeface/client/workspace/labels.ts'
import { awaitsAnswer } from '@vibeface/client/workspace/questions.ts'
import { openWorksFromOverview, overviewFromWorkspace } from '@vibeface/client/workspace/model.ts'
import type { BotRecord, NodeInfo, OpenWork, OverviewTask } from '@vibeface/client/workspace/types.ts'
import { BotFace } from '@/components/bot-face'
import { EmptyState, providerIcons, Skeleton } from '@/components/kit'
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
  const bot = workspace.bots[0]
  const held = new Set((shared?.questions ?? []).filter((question) => awaitsAnswer(question, shared?.answered ?? {}) && question.about.request_id).map((question) => question.about.session_id))
  const groups: Record<TaskGroup, OpenWork[]> = { attention: [], working: [], waiting: [] }
  for (const work of visibleWorks) groups[taskGroup(cardState(work, lookup.nodes, projected.get(work.work_id)?.state), held.has(work.thread_id ?? ''))].push(work)

  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10">
    {loading ? <View accessibilityLabel="正在读取任务" className="gap-2">
      {[0, 1, 2].map((item) => <View key={item} className="min-h-16 flex-row items-center gap-3 rounded-xl border border-border bg-card px-4 py-3">
        <Skeleton className="size-8 rounded-lg" />
        <View className="flex-1 gap-2"><Skeleton className="h-3.5 w-2/3 rounded-full" /><Skeleton className="h-3 w-1/2 rounded-full" /></View>
      </View>)}
    </View> : visibleWorks.length ? taskGroups.map(({ group, title }) => groups[group].length ? <View key={group} className="gap-2">
      <Text role="heading" className="px-1 text-heading font-semibold">{title} <Text className="text-muted-foreground">{groups[group].length}</Text></Text>
      {groups[group].map((work) => <TaskRow key={work.work_id} work={work} projected={projected.get(work.work_id)} bot={workspace.bots.find((item) => item.conversation_id === work.conversation_id)} lookup={lookup} connected={Boolean(shared?.connectionOnline)} />)}
    </View> : null) : <MobileEmpty bot={bot} />}
  </ScrollView>
}

function MobileEmpty({ bot }: { bot?: Pick<BotRecord, 'bot_id' | 'name'> }) {
  return <EmptyState icon={ListChecks} title="没有未完成的任务" className="py-12" action={bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-2 rounded-xl border border-border px-4 py-2.5 active:bg-muted"><Icon as={MessageCircle} className="size-4" /><Text className="font-medium">打开 {bot.name}</Text></Pressable></Link> : undefined} />
}

type Lookup = { nodes: NodeInfo[]; directory: ReturnType<typeof useDevice>['directory'] }
type TaskBot = Pick<BotRecord, 'bot_id' | 'conversation_id' | 'name' | 'avatar'>

function TaskRow({ work, projected, bot, lookup, connected }: { work: OpenWork; projected?: Extract<OverviewTask, { kind: 'work' | 'dispatch' }>; bot?: TaskBot; lookup: Lookup; connected: boolean }) {
  const rawState = cardState(work, lookup.nodes, projected?.state)
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
  const line = failed || rawState === 'dispatch_unverified' ? workFailure(work) : work.summary
  return <View className={cn('min-h-16 flex-row items-center gap-3 rounded-xl border bg-card px-4 py-2.5 shadow-xs dark:border-line-strong', active ? 'border-brand/25' : 'border-border')}>
    {bot ? <BotFace bot={bot} mood="idle" size={28} still /> : <View className="size-8 items-center justify-center rounded-lg bg-muted"><Icon as={Bot} className="text-muted-foreground" /></View>}
    <View className="min-w-0 flex-1 gap-0.5">
      <View className="flex-row items-center gap-2">
        {href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={`查看任务 ${title}`} className="min-h-9 min-w-0 flex-1 justify-center rounded-md active:opacity-60"><Text numberOfLines={1} className="font-medium">{title}</Text></Pressable></Link> : <Text numberOfLines={1} className="min-w-0 flex-1 font-medium">{title}</Text>}
        <View className="flex-row items-center gap-1">
          <Icon as={icon} className={cn('size-3.5', failed ? 'text-danger' : unknown ? 'text-warning' : active ? 'text-foreground' : 'text-muted-foreground')} />
          <Text numberOfLines={1} className={cn('text-meta', failed ? 'text-danger' : unknown ? 'text-warning' : 'text-muted-foreground')}>{state}</Text>
        </View>
      </View>
      <View className="flex-row flex-wrap items-center gap-x-1.5 gap-y-0.5">
        {bot ? <Link href={`/b/${bot.bot_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`去对话 ${bot.name}`} className="min-h-7 max-w-[45%] flex-row items-center gap-1 rounded-md active:opacity-60"><Text numberOfLines={1} className="shrink text-meta text-muted-foreground underline">{bot.name}</Text></Pressable></Link> : <Text className="text-meta text-muted-foreground">已删除的 Bot</Text>}
        {agent ? <><Text className="text-meta text-muted-foreground">·</Text><Icon as={providerIcons[agent]} className="size-3.5 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{providerNames[agent]}</Text></> : null}
        <Text className="text-meta text-muted-foreground">·</Text><Icon as={Laptop} className="size-3.5 text-muted-foreground" /><Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{device}</Text>
      </View>
      {line ? <Text numberOfLines={1} className={cn('text-meta', failed ? 'text-danger' : 'text-muted-foreground')}>{line}</Text> : null}
    </View>
    {href ? <Link href={href} asChild><Pressable accessibilityRole="link" accessibilityLabel={`查看任务详情 ${title}`} className="size-11 items-center justify-center rounded-lg active:bg-muted"><Icon as={ChevronRight} className="size-4 text-muted-foreground" /></Pressable></Link> : null}
  </View>
}
