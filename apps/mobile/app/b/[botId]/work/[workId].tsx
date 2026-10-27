import { Stack, useLocalSearchParams, useRouter } from 'expo-router'
import { Link2, ListChecks, RotateCw, Square } from 'lucide-react-native'
import { useEffect, type ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { failureText, rawError, taskError } from '@vibeface/client/lib/errors.ts'
import { nodeName, progressLabel, shownState, taskActive, taskLabel } from '@vibeface/client/workspace/labels.ts'
import { confirmReason } from '@vibeface/client/workspace/model.ts'
import { ActionButton } from '@/components/action'
import { CopyableId, EmptyState, toneText, type Tone } from '@/components/kit'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { confirmText } from '@/features/chat/work'
import { cn } from '@/lib/utils'

const tone = (state: string): Tone => state === 'failed' ? 'danger' : state === 'completed' ? 'success' : ['awaiting_confirmation', 'stop_timed_out'].includes(state) ? 'warning' : 'neutral'

/** One task of a bot's chat, as the web's task panel: progress, result, stop and the details behind it. */
export default function TaskDetail() {
  const { botId, workId } = useLocalSearchParams<{ botId: string; workId: string }>()
  const shared = useLive()
  const { directory } = useDevice()
  const router = useRouter()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  // Opened from the chat, the bot is already selected; a cold link selects it.
  useEffect(() => { if (shared && bot && shared.conversationId !== bot.conversation_id) void shared.selectBot(bot).catch(() => {}) }, [bot?.conversation_id])
  const data = shared && bot && shared.conversationId === bot.conversation_id ? shared.data : null
  const work = data?.works.find((item) => item.work_id === workId)
  if (!shared || !work) return <><Stack.Screen options={{ title: '任务' }} /><EmptyState icon={ListChecks} title={data ? '找不到这个任务' : '稍等'} /></>

  const lookup = { nodes: shared.nodes, directory }
  const state = shownState(work, shared.nodes)
  const device = work.target_node
  const progress = shared.liveProgress.find((item) => item.kind === 'native' && item.work_id === workId && item.target_node === device)
  const canStop = state !== 'awaiting_confirmation' && !work.outcome && !work.stop && ['accepted', 'running'].includes(work.state)
  const result = work.result_text || failureText(work.failure) || (work.failure_reason ? taskError(work.failure_reason) : state === 'completed' ? '没有返回文字' : '')
  // Stopping one input steered into a turn stops every work sharing that turn.
  const sharing = work.turn_id ? data!.works.filter((item) => item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const active = taskActive(state)
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-8 p-4 pb-10">
    <Stack.Screen options={{ title: work.title || '任务' }} />
    <View className="gap-1">
      <View className="flex-row flex-wrap items-center gap-x-2">
        <Text className={cn('font-medium', toneText[tone(state)])}>{taskLabel(state)}</Text>
        {device ? <Text className="text-meta text-muted-foreground">在 {nodeName(device, lookup)} 上</Text> : null}
      </View>
      {progress?.status && active && progress.status !== 'awaiting_input' && progressLabel(progress.status) !== '进行中' ? <Text className="text-meta text-muted-foreground">{progressLabel(progress.status)}</Text> : null}
      {!progress?.text && work.summary && active ? <Text selectable className="mt-2">{work.summary}</Text> : null}
      {progress?.text ? <ScrollView nestedScrollEnabled className="mt-3 max-h-96 rounded-xl bg-muted" contentContainerClassName="p-3">
        <Text selectable className="font-mono text-meta">{progress.text}</Text>
      </ScrollView> : null}
    </View>
    {result && !active ? <Section title={work.result_text || state === 'completed' ? '结果' : '原因'}><Text selectable>{result}</Text></Section> : null}
    {state === 'awaiting_confirmation' ? <Text className="text-muted-foreground">
      {confirmText({ reason: confirmReason(work, shared.nodes) ?? 'unverified', detail: work.failure_reason ?? '' }, nodeName(device, lookup))}请在对话或待处理里确认它做完了没有。
    </Text> : null}
    {canStop && sharing.length ? <Section title="会一起停止">
      {sharing.map((item) => <View key={item.work_id} className="flex-row items-center gap-2">
        <Icon as={Link2} className="size-4 text-muted-foreground" /><Text numberOfLines={1} className="shrink">{item.title || '任务'}</Text>
      </View>)}
    </Section> : null}
    {canStop ? <ActionButton variant="outline" className="min-h-11 self-start" icon={Square} iconClassName="fill-foreground" disabled={!shared.writable} action={() => shared.interruptWork(work.work_id)}>
      {sharing.length ? `一起停止 ${sharing.length + 1} 个任务` : '停止任务'}
    </ActionButton> : null}
    {['failed', 'interrupted', 'rejected'].includes(state) ? <ActionButton variant="outline" className="min-h-11 self-start" icon={RotateCw} disabled={!shared.writable}
      action={async () => { if (await shared.sendMessage(`再试一次「${work.title || '这个任务'}」`)) router.back() }}>让 Bot 再试</ActionButton> : null}
    <Section title="详情">
      {work.prompt ? <Detail label="交代的事">{work.prompt}</Detail> : null}
      {work.failure_reason && rawError(work.failure_reason) ? <Detail label="原始错误">{work.failure_reason}</Detail> : null}
      {work.cwd ? <Detail label="文件夹" mono>{work.cwd}</Detail> : null}
      <View className="gap-0.5">
        <Text className="text-meta text-muted-foreground">任务编号</Text>
        <CopyableId value={work.work_id} label="任务编号" />
      </View>
    </Section>
  </ScrollView>
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return <View className="gap-2">
    <Text role="heading" className="text-heading font-semibold">{title}</Text>
    {children}
  </View>
}

function Detail({ label, mono, children }: { label: string; mono?: boolean; children: string }) {
  return <View className="gap-0.5">
    <Text className="text-meta text-muted-foreground">{label}</Text>
    <Text selectable className={cn(mono && 'font-mono text-meta')}>{children}</Text>
  </View>
}
