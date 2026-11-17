import { useSelectionOwner } from '@/lib/use-selection-owner'
import { Card } from '@/components/arc/surface'
import { SelectionItem } from '@/components/arc/selection'
import { Link, Stack, useFocusEffect, useRouter } from 'expo-router'
import { BrainCircuit, ChevronRight, CircleQuestionMark, ListChecks, MessagesSquare, Plus, Reply, Settings, X } from 'lucide-react-native'
import { useCallback, useEffect, useRef, useState } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { KeyboardAwareScrollView, KeyboardStickyView } from 'react-native-keyboard-controller'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { inputTargetKey, promptIntent } from '@vibeface/client/chat/protocol.ts'
import { networkKey, newId, storageOf } from '@vibeface/client/workspace/storage.ts'
import type { ContextProjection, InputReceipt, InputTarget, PendingMessage, ScopeContext, WorkspaceChannel, WorkspaceCommand, WorkspaceHistory, WorkspaceTask } from '@vibeface/client/workspace/types.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { Details, EmptyState } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'
import { ErrorText } from '@/components/action'
import { useLive } from '@/data/live'
import type { Attachment } from '@/data/types'
import { Composer } from '@/features/chat/composer'
import { Markdown } from '@/features/chat/markdown'
import { QuestionCard } from '@/features/chat/question-card'
import { mobilePlatform } from '@/platform'
import { useRun } from '@/lib/use-run'
import { cn } from '@/lib/utils'

const taskLabels: Record<WorkspaceTask['state'], string> = {
  not_started: '未开始', running: '执行中', waiting_dependencies: '等待依赖', waiting_user: '等待你决定', update_pending: '更新待承接',
  stop_pending: '停止待确认', needs_reconciliation: '结果待确认', completed: '已完成', cancelled: '已取消', deferred: '稍后处理',
}
const receiptLabels: Record<InputReceipt['stage'], string> = {
  entry_accepted: '连接的设备已接收', owner_accepted: '执行设备已接纳', submitting: '提交已登记，等待执行服务确认', provider_accepted: '执行服务已接受输入',
  associated: '已确认回合归属', refused: '已拒绝', unknown: '补充是否进入当前工作尚未确认',
}
const deliveryLabels: Record<PendingMessage['delivery'], string> = { saved: '本地已保存', uploading: '本地已保存，附件准备中', submitting: '正在发送给连接的设备', accepted: '连接的设备已接收', unknown: '是否送达尚未确认' }
type ComposerDraft = { text: string; files: Attachment[]; target: string; conversationId: string }

export function ScopeSources({ context }: { context: ContextProjection }) {
  const kinds: Record<ContextProjection['sources'][number]['kind'], string> = { trigger: '本轮交代', message: '共享消息', work: '工作结果', dispatch: '工作指派', checkpoint: '历史摘要', handoff: '任务交接', memory: '持久记忆', question: '已知问题', attachment: '附件' }
  return <Card className="gap-3">
    <Text role="heading" className="font-medium">本轮上下文引用</Text>
    <Text className="text-sm text-muted">{context.stage === 'compiled' ? '上下文已整理，尚未确认执行服务收到。' : '以下来源来自这个执行范围的上下文编译记录。'}</Text>
    <View className="flex-row flex-wrap gap-x-4 gap-y-1">{Object.entries(kinds).map(([kind, name]) => { const count = context.sources.filter((source) => source.kind === kind).length; return count ? <Text key={kind} className="text-sm">{name} {count} 项</Text> : null })}</View>
    <Text className="text-sm">因预算省略 · 消息 {context.coverage.messages_omitted} 条 · 附件 {context.coverage.attachments_omitted} 个 · 记忆 {context.coverage.memory_omitted} 条</Text>
    {context.sources_omitted ? <Text className="text-sm text-muted">另有 {context.sources_omitted} 个来源引用未展开。</Text> : null}
    <Details label="查看来源引用">{context.sources.map((source, index) => <Text selectable key={`${source.kind}:${index}`} className="text-sm">{kinds[source.kind]} · {source.reference ?? source.event_id ?? (source.author ? `${source.author}:${source.seq ?? ''}` : '未提供引用编号')}</Text>)}</Details>
    <Text role="heading" className="font-medium">补充归属</Text>
    {context.inputs.length ? context.inputs.map((input, index) => <View key={input.message_id} className="gap-1 rounded-control bg-surface-muted p-3"><Text className="text-sm">第 {index + 1} 条补充 · {receiptLabels[input.stage]}</Text><Details label="消息和回合编号"><Text selectable className="text-sm">消息 · {input.message_id}</Text><Text selectable className="text-sm">目标回合 · {input.expected_turn_id}</Text><Text selectable className="text-sm">实际回合 · {input.actual_turn_id ?? '尚未确认'}</Text></Details></View>) : <Text className="text-sm text-muted">这个范围的上下文记录没有补充回执。</Text>}
    {context.inputs_omitted ? <Text className="text-sm text-muted">另有 {context.inputs_omitted} 条补充回执未展开。</Text> : null}
  </Card>
}

