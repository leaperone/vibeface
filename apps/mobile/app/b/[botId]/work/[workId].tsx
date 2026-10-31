import { Stack, useFocusEffect, useLocalSearchParams, useRouter } from 'expo-router'
import { Bot, Circle, CircleCheck, CircleDot, FolderOpen, Laptop, Link2, ListChecks, Play, RotateCw, SearchCheck, Settings, Square, type LucideIcon } from 'lucide-react-native'
import { useCallback, useRef, useState, type ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { outcomeUnknown, rawError, workFailure } from '@vibeface/client/lib/errors.ts'
import { agentLaneLabels, contextStages, fixOnDevice, fixOnDeviceLabel, followUp, followUpCopy, interruptedCopy, nodeName, parseSummarySteps, progressLabel, promptSummary, queueHoldLabels, schedulerStatus, shownState, staleTaskLabel, taskActive, taskAgent, taskLabel, taskStale, type FollowUp, type SummaryStep } from '@vibeface/client/workspace/labels.ts'
import { messageOf, providerNames } from '@vibeface/client/lib/node.ts'
import { projectName, workApproval } from '@vibeface/client/workspace/questions.ts'
import { workChecks, type WorkCheck } from '@vibeface/client/workspace/model.ts'
import { ActionButton } from '@/components/action'
import { CopyableId, Details, EmptyState, providerIcons, toneText, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { QuestionCard } from '@/features/chat/question-card'
import { ConfirmCard } from '@/features/chat/work'
import { cn } from '@/lib/utils'

const tone = (state: string): Tone => state === 'failed' ? 'danger' : state === 'completed' ? 'success' : ['awaiting_confirmation', 'stop_timed_out'].includes(state) ? 'warning' : 'neutral'

/** One task of a bot's chat, as the web's task panel: progress, result, stop and the details behind it. */
export default function TaskDetail() {
  const { botId, workId } = useLocalSearchParams<{ botId: string; workId: string }>()
  const shared = useLive()
  const { directory, ready } = useDevice()
  const router = useRouter()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  const [error, setError] = useState<{ generation: number; botId: string; text: string } | null>(null)
  const [retry, setRetry] = useState(0)
  const latest = useRef(shared)
  latest.current = shared
  useFocusEffect(useCallback(() => {
    const shared = latest.current
    if (!shared) return
    shared.setView('other')
    let alive = true
    const current = () => alive && shared.isCurrent()
    if (bot && shared.connectionOnline && (retry > 0 || !shared.synced || shared.conversationId !== bot.conversation_id || shared.data?.conversation?.conversation_id !== bot.conversation_id)) {
      setError(null)
      void shared.selectBot(bot).then(() => { if (current()) setError(null) }, (cause: unknown) => {
        if (current()) setError({ generation: shared.generation, botId, text: messageOf(cause, '读取任务') })
      })
    }
    return () => { alive = false }
  }, [bot?.conversation_id, shared?.generation, shared?.connectionOnline, retry]))
  const data = shared && bot && shared.conversationId === bot.conversation_id && shared.data?.conversation?.conversation_id === bot.conversation_id ? shared.data : null
  const readError = error?.generation === shared?.generation && error?.botId === botId ? error.text : null
  const work = data?.works.find((item) => item.work_id === workId)
  if (!shared || !bot || !data || !work) return <><Stack.Screen options={{ title: '任务' }} /><EmptyState icon={ListChecks}
    title={readError ?? (!shared || !shared.connectionOnline ? '任务状态未知，连接后再读取' : !shared.synced ? '正在读取任务' : !bot ? '找不到这个 Bot' : data ? '当前记录中找不到这个任务' : '正在读取这个 Bot 的任务')}
    action={shared?.connectionOnline && bot ? <Button variant="outline" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button> : undefined} className="flex-1 justify-center" /></>

  const lookup = { nodes: shared.nodes, directory }
  const asking = workApproval({ work, conversationId: bot.conversation_id, questions: data.questions, answered: shared.answered })
  const shown = shownState(work, shared.nodes)
  const state = shown === 'running' && asking ? 'waiting' : shown
  const device = work.target_node
  const agent = taskAgent(work)
  const progress = shared.liveProgress.find((item) => item.kind === 'native' && item.work_id === workId && item.target_node === device)
  const canStop = state !== 'awaiting_confirmation' && !work.outcome && !work.stop && ['accepted', 'running'].includes(work.state)
  // A failed task's text is what it said before failing, so its cause leads and that text waits in 详细信息.
  const failed = ['failed', 'rejected', 'dispatch_rejected'].includes(state)
  const result = failed ? workFailure(work) || '这个任务没有成功。' : work.result_text || workFailure(work) || (state === 'completed' ? '没有返回文字' : state === 'interrupted' ? interruptedCopy.reason : '')
  // It may have done its writes anyway, so it is checked first; running it again could do them twice.
  const unknown = state === 'dispatch_unverified' || outcomeUnknown(work.failure, work.failure_reason)
  // Stopping one input steered into a turn stops every work sharing that turn.
  const sharing = work.turn_id ? data.works.filter((item) => item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const active = taskActive(state)
  const next = followUp(state, unknown)
  // Its device cannot say whether it finished, so the person settles it here as in the chat; nothing is re-run.
  const check = state === 'awaiting_confirmation' ? workChecks([{ ...work, conversation_id: bot.conversation_id }], shared.nodes).find((item): item is WorkCheck => item.kind === 'work') : undefined
  const asked = promptSummary(work.prompt, work.title)
  const plan = parseSummarySteps(work.summary)
  const coordination = work.coordination_job
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: work.title || '任务' }} />
    <Button variant="ghost" className="self-start" accessibilityLabel={`打开所属 Bot ${bot.name}`} onPress={() => router.push(`/b/${bot.bot_id}`)}><Icon as={Bot} /><Text>{bot.name}</Text></Button>
    {!shared.synced || !shared.connectionOnline ? <Text className="text-meta text-warning">{!shared.connectionOnline ? '连接已断开，显示最近记录；当前状态未知。' : '当前连接尚未完成首读，显示已知记录。'}</Text> : null}
    {readError ? <View className="gap-2"><Text className="text-danger">{readError}</Text><Button variant="outline" className="self-start" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button></View> : null}
    <View className="gap-1">
      {taskStale(state, shared.connectionOnline && shared.synced) ? <Text className="font-medium text-muted-foreground">{staleTaskLabel}</Text> : <Text className={cn('font-medium', toneText[tone(state)])}>{taskLabel(state)}</Text>}
      {device || work.cwd || agent ? <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
        {agent ? <Meta icon={providerIcons[agent]} label="AI">{providerNames[agent]}</Meta> : null}
        {device ? <Meta icon={Laptop} label="设备">{nodeName(device, lookup)}</Meta> : null}
        {work.cwd ? <Meta icon={FolderOpen} label="项目">{projectName(work.cwd)}</Meta> : null}
      </View> : null}
      {progress?.status && active && progress.status !== 'awaiting_input' && progressLabel(progress.status) !== '进行中' ? <Text className="text-meta text-muted-foreground">{progressLabel(progress.status)}</Text> : null}
      {plan ? <Details label={`最近步骤摘要 · ${plan.length}`} open={active}><SummarySteps steps={plan} /></Details> : !progress?.text && work.summary && active ? <Text selectable className="mt-2">{work.summary}</Text> : null}
      {progress?.text ? <ScrollView nestedScrollEnabled className="mt-3 max-h-96 rounded-xl bg-muted" contentContainerClassName="p-3">
        <Text selectable className="font-mono text-meta">{progress.text}</Text>
      </ScrollView> : null}
    </View>
    {work.kind === 'task' ? <Details label="协调回合">
      {coordination ? <>
        <Text>{!shared.connectionOnline || !shared.synced || shared.nodes.find((node) => node.device_id === device)?.online === false ? '当前状态未知，显示最近记录' : schedulerStatus(coordination.state).label}{coordination.lane ? ` · ${agentLaneLabels[coordination.lane]}` : ''}</Text>
        {coordination.hold_reason ? <Text>{queueHoldLabels[coordination.hold_reason]}</Text> : null}
        {coordination.pending_check ? <Text>执行结果需要核对，不能据此重发。</Text> : null}
        {coordination.context ? <Text>会话代次 {coordination.context.session_generation} · {contextStages[coordination.context.stage].label}</Text> : null}
      </> : <Text>执行设备尚未提供这个任务的协调回合观察。</Text>}
      <Text className="text-meta text-muted-foreground">这里显示 Bot 的准备和排队情况，任务结果仍以上面的共享记录为准。</Text>
    </Details> : null}
    {asked.summary ? <Section title="交代的事"><Text selectable>{asked.summary}</Text></Section> : null}
    {result && !active ? <Section title={!failed && (work.result_text || state === 'completed') ? '结果' : '原因'}><Text selectable className={cn(failed && 'text-danger')}>{result}</Text></Section> : null}
    {check ? <ConfirmCard check={check} /> : null}
    {asking ? <QuestionCard key={asking.question_id} question={asking} inTask /> : null}
    {canStop && sharing.length ? <Section title="会一起停止">
      {sharing.map((item) => <View key={item.work_id} className="flex-row items-center gap-2">
        <Icon as={Link2} className="size-4 text-muted-foreground" /><Text numberOfLines={1} className="shrink">{item.title || '任务'}</Text>
      </View>)}
    </Section> : null}
    {canStop ? <ActionButton variant="outline" className="self-start" icon={Square} iconClassName="fill-foreground" disabled={!shared.writable} action={() => shared.interruptWork(work.work_id)}>
      {sharing.length ? `一起停止 ${sharing.length + 1} 个任务` : '停止任务'}
    </ActionButton> : null}
    {failed && device && fixOnDevice(work) ? <Button variant="outline" className="self-start" onPress={() => router.push(device === ready?.entry.device_id ? '/ai' : `/device/${device}`)}>
      <Icon as={Settings} /><Text>{fixOnDeviceLabel(nodeName(device, lookup))}</Text>
    </Button> : null}
    {failed || unknown || state === 'interrupted' ? <ActionButton variant="outline" className="self-start" icon={followUpIcons[next]} disabled={!shared.writable}
      action={async () => { if (await shared.sendMessage(followUpCopy[next].ask(work.title || '这个任务')) && shared.isCurrent()) router.back() }}>{followUpCopy[next].action}</ActionButton> : null}
    <Details>
      {asked.folded ? <Detail label="交代的原文">{work.prompt}</Detail> : null}
      {failed && work.result_text ? <Detail label="已有输出">{work.result_text}</Detail> : null}
      {work.failure_reason && rawError(work.failure_reason) ? <Detail label="原始错误">{work.failure_reason}</Detail> : null}
      {work.cwd ? <Detail label="文件夹" mono>{work.cwd}</Detail> : null}
      <View className="gap-0.5">
        <Text className="text-meta text-muted-foreground">任务编号</Text>
        <CopyableId value={work.work_id} label="任务编号" />
      </View>
    </Details>
  </ScrollView>
}

const followUpIcons: Record<FollowUp, LucideIcon> = { check: SearchCheck, resume: Play, retry: RotateCw }

const steps: Record<SummaryStep['state'], { icon: LucideIcon; label: string }> = {
  done: { icon: CircleCheck, label: '已完成' }, current: { icon: CircleDot, label: '进行中' }, pending: { icon: Circle, label: '待做' },
}

function SummarySteps({ steps: items }: { steps: SummaryStep[] }) {
  return <View className="gap-2">{items.map((item, index) => <View key={index} accessible accessibilityLabel={`${steps[item.state].label} ${item.text}`} className="flex-row items-start gap-2">
    <Icon as={steps[item.state].icon} className={cn('mt-0.5 size-4', item.state === 'done' ? 'text-success' : item.state === 'current' ? 'text-info' : 'text-muted-foreground')} /><Text selectable className="min-w-0 flex-1">{item.text}</Text>
  </View>)}</View>
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return <View className="gap-2">
    <Text role="heading" className="text-heading font-semibold">{title}</Text>
    {children}
  </View>
}

function Meta({ icon, label, children }: { icon: LucideIcon; label: string; children: string }) {
  return <View accessible accessibilityLabel={`${label} ${children}`} className="min-w-0 shrink flex-row items-center gap-1">
    <Icon as={icon} className="size-4 text-muted-foreground" />
    <Text numberOfLines={1} className="shrink text-meta text-muted-foreground">{children}</Text>
  </View>
}

function Detail({ label, mono, children }: { label: string; mono?: boolean; children: string }) {
  return <View className="gap-0.5">
    <Text className="text-meta text-muted-foreground">{label}</Text>
    <Text selectable className={cn(mono && 'font-mono text-meta')}>{children}</Text>
  </View>
}
