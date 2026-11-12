import { Stack, useFocusEffect, useLocalSearchParams, useRouter } from 'expo-router'
import { Bot, Circle, CircleCheck, CircleDot, FolderOpen, Laptop, Link2, ListChecks, Play, RotateCw, SearchCheck, Settings, Square, type LucideIcon } from 'lucide-react-native'
import { useCallback, useRef, useState, type ReactNode } from 'react'
import { File } from 'expo-file-system'
import { ScrollView, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { outcomeUnknown, rawError, workFailure } from '@vibeface/client/lib/errors.ts'
import { agentLaneLabels, fixOnDevice, fixOnDeviceLabel, followUp, followUpCopy, interruptedCopy, nodeName, parseSummarySteps, progressLabel, promptSummary, queueHoldLabels, schedulerStatus, shownState, staleTaskLabel, taskActive, taskAgent, taskLabel, taskStale, type FollowUp, type SummaryStep } from '@vibeface/client/workspace/labels.ts'
import { messageOf, providerNames } from '@vibeface/client/lib/node.ts'
import { projectName, workApproval } from '@vibeface/client/workspace/questions.ts'
import { workChecks, type WorkCheck } from '@vibeface/client/workspace/model.ts'
import { ActionButton, ErrorText } from '@/components/action'
import { CopyableId, Details, EmptyState, providerIcons, toneText, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { QuestionCard } from '@/features/chat/question-card'
import { ConfirmCard } from '@/features/chat/work'
import { cn } from '@/lib/utils'
import { Composer } from '@/features/chat/composer'
import { useChatChrome } from '@/features/chat/chrome'
import { InputStatus } from '@/features/workspace/workspace'
import type { WorkDetail } from '@vibeface/client/workspace/types.ts'
import { promptIntent } from '@vibeface/client/chat/protocol.ts'
import type { Attachment } from '@/data/types'

const tone = (state: string): Tone => state === 'failed' ? 'danger' : state === 'completed' ? 'success' : ['awaiting_confirmation', 'stop_timed_out'].includes(state) ? 'warning' : 'neutral'
type ComposerDraft = { text: string; files: Attachment[]; workId: string; conversationId: string }

/** One task of a bot's chat, as the web's task panel: progress, result, stop and the details behind it. */
export default function TaskDetailRoute() {
  const { botId, workId } = useLocalSearchParams<{ botId: string; workId: string }>()
  return <TaskDetail key={`${botId}:${workId}`} botId={botId} workId={workId} />
}

function TaskDetail({ botId, workId }: { botId: string; workId: string }) {
  const shared = useLive()
  const chrome = useChatChrome()
  const { directory, ready } = useDevice()
  const router = useRouter()
  const bot = shared?.bots.find((item) => item.bot_id === botId)
  const [error, setError] = useState<{ generation: number; botId: string; workId: string; text: string } | null>(null)
  const [detail, setDetail] = useState<{ generation: number; value: WorkDetail } | null>(null)
  const [retry, setRetry] = useState(0)
  const [text, setText] = useState('')
  const [files, setFiles] = useState<Attachment[]>([])
  const [sendError, setSendError] = useState('')
  const conversationId = bot?.conversation_id ?? ''
  const draftRef = useRef<ComposerDraft>({ text, files, workId, conversationId })
  if (draftRef.current.text !== text || draftRef.current.files !== files || draftRef.current.workId !== workId || draftRef.current.conversationId !== conversationId) draftRef.current = { text, files, workId, conversationId }
  const draft = draftRef.current
  const savingDrafts = useRef(new Set<ComposerDraft>())
  const routeActive = useRef(false)
  const latest = useRef(shared)
  latest.current = shared
  useFocusEffect(useCallback(() => {
    const shared = latest.current
    if (!shared) return
    shared.setView('other')
    routeActive.current = true
    let alive = true
    const current = () => alive && shared.isCurrent()
    if (bot && shared.connectionOnline) {
      setError(null)
      void (async () => {
        if (!shared.synced || shared.conversationId !== bot.conversation_id || shared.data?.conversation?.conversation_id !== bot.conversation_id) await shared.selectBot(bot)
        if (!current()) return
        const value = await shared.readWork({ conversation_id: bot.conversation_id, work_id: workId })
        if (current()) { setDetail({ generation: shared.generation, value }); setError(null) }
      })().catch((cause: unknown) => {
        if (current()) setError({ generation: shared.generation, botId, workId, text: messageOf(cause, '读取工作') })
      })
    }
    return () => { alive = false; routeActive.current = false }
  }, [bot?.conversation_id, workId, shared?.generation, shared?.connectionOnline, shared?.data, shared?.openWorks, retry]))
  const readError = error?.generation === shared?.generation && error?.botId === botId && error.workId === workId ? error.text : null
  const loaded = detail?.generation === shared?.generation && detail?.value.work.conversation_id === bot?.conversation_id && detail?.value.work.work_id === workId ? detail.value.work : null
  const work = loaded ? { ...loaded, target: loaded.target ?? undefined } : null
  if (!shared || !bot || !work) return <><Stack.Screen options={{ title: '任务' }} /><EmptyState icon={ListChecks}
    title={readError ?? (!shared || !shared.connectionOnline ? '工作状态未知，连接后再读取' : !bot ? '找不到这个 Bot' : '正在读取工作')}
    action={shared?.connectionOnline && bot ? <Button variant="outline" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button> : undefined} className="flex-1 justify-center" /></>

  const lookup = { nodes: shared.nodes, directory }
  const asking = workApproval({ work, conversationId: bot.conversation_id, questions: shared.questions, answered: shared.answered })
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
  const sharing = work.turn_id ? shared.openWorks.filter((item) => item.conversation_id === work.conversation_id && item.work_id !== work.work_id && !item.outcome && item.thread_id === work.thread_id && item.turn_id === work.turn_id) : []
  const active = taskActive(state)
  const next = followUp(state, unknown)
  const taskTarget = work.target?.kind === 'task' || work.target?.kind === 'channel_task' ? work.target : null
  const followUpLabel = unknown ? followUpCopy[next].action : taskTarget ? '继续所属任务' : state === 'interrupted' ? '新任务继续' : '新任务重做'
  // Its device cannot say whether it finished, so the person settles it here as in the chat; nothing is re-run.
  const check = state === 'awaiting_confirmation' ? workChecks([{ ...work, conversation_id: bot.conversation_id }], shared.nodes).find((item): item is WorkCheck => item.kind === 'work') : undefined
  const asked = promptSummary(work.prompt, work.title)
  const plan = parseSummarySteps(work.summary)
  const coordination = work.coordination_job
  const canContinue = !work.outcome && !work.stop && ["accepted", "running"].includes(work.state)
  // A work whose result is unknown may still write; new input waits until the result is checked.
  const inputPaused = !canContinue && (!work.outcome || unknown || work.outcome === "unverified")
  const inputIntent = canContinue ? promptIntent(text) : "new_task"
  return <KeyboardAwareScrollView keyboardShouldPersistTaps="handled" bottomOffset={16} contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: work.title || '任务' }} />
    <Button variant="ghost" className="self-start" accessibilityLabel={`打开所属 Bot ${bot.name}`} onPress={() => router.push(`/b/${bot.bot_id}`)}><Icon as={Bot} /><Text>{bot.name}</Text></Button>
    <Text className="text-meta text-muted-foreground">来源：共享历史中的工作记录。完整执行历史未提供。</Text>
    <Button variant="ghost" className="self-start" disabled={!shared.connectionOnline} onPress={() => setRetry((value) => value + 1)}><Text>重新读取工作</Text></Button>
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
      action={async () => {
        if (unknown) { await shared.refresh(); setRetry((value) => value + 1); return }
        let revision: number | undefined
        if (taskTarget) {
          const state = await shared.readWorkspaceState({ task_id: taskTarget.task_id })
          const task = state.tasks.find((item) => item.task_id === taskTarget.task_id)
          if (!task) throw new Error('尚未读到所属任务，请先查询任务。')
          if (task.action !== 'continue') { router.push(`/task/${task.task_id}`); return }
          revision = task.revision
        }
        const request = `${followUpCopy[next].ask(work.title || '这项工作')}\n\n原交代：\n${work.prompt}${work.result_text ? `\n\n上次已有输出：\n${work.result_text}` : ''}`
        await shared.sendMessage(request, [], undefined, { target: taskTarget ?? { kind: 'work', work_id: workId }, intent: taskTarget ? 'continue' : 'new_task', ...(revision === undefined ? {} : { expected_revision: revision }), conversation_id: work.conversation_id, onSaved: (saved) => { if (!routeActive.current || !shared.isCurrent()) return; if (saved.target.kind === 'task' || saved.target.kind === 'channel_task') router.push(`/task/${saved.target.task_id}`) } })
      }}>{followUpLabel}</ActionButton> : null}
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
    {shared.pendingMessages.filter((item) => item.target.kind === 'work' && item.target.work_id === workId).map((item) => <InputStatus key={item.message_id} pending={item} />)}
    <Composer chrome={chrome} value={text} onChange={setText} files={files} onFiles={setFiles} attachmentScope={JSON.stringify([ready?.principal.network_id ?? '', conversationId, ['work', workId]])} blocked={inputPaused ? (unknown || work.outcome === 'unverified' ? '先让 Bot 查结果，再继续。' : '这个工作当前不能接收补充。') : shared.canSaveMessages ? null : shared.readOnlyNotice || shared.writeNotice || '尚未完成首读'} sending={false} label={inputIntent === 'continue' ? `补充给 ${work.title || '这个工作'}` : '发送为新任务'} sendLabel={inputIntent === 'continue' ? '补充这个工作' : '发送为新任务'} onSend={() => {
      const submittedDraft = draft
      if (savingDrafts.current.has(submittedDraft)) return
      savingDrafts.current.add(submittedDraft)
      const submitted = text, selectedFiles = files
      setSendError('')
      void shared.sendMessage(submitted, selectedFiles.map((file) => ({ name: file.name, arrayBuffer: () => new File(file.uri).arrayBuffer() })), undefined, { target: { kind: 'work', work_id: workId }, intent: inputIntent, conversation_id: work.conversation_id, onSaved: (saved) => { if (!routeActive.current || !shared.isCurrent() || draftRef.current !== submittedDraft) return; if (saved.target.kind === 'task' || saved.target.kind === 'channel_task') router.push(`/task/${saved.target.task_id}`) } }).then((saved) => { if (saved && draftRef.current === submittedDraft) { setText((current) => current === submitted ? '' : current); setFiles((current) => current === selectedFiles ? [] : current) } }).catch((cause) => setSendError(messageOf(cause))).finally(() => { savingDrafts.current.delete(submittedDraft) })
    }}>{sendError ? <ErrorText error={sendError} /> : null}</Composer>
  </KeyboardAwareScrollView>
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
