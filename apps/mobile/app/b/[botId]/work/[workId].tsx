import { Stack, useLocalSearchParams, useRouter } from 'expo-router'
import { FolderOpen, Laptop, Link2, ListChecks, Play, RotateCw, SearchCheck, Settings, Square, type LucideIcon } from 'lucide-react-native'
import { useEffect, type ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { outcomeUnknown, rawError, workFailure } from '@vibeface/client/lib/errors.ts'
import { fixOnDevice, fixOnDeviceLabel, followUp, followUpCopy, interruptedCopy, nodeName, progressLabel, promptSummary, shownState, staleTaskLabel, taskActive, taskAgent, taskLabel, taskStale, type FollowUp } from '@vibeface/client/workspace/labels.ts'
import { providerNames } from '@vibeface/client/lib/node.ts'
import { projectName, questionOpen } from '@vibeface/client/workspace/questions.ts'
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
  // Opened from the chat, the bot is already selected; a cold link selects it.
  useEffect(() => { if (shared && bot && shared.conversationId !== bot.conversation_id) void shared.selectBot(bot).catch(() => {}) }, [bot?.conversation_id])
  const data = shared && bot && shared.conversationId === bot.conversation_id ? shared.data : null
  const work = data?.works.find((item) => item.work_id === workId)
  if (!shared || !work) return <><Stack.Screen options={{ title: '任务' }} /><EmptyState icon={ListChecks} title={data ? '找不到这个任务' : '稍等'} className="flex-1 justify-center" /></>

  const lookup = { nodes: shared.nodes, directory }
  const state = shownState(work, shared.nodes)
  const device = work.target_node
  const agent = taskAgent(work)
  const progress = shared.liveProgress.find((item) => item.kind === 'native' && item.work_id === workId && item.target_node === device)
  const canStop = state !== 'awaiting_confirmation' && !work.outcome && !work.stop && ['accepted', 'running'].includes(work.state)
  // A failed task's text is what it said before failing, so its cause leads and that text waits in 详细信息.
  const failed = ['failed', 'rejected', 'dispatch_rejected'].includes(state)
  const result = failed ? workFailure(work) || '这个任务没有成功。' : work.result_text || workFailure(work) || (state === 'completed' ? '没有返回文字' : state === 'interrupted' ? interruptedCopy.reason : '')
  // It may have done its writes anyway, so it is checked first; running it again could do them twice.
  const unknown = state === 'dispatch_unverified' || outcomeUnknown(work.failure, work.failure_reason)
  // The request this task waits on is answered here, as on the web, so the chat's task card only says it waits.
  const asking = work.thread_id && !work.outcome ? data!.questions.find((question) => questionOpen(question) && question.about.request_id && question.about.session_id === work.thread_id) : undefined
  // Stopping one input steered into a turn stops every work sharing that turn.
  const sharing = work.turn_id ? data!.works.filter((item) => item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const active = taskActive(state)
  const next = followUp(state, unknown)
  // Its device cannot say whether it finished, so the person settles it here as in the chat; nothing is re-run.
  const check = state === 'awaiting_confirmation' ? workChecks([{ ...work, conversation_id: bot!.conversation_id }], shared.nodes).find((item): item is WorkCheck => item.kind === 'work') : undefined
  const asked = promptSummary(work.prompt, work.title)
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: work.title || '任务' }} />
    <View className="gap-1">
      {taskStale(state, shared.connectionOnline) ? <Text className="font-medium text-muted-foreground">{staleTaskLabel}</Text> : <Text className={cn('font-medium', toneText[tone(state)])}>{taskLabel(state)}</Text>}
      {device || work.cwd || agent ? <View className="flex-row flex-wrap items-center gap-x-3 gap-y-1">
        {agent ? <Meta icon={providerIcons[agent]} label="AI">{providerNames[agent]}</Meta> : null}
        {device ? <Meta icon={Laptop} label="设备">{nodeName(device, lookup)}</Meta> : null}
        {work.cwd ? <Meta icon={FolderOpen} label="项目">{projectName(work.cwd)}</Meta> : null}
      </View> : null}
      {progress?.status && active && progress.status !== 'awaiting_input' && progressLabel(progress.status) !== '进行中' ? <Text className="text-meta text-muted-foreground">{progressLabel(progress.status)}</Text> : null}
      {!progress?.text && work.summary && active ? <Text selectable className="mt-2">{work.summary}</Text> : null}
      {progress?.text ? <ScrollView nestedScrollEnabled className="mt-3 max-h-96 rounded-xl bg-muted" contentContainerClassName="p-3">
        <Text selectable className="font-mono text-meta">{progress.text}</Text>
      </ScrollView> : null}
    </View>
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
      action={async () => { if (await shared.sendMessage(followUpCopy[next].ask(work.title || '这个任务'))) router.back() }}>{followUpCopy[next].action}</ActionButton> : null}
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