export function WorkspaceTaskRow({ task }: { task: WorkspaceTask }) {
  const shared = useLive()
  return <Link href={`/task/${task.task_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`${task.title}，${taskLabels[task.state]}`} className="min-h-16 gap-2 rounded-panel border border-border bg-surface p-4 active:bg-surface-muted">
    <View className="flex-row items-start gap-2"><Text className="min-w-0 flex-1 font-medium">{task.title}</Text><Text className={cn('text-sm', task.state === 'running' ? 'text-info' : ['needs_reconciliation', 'waiting_user'].includes(task.state) ? 'text-warning' : 'text-muted')}>{taskLabels[task.state]}</Text><Icon as={ChevronRight} className="size-4 text-muted" /></View>
    <Text className="text-sm text-muted">{shared?.bots.find((bot) => bot.bot_id === task.owner_bot)?.name ?? 'Bot 未加载'} · 第 {task.revision} 版{task.channel_id ? ' · 频道任务' : ''}</Text>
    {task.waiting_reason ? <Text className="text-sm text-warning">{task.waiting_reason}</Text> : null}
  </Pressable></Link>
}

export function WorkspaceTasks({ botFilter = 'all', filter = 'all' }: { botFilter?: string; filter?: 'all' | 'attention' | 'working' | 'waiting' }) {
  const shared = useLive()
  const latest = useRef(shared)
  latest.current = shared
  useEffect(() => { void latest.current?.readWorkspaceState().catch(() => {}) }, [shared?.generation])
  const groups = { attention: ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'], working: ['running'], waiting: ['not_started', 'waiting_dependencies', 'deferred'] }
  const tasks = (shared?.workspaceState?.tasks ?? []).filter((task) => (botFilter === 'all' || task.owner_bot === botFilter) && (filter === 'all' || groups[filter].includes(task.state)))
  return <View className="gap-3"><View className="flex-row items-center justify-between gap-3"><Text role="heading" className="text-lg font-medium">任务对话</Text><Link href="/new-task" asChild><Button variant="secondary" disabled={!shared?.writable || !shared.bots.length}><Icon as={Plus} /><Text>新任务</Text></Button></Link></View><Text className="text-sm text-muted">每个任务保留独立对话，后续补充留在同一个任务。</Text>{tasks.map((task) => <WorkspaceTaskRow key={task.task_id} task={task} />)}{!tasks.length ? <Text className="py-3 text-sm text-muted">{shared?.workspaceState ? '还没有任务对话。选择 Bot 新建任务。' : '正在读取任务对话。'}</Text> : null}</View>
}

export function CreateTaskScreen() {
  const router = useRouter()
  const shared = useLive()
  const { busy, error, run } = useRun()
  const [botId, setBotId] = useState(shared?.selectedBot?.bot_id ?? shared?.bots[0]?.bot_id ?? '')
  const [title, setTitle] = useState('')
  const [request, setRequest] = useState('')
  const taskId = useRef(newId())
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_task' }> | null>(null)
  return <KeyboardAwareScrollView mode="layout" keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12"><Stack.Screen options={{ title: '新任务' }} />
    <Text className="text-sm text-muted">选择 Bot，任务后续的补充留在同一个对话里。</Text><Text className="text-sm font-medium">交给哪个 Bot</Text><View className="flex-row flex-wrap gap-2">{shared?.bots.map((bot) => <Button key={bot.bot_id} variant={botId === bot.bot_id ? 'primary' : 'secondary'} disabled={Boolean(submission.current)} onPress={() => setBotId(bot.bot_id)} accessibilityState={{ selected: botId === bot.bot_id }}><Text>{bot.name}</Text></Button>)}</View>
    <View className="gap-2"><Text nativeID="task-title-label" className="text-sm font-medium">任务名称</Text><Input editable={!submission.current} accessibilityLabel="任务名称" accessibilityLabelledBy="task-title-label" value={title} onChangeText={setTitle} maxLength={512} placeholder="例如：检查官网导航" /></View>
    <View className="gap-2"><Text nativeID="task-request-label" className="text-sm font-medium">交代内容</Text><Input editable={!submission.current} accessibilityLabel="交代内容" accessibilityLabelledBy="task-request-label" multiline className="min-h-36 py-3" textAlignVertical="top" value={request} onChangeText={setRequest} maxLength={100000} placeholder="描述要完成的事情和需要保留的约束" /></View>
    {error ? <ErrorText error={error} /> : null}<Button disabled={Boolean(busy) || !shared?.writable} onPress={() => void run('create-task', async () => {
      const bot = shared?.bots.find((item) => item.bot_id === (submission.current?.owner_bot ?? botId))
      if (!shared || !bot || !title.trim() || !request.trim()) throw new Error('请选择 Bot，并填写任务名称和交代内容。')
      const command: Extract<WorkspaceCommand, { kind: 'create_task' }> = submission.current ?? { kind: 'create_task', task_id: taskId.current, owner_bot: bot.bot_id, title: title.trim(), request: request.trim(), expected_result: title.trim(), source_events: [], dependencies: [], reply_only: false }
      submission.current = command
      const result = await shared.workspaceCommand(command)
      const task = result.tasks.find((item) => item.task_id === taskId.current)
      if (!task) throw new Error('任务已提交，尚未读到任务记录。请重新查询。')
      if (!await shared.sendMessage(command.request, [], undefined, { target: { kind: 'task', task_id: task.task_id }, expected_revision: task.revision, conversation_id: bot.conversation_id })) throw new Error('任务已创建，首次交代尚未保存。请重试保存。')
      router.replace(`/task/${task.task_id}`)
    })}><Text>{busy ? '正在保存任务' : '创建并发送'}</Text></Button>
  </KeyboardAwareScrollView>
}

export function ChannelsScreen() {
  const shared = useLive()
  const latest = useRef(shared)
  latest.current = shared
  const { error, run } = useRun()
  useEffect(() => { void latest.current?.readWorkspaceState().catch(() => {}) }, [shared?.generation])
  const channels = shared?.workspaceState?.channels ?? []
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12"><Stack.Screen options={{ title: '频道' }} /><Link href="/new-channel" asChild><Button disabled={!shared?.writable || !shared.bots.length}><Icon as={Plus} /><Text>新频道</Text></Button></Link>{channels.map((channel) => <Link key={channel.channel_id} href={`/channel/${channel.channel_id}`} asChild><Pressable accessibilityRole="link" className="min-h-16 gap-2 rounded-panel border border-border bg-surface p-4 active:bg-surface-muted"><View className="flex-row items-center gap-2"><Icon as={MessagesSquare} className="text-muted" /><Text className="min-w-0 flex-1 font-medium">{channel.title}</Text><Icon as={ChevronRight} className="size-4 text-muted" /></View>{channel.purpose ? <Text className="text-sm text-muted">{channel.purpose}</Text> : null}<Text className="text-sm text-muted">{channel.members.length} 个 Bot · 默认协调 {shared?.bots.find((bot) => bot.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</Text></Pressable></Link>)}{!channels.length ? <EmptyState icon={MessagesSquare} title={shared?.workspaceState ? '还没有频道' : '正在读取频道'} action={<Button variant="secondary" onPress={() => void run('read-channels', async () => { await shared?.readWorkspaceState() })}><Text>重新读取</Text></Button>} /> : null}{error ? <ErrorText error={error} /> : null}</ScrollView>
}

type ChannelFields = Pick<WorkspaceChannel, 'channel_id' | 'title' | 'purpose' | 'members' | 'coordinator_bot' | 'coordinator_node'>
type ChannelDraft = ChannelFields & (
  | { kind: 'create' }
  | { kind: 'edit'; expected_revision: WorkspaceChannel['revision']; owner_epoch: WorkspaceChannel['owner_epoch'] }
)
type ChannelSource = { kind: 'create'; node: string } | { kind: 'edit'; channel: WorkspaceChannel }
type ChannelRead = { owner: string } & (
  | { kind: 'loading' }
  | { kind: 'ready'; channel: WorkspaceChannel }
  | { kind: 'missing' }
  | { kind: 'error'; error: string }
)

export function ChannelSettingsScreen({ channelId }: { channelId?: string }) {
  const shared = useLive()
  const { ready } = useDevice()
  const owner = JSON.stringify([ready?.principal.network_id, ready?.principal.device_id, shared?.generation, channelId ?? null])
  const currentOwner = useRef(owner)
  currentOwner.current = owner
  const latest = useRef(shared)
  latest.current = shared
  const [read, setRead] = useState<ChannelRead>({ owner, kind: 'loading' })
  const [attempt, setAttempt] = useState(0)
  const connected = Boolean(shared && ready)
  useEffect(() => {
    const workspace = latest.current
    if (channelId === undefined || !connected || !workspace) return
    let active = true
    const current = () => active && currentOwner.current === owner && workspace.isCurrent()
    setRead({ owner, kind: 'loading' })
    void (async () => {
      if (!current()) return
      try {
        const result = await workspace.readWorkspaceState({ channel_id: channelId })
        if (!current()) return
        const channel = result.channels.find((item) => item.channel_id === channelId)
        setRead(channel ? { owner, kind: 'ready', channel } : { owner, kind: 'missing' })
      } catch (cause) { if (current()) setRead({ owner, kind: 'error', error: messageOf(cause) }) }
    })()
    return () => { active = false }
  }, [owner, connected, attempt])
  const ownedRead = read.owner === owner ? read : null
  const cached = shared?.workspaceState?.channels.find((item) => item.channel_id === channelId)
  const loaded = ownedRead?.kind === 'ready' ? ownedRead.channel : null
  const channel = loaded && (!cached || loaded.revision > cached.revision) ? loaded : cached
  if (!shared || !ready || channelId !== undefined && (ownedRead?.kind !== 'ready' || !channel)) {
    const title = ownedRead?.kind === 'error' ? ownedRead.error : ownedRead?.kind === 'missing' ? '没有找到这个频道' : '正在读取频道设置'
    const retry = ownedRead?.kind === 'error' || ownedRead?.kind === 'missing'
    return <View className="flex-1"><Stack.Screen options={{ title: channelId !== undefined ? '频道设置' : '新频道' }} /><EmptyState icon={MessagesSquare} title={title} className="flex-1 justify-center" action={retry ? <Button variant="secondary" onPress={() => setAttempt((current) => current + 1)}><Text>重新读取</Text></Button> : undefined} /></View>
  }
  const source: ChannelSource = channelId !== undefined && channel ? { kind: 'edit', channel } : { kind: 'create', node: ready.entry.device_id }
  return <ChannelForm key={owner} source={source} shared={shared} isCurrent={() => currentOwner.current === owner && shared.isCurrent()} />
}

function ChannelForm({ source, shared, isCurrent }: { source: ChannelSource; shared: NonNullable<ReturnType<typeof useLive>>; isCurrent: () => boolean }) {
  const router = useRouter()
  const [draft, setDraft] = useState<ChannelDraft>(() => source.kind === 'edit' ? {
    kind: 'edit', channel_id: source.channel.channel_id, title: source.channel.title, purpose: source.channel.purpose,
    members: [...source.channel.members], coordinator_bot: source.channel.coordinator_bot, coordinator_node: source.channel.coordinator_node,
    expected_revision: source.channel.revision, owner_epoch: source.channel.owner_epoch,
  } : {
    kind: 'create', channel_id: newId(), title: '', purpose: '', members: shared.bots.map((bot) => bot.bot_id),
    coordinator_bot: shared.bots[0]?.bot_id ?? '', coordinator_node: source.node,
  })
  const { title, purpose, members, coordinator_bot: coordinator, coordinator_node: node } = draft
  const conflict = draft.kind === 'edit' && source.kind === 'edit' && (draft.expected_revision !== source.channel.revision || draft.owner_epoch !== source.channel.owner_epoch)
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> | null>(null)
  const active = useRef(true)
  useEffect(() => { active.current = true; return () => { active.current = false } }, [])
  const { error, busy, run } = useRun()
  return <KeyboardAwareScrollView mode="layout" keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12"><Stack.Screen options={{ title: draft.kind === 'edit' ? '频道设置' : '新频道' }} />
    <View className="gap-2"><Text nativeID="channel-title-label" className="text-sm font-medium">频道名称</Text><Input editable={!submission.current} accessibilityLabel="频道名称" accessibilityLabelledBy="channel-title-label" value={title} onChangeText={(value) => setDraft((current) => ({ ...current, title: value }))} maxLength={512} placeholder="例如：官网改版" /></View><View className="gap-2"><Text nativeID="channel-purpose-label" className="text-sm font-medium">频道目标</Text><Input editable={!submission.current} accessibilityLabel="频道目标" accessibilityLabelledBy="channel-purpose-label" value={purpose} onChangeText={(value) => setDraft((current) => ({ ...current, purpose: value }))} maxLength={8192} multiline placeholder="说明这个频道要完成什么" /></View>
    <Text role="heading" className="text-sm font-medium">Bot 成员</Text><View className="gap-2">{shared.bots.map((bot) => <SelectionItem key={bot.bot_id} accessibilityRole="checkbox" accessibilityState={{ checked: members.includes(bot.bot_id) }} accessibilityLabel={bot.name} disabled={Boolean(submission.current)} className={cn('min-h-11 flex-row items-center gap-3 rounded-panel border px-3 py-2', members.includes(bot.bot_id) ? 'border-accent bg-accent-subtle' : 'border-border bg-surface')} onPress={() => setDraft((current) => ({ ...current, members: current.members.includes(bot.bot_id) ? current.members.filter((value) => value !== bot.bot_id) : [...current.members, bot.bot_id] }))}><Text className="flex-1">{bot.name}</Text><Text>{members.includes(bot.bot_id) ? '已加入' : '加入'}</Text></SelectionItem>)}</View>
    <Text role="heading" className="text-sm font-medium">默认协调 Bot</Text><View className="flex-row flex-wrap gap-2">{shared.bots.filter((bot) => members.includes(bot.bot_id)).map((bot) => <Button key={bot.bot_id} variant={coordinator === bot.bot_id ? 'primary' : 'secondary'} disabled={Boolean(submission.current)} onPress={() => setDraft((current) => ({ ...current, coordinator_bot: bot.bot_id }))} accessibilityState={{ selected: coordinator === bot.bot_id }}><Text>{bot.name}</Text></Button>)}</View>
    <Text role="heading" className="text-sm font-medium">协调设备</Text><View className="gap-2">{shared.nodes.filter((item) => !item.revoked).map((item) => <Button key={item.device_id} variant={node === item.device_id ? 'primary' : 'secondary'} disabled={Boolean(submission.current)} onPress={() => setDraft((current) => ({ ...current, coordinator_node: item.device_id }))} accessibilityState={{ selected: node === item.device_id }}><Text>{item.name}{item.online ? '' : ' · 离线'}</Text></Button>)}</View>
    <Text className="text-sm text-muted">频道消息由默认协调 Bot 承接。</Text>{conflict ? <Text role="status" className="text-sm text-warning">频道设置已在其他地方更新。当前编辑已保留，保存仍使用打开时的版本；请返回后重新打开以读取最新设置。</Text> : null}{error ? <ErrorText error={error} /> : null}<Button disabled={Boolean(busy) || !shared.writable} onPress={() => void run('save-channel', async () => {
      if (!isCurrent() || !active.current) return
      if (!title.trim() || !members.length || !members.includes(coordinator)) throw new Error('请填写频道名称，选择成员并将默认协调 Bot 加入成员。')
      const configuration = { channel_id: draft.channel_id, title: title.trim(), purpose: purpose.trim(), members, coordinator_bot: coordinator, coordinator_node: node }
      const command: Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> = submission.current ?? (draft.kind === 'edit' ? { kind: 'configure_channel', ...configuration, expected_revision: draft.expected_revision, owner_epoch: draft.owner_epoch } : { kind: 'create_channel', ...configuration })
      submission.current = command
      await shared.workspaceCommand(command)
      if (active.current && isCurrent()) router.replace(`/channel/${command.channel_id}`)
    })}><Text>{busy ? '正在保存' : draft.kind === 'edit' ? '保存频道' : '创建频道'}</Text></Button>
  </KeyboardAwareScrollView>
}

function sameTarget(left: InputTarget, right: InputTarget) {
  if ((left.kind === 'task' || left.kind === 'channel_task') && (right.kind === 'task' || right.kind === 'channel_task')) return left.task_id === right.task_id
  return JSON.stringify(left) === JSON.stringify(right)
}

export function InputLocation({ target, conversationId }: { target: InputTarget; conversationId: string }) {
  const shared = useLive()
  const bot = shared?.bots.find((item) => item.conversation_id === conversationId)
  const href = target.kind === 'task' || target.kind === 'channel_task' ? `/task/${target.task_id}` : target.kind === 'channel_member' ? `/channel/${target.channel_id}` : target.kind === 'work' && bot ? `/b/${bot.bot_id}/work/${target.work_id}` : bot ? `/b/${bot.bot_id}${target.kind === 'routine' ? `?routine=${encodeURIComponent(target.trigger)}` : ''}` : null
  return href ? <Link href={href} asChild><Button variant="ghost" className="self-start"><Text>{target.kind === 'task' || target.kind === 'channel_task' ? '打开所属任务' : target.kind === 'work' ? '打开所属工作' : target.kind === 'channel_member' ? '打开所属频道' : '打开所属 Bot'}</Text></Button></Link> : null
}

export function InputStatus({ pending }: { pending: PendingMessage }) {
  const shared = useLive()
  const { error, busy, run } = useRun()
  const receipt = pending.receipt
  const queued = shared?.workspaceState?.inputs.find((item) => item.conversation_id === pending.conversation_id && item.input.message_id === pending.message_id)
  const releasePending = pending.release_expected_event !== undefined || pending.release_command_id !== undefined
  const inputMissing = pending.rejection_code === 'input_missing'
  const canClose = !inputMissing && pending.delivery === 'accepted' && receipt?.stage === 'refused' && pending.rejection_code !== 'input_conflict' && !releasePending
    && receipt.message_id === pending.message_id && inputTargetKey(receipt.target) === inputTargetKey(pending.target)
  const canDiscard = !inputMissing && pending.delivery === 'saved' && receipt === undefined && pending.rejection_code !== 'input_conflict' && !releasePending
  const canRetry = !inputMissing && shared?.writable && !releasePending && !queued?.cancelled && !queued?.deferred && (pending.delivery === 'saved' && pending.rejection_code === 'input_not_saved' && pending.entry_device_id !== undefined && receipt === undefined || pending.delivery === 'accepted' && receipt?.stage === 'refused' && receipt.retry_safe === true && Boolean(receipt.control_event?.trim()) && pending.rejection_code !== 'input_conflict'
    && receipt.message_id === pending.message_id && inputTargetKey(receipt.target) === inputTargetKey(pending.target))
  const confirmEntry = pending.entry_device_id === undefined && (pending.delivery === 'unknown' || pending.delivery === 'submitting') && !releasePending && shared?.connectionOnline ? shared.info : null
  return <View role="status" className="gap-2 rounded-control bg-surface-muted/60 px-3 py-2"><Text className={cn('text-sm', pending.rejected || pending.delivery === 'unknown' || receipt?.stage === 'unknown' ? 'text-warning' : 'text-muted')}>{inputMissing ? '这条输入的送达结果仍未确认' : pending.rejected ? `已拒绝 · ${pending.rejected}` : pending.delivery === 'unknown' && receipt?.stage === 'refused' && receipt.retry_safe ? '重试请求待确认' : receipt ? receiptLabels[receipt.stage] : deliveryLabels[pending.delivery]}</Text>{receipt?.actual_turn_id ? <Text selectable className="text-sm text-muted">实际回合 · {receipt.actual_turn_id}</Text> : null}{receipt?.reason ? <Text className="text-sm text-muted">{receipt.reason}</Text> : null}{inputMissing ? <Text className="text-sm text-muted">原内容保存在此应用。请连接最初接收它的设备，再确认原设备并查询。不会自动重发。</Text> : null}{confirmEntry ? <Text className="text-sm text-muted">这条输入没有记录原设备。只有最初发给「{confirmEntry.name || '当前连接的设备'} · {confirmEntry.device_id.slice(0, 8)}」时，才确认原设备并查询。查询不会重新发送。</Text> : null}{confirmEntry ? <Button variant="secondary" className="self-start" disabled={Boolean(busy)} onPress={() => void run('confirm-input-entry', async () => { await shared?.confirmInputEntry(pending.message_id, confirmEntry.device_id) })}><Text>确认原设备并查询</Text></Button> : null}<View className="flex-row flex-wrap gap-2">{!inputMissing ? <Button variant="ghost" disabled={Boolean(busy)} onPress={() => void run('query-input', async () => { await shared?.queryInput({ message_id: pending.message_id, target: pending.target, conversation_id: pending.conversation_id }) })}><Icon as={CircleQuestionMark} /><Text>查询结果</Text></Button> : null}{canRetry ? <Button variant="secondary" disabled={Boolean(busy)} onPress={() => { if (canRetry && shared) void run('retry-input', () => shared.retryPendingMessage(pending.message_id)) }}><Text>重试原输入</Text></Button> : null}{canClose || canDiscard ? <Button variant="ghost" disabled={Boolean(busy)} onPress={() => void run('release-input', async () => { await shared?.releaseMessagePending(pending.message_id) })}><Text>{canClose ? '关闭提醒' : '不再发送'}</Text></Button> : null}</View>{error ? <ErrorText error={error} /> : null}</View>
}

export function ScopedWorkspaceScreen({ taskId, channelId }: { taskId?: string; channelId?: string }) {
  const router = useRouter()
  const shared = useLive()
  const { ready } = useDevice()
  const insets = useSafeAreaInsets()
  const [history, setHistory] = useState<WorkspaceHistory | null>(null)
  const [historyQuery, setHistoryQuery] = useState('')
  const storage = storageOf(mobilePlatform.storage)
  const selectionKey = networkKey(`channel-target:${channelId}`, ready?.principal.network_id ?? '')
  const cached = channelId ? storage.readStorage(selectionKey, null) : null
  const cachedBot = cached && typeof cached === 'object' && 'bot_id' in cached && typeof cached.bot_id === 'string' ? cached.bot_id : ''
  const cachedTask = cached && typeof cached === 'object' && 'task_id' in cached && typeof cached.task_id === 'string' ? cached.task_id : ''
  const [selected, setSelected] = useState(cachedTask ? `task:${cachedTask}` : '')
  const [recipient, setRecipient] = useState(cachedTask ? cachedBot : '')
  const [savedInput, setSavedInput] = useState<PendingMessage | null>(null)
  const selectionRevision = useRef(0)
  const savedSelection = selectionRevision.current
  const [cleanupError, setCleanupError] = useState('')
  const [files, setFiles] = useState<Attachment[]>([])
  const filesRef = useRef(files)
  const selection = useSelectionOwner<Attachment>(ready?.principal.network_id, () => { filesRef.current = []; setFiles([]) }, (cause) => setCleanupError(messageOf(cause)))
  const changeFiles = (next: Attachment[]) => {
    if (filesRef.current === next) return
    if (!selection.current?.replace(next)) return
    filesRef.current = next
    if (draftRef.current) draftRef.current = { ...draftRef.current, files: next }
    setFiles(next)
  }
  const [error, setError] = useState('')
  const { error: actionError, busy: actionBusy, run } = useRun()
  const routeActive = useRef(false)
  useFocusEffect(useCallback(() => { routeActive.current = true; return () => { routeActive.current = false } }, [taskId, channelId, shared?.generation]))
  const latest = useRef(shared)
  latest.current = shared
  const channel = shared?.workspaceState?.channels.find((item) => item.channel_id === channelId)
  const tasks = (shared?.workspaceState?.tasks ?? []).filter((item) => channelId ? item.channel_id === channelId : item.task_id === taskId)
  const selectedTaskId = taskId ?? (selected.startsWith('task:') ? selected.slice(5) : undefined)
  const task = selectedTaskId ? tasks.find((item) => item.task_id === selectedTaskId) : tasks.findLast((item) => item.owner_bot === (recipient || channel?.coordinator_bot) && !['completed', 'cancelled'].includes(item.state))
  const pendingTask = savedInput ?? shared?.pendingMessages.find((input) => (input.target.kind === 'task' || input.target.kind === 'channel_task') && input.target.task_id === selectedTaskId)
  const bot = shared?.bots.find((item) => task ? item.bot_id === task.owner_bot : pendingTask ? item.conversation_id === pendingTask.conversation_id : item.bot_id === (recipient || channel?.coordinator_bot))
  const draftKey = networkKey(`draft:${channelId ? `channel:${channelId}` : `task:${taskId}`}`, ready?.principal.network_id ?? '')
  const [text, setText] = useState(() => { const saved = storage.readStorage(draftKey, ''); return typeof saved === 'string' ? saved : '' })
  const draftRef = useRef<ComposerDraft | null>(null)
  const savingDrafts = useRef(new Set<ComposerDraft>())
  const changeText = (value: string) => { if (draftRef.current && draftRef.current.text !== value) draftRef.current = { ...draftRef.current, text: value }; setText(value); storage.writeStorage(draftKey, value) }
  const read = async () => {
    const live = latest.current
    if (!live) return
    const filter = channelId ? { channel_id: channelId } : { task_id: taskId }
    await live.readWorkspaceState(filter)
    const result = await live.readWorkspaceHistory(filter)
    if (live.isCurrent()) { setHistory(result); setError('') }
  }
  useEffect(() => { let active = true; const refresh = () => { if (active) void read().catch((cause) => { if (active) setError(messageOf(cause)) }) }; refresh(); const timer = setInterval(refresh, 3000); return () => { active = false; clearInterval(timer) } }, [taskId, channelId, shared?.generation])
  if (!shared || !bot || taskId && !task && !pendingTask || channelId && !channel) return <View className="flex-1"><EmptyState icon={ListChecks} title={error || (shared?.workspaceState ? '尚未读到这个任务、频道或 Bot' : '正在读取')} className="flex-1 justify-center" action={<Button variant="secondary" onPress={() => void read().catch((cause) => setError(messageOf(cause)))}><Text>重新读取</Text></Button>} /></View>
  const target: InputTarget = task ? task.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: task.task_id, bot_id: task.owner_bot } : { kind: 'task', task_id: task.task_id } : taskId && pendingTask ? pendingTask.target : selectedTaskId && channelId ? { kind: 'channel_task', channel_id: channelId, task_id: selectedTaskId, bot_id: bot.bot_id } : { kind: 'channel_member', channel_id: channelId ?? '', bot_id: bot.bot_id }
  const targetKey = inputTargetKey(target), conversationId = bot.conversation_id
  if (!draftRef.current || draftRef.current.text !== text || draftRef.current.files !== files || draftRef.current.target !== targetKey || draftRef.current.conversationId !== conversationId) draftRef.current = { text, files, target: targetKey, conversationId }
  const expectedRevision = task?.revision ?? pendingTask?.expected_revision
  const label = `${bot.name} · ${task?.title ?? (selectedTaskId ? '已保存的任务' : '新频道任务')}`
  const pending = shared.pendingMessages.filter((item) => channelId ? (item.target.kind === 'channel_task' || item.target.kind === 'channel_member') && item.target.channel_id === channelId : item.target.kind === 'task' && item.target.task_id === taskId || item.target.kind === 'channel_task' && item.target.task_id === taskId)
  const posted = new Set(history?.events.flatMap((event) => event.body.type === 'input_posted' ? [event.body.message_id] : []) ?? [])
  const active = task && ['running', 'update_pending'].includes(task.state)
  const control = (action: 'continue' | 'defer' | 'cancel') => run(action, async () => { if (task) await shared.workspaceCommand({ kind: 'control_task', task_id: task.task_id, expected_revision: task.revision, action }); await read() })
  return <View className="flex-1"><Stack.Screen options={{ title: channel?.title ?? task?.title ?? '正在创建任务', headerRight: () => channel ? <Link href={`/channel/${channel.channel_id}/settings`} asChild><Button variant="ghost" iconOnly accessibilityLabel="频道设置"><Icon as={Settings} /></Button></Link> : task ? <Link href={`/task/${task.task_id}/context`} asChild><Button variant="ghost" iconOnly accessibilityLabel="任务上下文和历史"><Icon as={BrainCircuit} /></Button></Link> : null }} />
    {!task && pendingTask && taskId ? <Text role="status" className="px-4 pt-3 text-sm text-muted">任务已在本地保存，正在读取任务记录。后续补充保留在这个任务。</Text> : null}
    <KeyboardAwareScrollView bottomOffset={16} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-6">
      {channel ? <View className="gap-2"><Text className="text-sm text-muted">{channel.purpose}</Text><Text className="text-sm text-muted">{channel.members.map((id) => shared.bots.find((item) => item.bot_id === id)?.name ?? id).join('、')} · 默认协调 {shared.bots.find((item) => item.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</Text><Details label={`频道任务 · ${tasks.length}`}><View className="gap-3">{tasks.map((item) => <WorkspaceTaskRow key={item.task_id} task={item} />)}</View></Details></View> : task ? <View className="gap-2"><Text className="text-sm text-muted">{bot.name} · 第 {task.revision} 版 · {taskLabels[task.state]}</Text>{task.waiting_reason ? <Text role="status" className="text-sm text-warning">{task.waiting_reason}</Text> : null}<View className="flex-row flex-wrap gap-2"><Button variant="secondary" disabled={Boolean(actionBusy)} onPress={() => void control('continue')}><Text>继续任务</Text></Button><Button variant="secondary" disabled={Boolean(actionBusy)} onPress={() => void control('defer')}><Text>稍后处理</Text></Button><Button variant="ghost" disabled={Boolean(actionBusy)} onPress={() => void control('cancel')}><Text>取消任务</Text></Button></View></View> : null}
      <View className="gap-2"><Text nativeID="scope-history-search" className="text-sm text-muted">{channelId ? "搜索当前频道已加载的记录" : "搜索当前任务已加载的记录"}</Text><Input accessibilityLabel={channelId ? "搜索当前频道已加载的记录" : "搜索当前任务已加载的记录"} accessibilityLabelledBy="scope-history-search" value={historyQuery} onChangeText={setHistoryQuery} placeholder="输入关键词" /></View>
      {history?.events.filter((item) => !historyQuery.trim() || JSON.stringify(item.body).toLocaleLowerCase().includes(historyQuery.trim().toLocaleLowerCase())).map((event) => {
        const body = event.body
        const actor = event.speaker
        const speaker = actor.kind === 'bot' ? shared.bots.find((bot) => bot.bot_id === actor.bot_id)?.name ?? 'Bot 未加载' : actor.kind === 'human' ? '你' : shared.nodes.find((node) => node.device_id === actor.device_id)?.name ?? '执行设备'
        if (body.type === 'input_posted' || body.type === 'message_posted') return <View key={event.event_hash} className={cn('gap-2', body.type === 'input_posted' ? 'max-w-[90%] self-end rounded-panel bg-surface-muted px-4 py-3' : 'w-full')}><Text className="text-sm text-muted">{speaker}</Text>{body.type === 'input_posted' ? <Text selectable>{body.text || `${body.attachments?.length ?? 0} 个附件`}</Text> : <Markdown text={body.text} />}{body.type === 'input_posted' && body.target.kind === 'channel_task' ? <Button variant="ghost" className="self-start" onPress={() => { if (body.target.kind === 'channel_task') { selectionRevision.current++; setSelected(`task:${body.target.task_id}`); setRecipient(body.target.bot_id); setSavedInput(null); storage.writeStorage(selectionKey, { bot_id: body.target.bot_id, task_id: body.target.task_id }); void shared.readWorkspaceState({ task_id: body.target.task_id }).catch((cause) => setError(messageOf(cause))) } }}><Icon as={Reply} /><Text>回复这个任务</Text></Button> : null}</View>
        if (body.type === 'work_finished') return <Card key={event.event_hash} className="gap-2"><Text className="text-sm font-medium">工作结果 · {body.outcome === 'completed' ? '已完成' : body.outcome === 'interrupted' ? '已停止' : '需要留意'}</Text><Markdown text={body.result_text || body.failure_reason || '没有结果文本'} /></Card>
        if (body.type === 'workspace_commanded') {
          const command = body.command
          if (command.kind === 'assignment_progress' && command.result) return <Card key={event.event_hash} className="gap-2"><Text className="text-sm text-muted">第 {command.revision} 版提交的结果，执行状态以任务记录为准</Text><Markdown text={command.result} /></Card>
          if (command.kind === 'post_channel_message') return <Card key={event.event_hash} className="gap-2"><Text className="text-sm text-muted">{speaker} · 频道记录</Text><Markdown text={command.text} />{command.attachments.length ? <Text className="text-sm">{command.attachments.length} 个附件</Text> : null}</Card>
          if (command.kind === 'create_task') { const item = tasks.find((task) => task.task_id === command.task_id); if (item) return <WorkspaceTaskRow key={event.event_hash} task={item} /> }
          if (command.kind === 'revise_task') return <Text key={event.event_hash} className="text-sm text-muted">任务交代已更新为第 {command.expected_revision + 1} 版。</Text>
        }
        return null
      })}
      {shared.questions.filter((question) => question.target && (channelId ? (question.target.kind === "channel_task" || question.target.kind === "channel_member") && question.target.channel_id === channelId : sameTarget(question.target, target))).map((question) => <QuestionCard key={question.question_id} question={question} inTask />)}
      {pending.map((item) => <View key={item.message_id} className="gap-2">{!posted.has(item.message_id) ? <View className="max-w-[90%] self-end rounded-panel bg-surface-muted px-4 py-3"><Text selectable>{item.text}</Text></View> : null}<InputStatus pending={item} /></View>)}
      {history && !history.events.length ? <Text className="text-center text-sm text-muted">发送一条交代开始对话。</Text> : null}<Text className="text-sm text-muted">{history ? `已加载最近 ${history.events.length} 条${history.coverage.truncated ? '，更早的记录未加载' : ''}。` : '尚未读取历史。'}执行服务是否接收、输入归入哪个回合，请查看每条输入的状态。</Text>
      {error || actionError || cleanupError ? <ErrorText error={error || actionError || cleanupError} /> : null}
    </KeyboardAwareScrollView>
    <KeyboardStickyView offset={{ closed: 0, opened: insets.bottom }}><View style={{ paddingBottom: insets.bottom }}><Composer value={text} onChange={changeText} files={files} onFiles={changeFiles} attachmentScope={JSON.stringify([ready?.principal.network_id ?? '', conversationId, targetKey])} blocked={shared.canSaveMessages ? null : shared.readOnlyNotice || shared.writeNotice || '尚未完成首读'} sending={false} label={`发给 ${label}`} sendLabel={active ? '补充当前工作' : '发送'} target={channelId && selectedTaskId ? <Button variant="ghost" iconOnly className="rounded-pill" accessibilityLabel="结束回复这个任务" onPress={() => { selectionRevision.current++; setSelected(''); setRecipient(''); setSavedInput(null); storage.writeStorage(selectionKey, null) }}><Icon as={X} className="text-muted" /></Button> : undefined} onStop={task && !['completed', 'cancelled', 'deferred'].includes(task.state) ? () => void shared.stopInputTarget({ target, expected_revision: task.revision }).catch((cause) => setError(messageOf(cause))) : undefined} onSend={() => {
      const submittedDraft = draftRef.current!
      if (savingDrafts.current.has(submittedDraft)) return
      savingDrafts.current.add(submittedDraft)
      const { text: submitted, files: selectedFiles } = submittedDraft
      const owner = selection.current!
      setError('')
      const save = () => shared.sendMessage(submitted, selectedFiles.map((file) => ({ name: file.name, arrayBuffer: file.arrayBuffer })), undefined, { target, intent: promptIntent(submitted), conversation_id: bot.conversation_id, ...(expectedRevision !== undefined ? { expected_revision: expectedRevision } : {}), onSaved: (input) => { if (savedSelection !== selectionRevision.current || !shared.isCurrent()) return; if (channelId && input.target.kind === 'channel_task' && input.target.channel_id === channelId) { setSavedInput(input); setSelected(`task:${input.target.task_id}`); setRecipient(input.target.bot_id); storage.writeStorage(selectionKey, { bot_id: input.target.bot_id, task_id: input.target.task_id }) } else if (draftRef.current === submittedDraft && routeActive.current && !channelId && (input.target.kind === 'task' || input.target.kind === 'channel_task') && input.target.task_id !== taskId) router.push(`/task/${input.target.task_id}`) } })
      void (selectedFiles.length ? owner.use(save) : save()).then((saved) => { if (saved && selection.current === owner && shared.isCurrent()) { if (draftRef.current === submittedDraft) { setText((current) => { if (current !== submitted) return current; storage.writeStorage(draftKey, ''); return '' }); if (filesRef.current === selectedFiles) changeFiles([]) } void read().catch((cause) => setError(messageOf(cause))) } }).catch((cause) => setError(messageOf(cause))).finally(() => { savingDrafts.current.delete(submittedDraft) })
    }}>{error || cleanupError ? <ErrorText error={error || cleanupError} /> : null}</Composer></View></KeyboardStickyView>
  </View>
}

export function TaskContextScreen({ taskId }: { taskId: string }) {
  const shared = useLive()
  const task = shared?.workspaceState?.tasks.find((item) => item.task_id === taskId)
  const [correction, setCorrection] = useState('')
  const [editing, setEditing] = useState(false)
  const [scopeContext, setScopeContext] = useState<ScopeContext | null>(null)
  const [contextError, setContextError] = useState('')
  const latest = useRef(shared)
  latest.current = shared
  const { error, busy, run } = useRun()
  useEffect(() => { let active = true; const workspace = latest.current; setScopeContext(null); setContextError(''); void workspace?.readWorkspaceState({ task_id: taskId }).then(() => workspace.readScopeContext({ target: { kind: 'task', task_id: taskId } })).then((value) => { if (active) setScopeContext(value) }, (cause) => { if (active) setContextError(messageOf(cause)) }); return () => { active = false } }, [taskId, task?.revision, shared?.generation])
  if (!task || !shared) return <EmptyState icon={BrainCircuit} title="尚未读取这个任务" className="flex-1 justify-center" />
  const assignment = shared.workspaceState?.assignments.find((item) => item.assignment_id === task.current_assignment)
  const target: InputTarget = task.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: task.task_id, bot_id: task.owner_bot } : { kind: 'task', task_id: task.task_id }
  return <KeyboardAwareScrollView keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12"><Stack.Screen options={{ title: '任务上下文和历史' }} /><Text role="heading" className="text-lg font-medium">{task.title}</Text><Text className="text-sm">独立任务对话 · 第 {task.revision} 版{assignment ? ` · 当前执行从第 ${assignment.started_revision} 版开始` : ''}</Text><Card className="gap-2"><Text className="font-medium">本轮来源</Text><Text selectable>交代内容 · {task.request}</Text><Text>期望结果 · {task.expected_result}</Text>{assignment?.work_id ? <Link href={`/b/${task.owner_bot}/work/${assignment.work_id}`} asChild><Button variant="secondary"><Text>查看执行工作</Text></Button></Link> : null}<Text>依赖任务 · {task.dependencies.length ? task.dependencies.map((id) => shared.workspaceState?.tasks.find((item) => item.task_id === id)?.title ?? id).join('、') : '无'}</Text><Details label="固定依据和引用来源"><Text selectable className="text-sm">任务来源事件 · {task.request_event}</Text>{task.source_events.map((source) => <Text selectable key={source} className="text-sm">引用来源 · {source}</Text>)}{assignment ? <Text selectable className="text-sm">本轮依据 · {JSON.stringify(assignment.basis)}</Text> : null}</Details></Card><Text className="text-sm text-muted">共享历史、Bot 的长期记忆与执行服务的上下文分别保存。只有本轮来源记录中的内容才显示为引用。</Text>{contextError ? <ErrorText error={contextError} /> : null}{scopeContext?.available ? <ScopeSources context={scopeContext.context} /> : <Text className="text-sm text-muted">{scopeContext ? scopeContext.reason || "这个任务尚无已编译的上下文。" : contextError ? "本次未读到任务上下文。" : "正在查询这个任务的上下文。"}</Text>}<Link href={`/b/${task.owner_bot}/context`} asChild><Button variant="secondary"><Icon as={BrainCircuit} /><Text>打开 Bot 上下文和持久记忆</Text></Button></Link>
    {shared.pendingMessages.filter((item) => item.target.kind === 'task' && item.target.task_id === taskId || item.target.kind === 'channel_task' && item.target.task_id === taskId).map((item) => <InputStatus key={item.message_id} pending={item} />)}
    <Button variant="secondary" disabled={Boolean(busy)} onPress={() => void run("read-context", async () => { setScopeContext(await shared.readScopeContext({ target })) })}><Text>重新读取上下文</Text></Button>
    <Button variant="secondary" onPress={() => { setCorrection(task.request); setEditing((value) => !value) }}><Text>纠正任务</Text></Button>{editing ? <View className="gap-3"><Text nativeID="revision-label" className="text-sm font-medium">更新任务交代</Text><Input accessibilityLabel="更新任务交代" accessibilityLabelledBy="revision-label" multiline className="min-h-36 py-3" textAlignVertical="top" value={correction} onChangeText={setCorrection} maxLength={100000} /><Text className="text-sm text-muted">保留任务身份并新增版本。旧版本结果不能完成这次更新。</Text><Button disabled={Boolean(busy) || !shared.writable} onPress={() => void run('revise-task', async () => { if (!correction.trim()) throw new Error('请填写更新后的交代。'); const result = await shared.workspaceCommand({ kind: 'revise_task', task_id: task.task_id, expected_revision: task.revision, request: correction.trim(), expected_result: task.expected_result, dependencies: task.dependencies }); const revised = result.tasks.find((item) => item.task_id === taskId); if (!revised) throw new Error('任务修订已提交，尚未确认新版本。'); await shared.sendMessage(correction.trim(), [], undefined, { target, expected_revision: revised.revision }); setEditing(false) })}><Text>保存修订并补充</Text></Button></View> : null}{error ? <ErrorText error={error} /> : null}
  </KeyboardAwareScrollView>
}
