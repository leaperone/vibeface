import { File } from 'expo-file-system'
import { Link, Stack, router, useFocusEffect } from 'expo-router'
import { BrainCircuit, ChevronRight, CircleQuestionMark, Clock, ListChecks, ListPlus, MessagesSquare, Plus, Reply, Settings, SlidersHorizontal, X } from 'lucide-react-native'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useReducedMotion } from 'react-native-reanimated'
import { Modal, Platform, Pressable, ScrollView, View } from 'react-native'
import { KeyboardAwareScrollView, KeyboardController, KeyboardStickyView } from 'react-native-keyboard-controller'
import { useSafeAreaInsets } from 'react-native-safe-area-context'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { inputTargetKey } from '@vibeface/client/chat/protocol.ts'
import { networkKey, newId, storageOf } from '@vibeface/client/workspace/storage.ts'
import type { ContextProjection, InputControlAction, InputIntent, InputReceipt, InputTarget, PendingMessage, QueuedInput, ScopeContext, WorkspaceCommand, WorkspaceHistory, WorkspaceTask } from '@vibeface/client/workspace/types.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { Details, EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { ErrorText } from '@/components/action'
import { useLive } from '@/data/live'
import type { Attachment } from '@/data/types'
import { Composer } from '@/features/chat/composer'
import { useChatChrome } from '@/features/chat/chrome'
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
type ComposerDraft = { text: string; files: Attachment[]; intent: InputIntent; target: string; conversationId: string }

export function ScopeSources({ context }: { context: ContextProjection }) {
  const kinds: Record<ContextProjection['sources'][number]['kind'], string> = { trigger: '本轮交代', message: '共享消息', work: '工作结果', dispatch: '工作指派', checkpoint: '历史摘要', handoff: '任务交接', memory: '持久记忆', question: '已知问题', attachment: '附件' }
  return <View className="gap-3 rounded-xl border border-border bg-card p-4">
    <Text role="heading" className="font-medium">本轮上下文引用</Text>
    <Text className="text-meta text-muted-foreground">{context.stage === 'compiled' ? '上下文已整理，尚未确认执行服务收到。' : '以下来源来自这个执行范围的上下文编译记录。'}</Text>
    <View className="flex-row flex-wrap gap-x-4 gap-y-1">{Object.entries(kinds).map(([kind, name]) => { const count = context.sources.filter((source) => source.kind === kind).length; return count ? <Text key={kind} className="text-meta">{name} {count} 项</Text> : null })}</View>
    <Text className="text-meta">因预算省略 · 消息 {context.coverage.messages_omitted} 条 · 附件 {context.coverage.attachments_omitted} 个 · 记忆 {context.coverage.memory_omitted} 条</Text>
    {context.sources_omitted ? <Text className="text-meta text-muted-foreground">另有 {context.sources_omitted} 个来源引用未展开。</Text> : null}
    <Details label="查看来源引用">{context.sources.map((source, index) => <Text selectable key={`${source.kind}:${index}`} className="text-meta">{kinds[source.kind]} · {source.reference ?? source.event_id ?? (source.author ? `${source.author}:${source.seq ?? ''}` : '未提供引用编号')}</Text>)}</Details>
    <Text role="heading" className="font-medium">补充归属</Text>
    {context.inputs.length ? context.inputs.map((input, index) => <View key={input.message_id} className="gap-1 rounded-lg bg-muted p-3"><Text className="text-meta">第 {index + 1} 条补充 · {receiptLabels[input.stage]}</Text><Details label="消息和回合编号"><Text selectable className="text-meta">消息 · {input.message_id}</Text><Text selectable className="text-meta">目标回合 · {input.expected_turn_id}</Text><Text selectable className="text-meta">实际回合 · {input.actual_turn_id ?? '尚未确认'}</Text></Details></View>) : <Text className="text-meta text-muted-foreground">这个范围的上下文记录没有补充回执。</Text>}
    {context.inputs_omitted ? <Text className="text-meta text-muted-foreground">另有 {context.inputs_omitted} 条补充回执未展开。</Text> : null}
  </View>
}

export function WorkspaceTaskRow({ task }: { task: WorkspaceTask }) {
  const shared = useLive()
  return <Link href={`/task/${task.task_id}`} asChild><Pressable accessibilityRole="link" accessibilityLabel={`${task.title}，${taskLabels[task.state]}`} className="min-h-16 gap-2 rounded-xl border border-border bg-card p-4 active:bg-muted">
    <View className="flex-row items-start gap-2"><Text className="min-w-0 flex-1 font-medium">{task.title}</Text><Text className={cn('text-meta', task.state === 'running' ? 'text-info' : ['needs_reconciliation', 'waiting_user'].includes(task.state) ? 'text-warning' : 'text-muted-foreground')}>{taskLabels[task.state]}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" /></View>
    <Text className="text-meta text-muted-foreground">{shared?.bots.find((bot) => bot.bot_id === task.owner_bot)?.name ?? 'Bot 未加载'} · 第 {task.revision} 版{task.channel_id ? ' · 频道任务' : ''}</Text>
    {task.waiting_reason ? <Text className="text-meta text-warning">{task.waiting_reason}</Text> : null}
  </Pressable></Link>
}

export function WorkspaceTasks({ botFilter = 'all', filter = 'all' }: { botFilter?: string; filter?: 'all' | 'attention' | 'working' | 'waiting' }) {
  const shared = useLive()
  const latest = useRef(shared)
  latest.current = shared
  useEffect(() => { void latest.current?.readWorkspaceState().catch(() => {}) }, [shared?.generation])
  const groups = { attention: ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'], working: ['running'], waiting: ['not_started', 'waiting_dependencies', 'deferred'] }
  const tasks = (shared?.workspaceState?.tasks ?? []).filter((task) => (botFilter === 'all' || task.owner_bot === botFilter) && (filter === 'all' || groups[filter].includes(task.state)))
  return <View className="gap-3"><View className="flex-row items-center justify-between gap-3"><Text role="heading" className="text-heading font-semibold">任务对话</Text><Link href="/new-task" asChild><Button variant="outline" disabled={!shared?.writable || !shared.bots.length}><Icon as={Plus} /><Text>新任务</Text></Button></Link></View><Text className="text-meta text-muted-foreground">每个任务保留独立对话，后续补充留在同一个任务。</Text>{tasks.map((task) => <WorkspaceTaskRow key={task.task_id} task={task} />)}{!tasks.length ? <Text className="py-3 text-meta text-muted-foreground">{shared?.workspaceState ? '还没有任务对话。选择 Bot 新建任务。' : '正在读取任务对话。'}</Text> : null}</View>
}

export function CreateTaskScreen() {
  const shared = useLive()
  const { busy, error, run } = useRun()
  const [botId, setBotId] = useState(shared?.selectedBot?.bot_id ?? shared?.bots[0]?.bot_id ?? '')
  const [title, setTitle] = useState('')
  const [request, setRequest] = useState('')
  const taskId = useRef(newId())
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_task' }> | null>(null)
  return <KeyboardAwareScrollView mode="layout" keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10"><Stack.Screen options={{ title: '新任务' }} />
    <Text className="text-meta text-muted-foreground">选择 Bot，任务后续的补充留在同一个对话里。</Text><Text className="text-meta font-medium">交给哪个 Bot</Text><View className="flex-row flex-wrap gap-2">{shared?.bots.map((bot) => <Button key={bot.bot_id} variant={botId === bot.bot_id ? 'default' : 'outline'} disabled={Boolean(submission.current)} onPress={() => setBotId(bot.bot_id)} accessibilityState={{ selected: botId === bot.bot_id }}><Text>{bot.name}</Text></Button>)}</View>
    <View className="gap-2"><Text nativeID="task-title-label" className="text-meta font-medium">任务名称</Text><Input editable={!submission.current} accessibilityLabel="任务名称" accessibilityLabelledBy="task-title-label" value={title} onChangeText={setTitle} maxLength={512} placeholder="例如：检查官网导航" /></View>
    <View className="gap-2"><Text nativeID="task-request-label" className="text-meta font-medium">交代内容</Text><Input editable={!submission.current} accessibilityLabel="交代内容" accessibilityLabelledBy="task-request-label" multiline className="min-h-36 py-3" textAlignVertical="top" value={request} onChangeText={setRequest} maxLength={100000} placeholder="描述要完成的事情和需要保留的约束" /></View>
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
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10"><Stack.Screen options={{ title: '频道' }} /><Link href="/new-channel" asChild><Button disabled={!shared?.writable || !shared.bots.length}><Icon as={Plus} /><Text>新频道</Text></Button></Link>{channels.map((channel) => <Link key={channel.channel_id} href={`/channel/${channel.channel_id}`} asChild><Pressable accessibilityRole="link" className="min-h-16 gap-2 rounded-xl border border-border bg-card p-4 active:bg-muted"><View className="flex-row items-center gap-2"><Icon as={MessagesSquare} className="text-muted-foreground" /><Text className="min-w-0 flex-1 font-medium">{channel.title}</Text><Icon as={ChevronRight} className="size-4 text-muted-foreground" /></View>{channel.purpose ? <Text className="text-meta text-muted-foreground">{channel.purpose}</Text> : null}<Text className="text-meta text-muted-foreground">{channel.members.length} 个 Bot · 默认协调 {shared?.bots.find((bot) => bot.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</Text></Pressable></Link>)}{!channels.length ? <EmptyState icon={MessagesSquare} title={shared?.workspaceState ? '还没有频道' : '正在读取频道'} action={<Button variant="outline" onPress={() => void run('read-channels', async () => { await shared?.readWorkspaceState() })}><Text>重新读取</Text></Button>} /> : null}{error ? <ErrorText error={error} /> : null}</ScrollView>
}

export function ChannelSettingsScreen({ channelId }: { channelId?: string }) {
  const shared = useLive()
  const { ready } = useDevice()
  const channel = shared?.workspaceState?.channels.find((item) => item.channel_id === channelId)
  const [title, setTitle] = useState(channel?.title ?? '')
  const [purpose, setPurpose] = useState(channel?.purpose ?? '')
  const [members, setMembers] = useState(channel?.members ?? shared?.bots.map((bot) => bot.bot_id) ?? [])
  const [coordinator, setCoordinator] = useState(channel?.coordinator_bot ?? shared?.bots[0]?.bot_id ?? '')
  const [node, setNode] = useState(channel?.coordinator_node ?? ready?.entry.device_id ?? '')
  const id = useRef(channelId ?? newId())
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> | null>(null)
  const { error, busy, run } = useRun()
  return <KeyboardAwareScrollView mode="layout" keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10"><Stack.Screen options={{ title: channelId ? '频道设置' : '新频道' }} />
    <View className="gap-2"><Text nativeID="channel-title-label" className="text-meta font-medium">频道名称</Text><Input editable={!submission.current} accessibilityLabel="频道名称" accessibilityLabelledBy="channel-title-label" value={title} onChangeText={setTitle} maxLength={512} placeholder="例如：官网改版" /></View><View className="gap-2"><Text nativeID="channel-purpose-label" className="text-meta font-medium">频道目标</Text><Input editable={!submission.current} accessibilityLabel="频道目标" accessibilityLabelledBy="channel-purpose-label" value={purpose} onChangeText={setPurpose} maxLength={8192} multiline placeholder="说明这个频道要完成什么" /></View>
    <Text role="heading" className="text-meta font-medium">Bot 成员</Text><View className="gap-2">{shared?.bots.map((bot) => <Pressable key={bot.bot_id} accessibilityRole="checkbox" accessibilityState={{ checked: members.includes(bot.bot_id) }} accessibilityLabel={bot.name} disabled={Boolean(submission.current)} className={cn('min-h-11 flex-row items-center gap-3 rounded-xl border px-3 py-2', members.includes(bot.bot_id) ? 'border-brand bg-brand-soft' : 'border-border bg-card')} onPress={() => setMembers((current) => current.includes(bot.bot_id) ? current.filter((value) => value !== bot.bot_id) : [...current, bot.bot_id])}><Text className="flex-1">{bot.name}</Text><Text>{members.includes(bot.bot_id) ? '已加入' : '加入'}</Text></Pressable>)}</View>
    <Text role="heading" className="text-meta font-medium">默认协调 Bot</Text><View className="flex-row flex-wrap gap-2">{shared?.bots.filter((bot) => members.includes(bot.bot_id)).map((bot) => <Button key={bot.bot_id} variant={coordinator === bot.bot_id ? 'default' : 'outline'} disabled={Boolean(submission.current)} onPress={() => setCoordinator(bot.bot_id)} accessibilityState={{ selected: coordinator === bot.bot_id }}><Text>{bot.name}</Text></Button>)}</View>
    <Text role="heading" className="text-meta font-medium">协调设备</Text><View className="gap-2">{shared?.nodes.filter((item) => !item.revoked).map((item) => <Button key={item.device_id} variant={node === item.device_id ? 'default' : 'outline'} disabled={Boolean(submission.current)} onPress={() => setNode(item.device_id)} accessibilityState={{ selected: node === item.device_id }}><Text>{item.name}{item.online ? '' : ' · 离线'}</Text></Button>)}</View>
    <Text className="text-meta text-muted-foreground">普通消息只交给选中的 Bot。没有指定目标时，由默认协调 Bot 承接。</Text>{error ? <ErrorText error={error} /> : null}<Button disabled={Boolean(busy) || !shared?.writable} onPress={() => void run('save-channel', async () => {
      if (!shared || !title.trim() || !members.length || !members.includes(coordinator)) throw new Error('请填写频道名称，选择成员并将默认协调 Bot 加入成员。')
      if (channelId && !channel) throw new Error('尚未读取频道设置，请返回频道重新打开。')
      const configuration = { channel_id: id.current, title: title.trim(), purpose: purpose.trim(), members, coordinator_bot: coordinator, coordinator_node: node }
      const command: Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> = submission.current ?? (channel ? { kind: 'configure_channel', ...configuration, expected_revision: channel.revision, owner_epoch: channel.owner_epoch } : { kind: 'create_channel', ...configuration })
      submission.current = command
      await shared.workspaceCommand(command)
      router.replace(`/channel/${id.current}`)
    })}><Text>{busy ? '正在保存' : channelId ? '保存频道' : '创建频道'}</Text></Button>
  </KeyboardAwareScrollView>
}

export type TargetOption = { key: string; label: string; target: InputTarget; revision?: number }

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

function InputQueue({ targets, editing, setEditing }: { targets: InputTarget[]; editing: string | null; setEditing: (id: string | null) => void }) {
  const shared = useLive()
  const { busy, error, run } = useRun()
  const [text, setText] = useState('')
  const [attachments, setAttachments] = useState<NonNullable<PendingMessage['attachments']>>([])
  const queued = (shared?.workspaceState?.inputs ?? []).filter((item) => item.deferred && !item.cancelled && targets.some((target) => sameTarget(item.input.target, target))).sort((left, right) => left.effective_order - right.effective_order)
  const control = (item: QueuedInput, action: InputControlAction) => run(item.input.message_id, async () => {
    if (!shared) throw new Error('尚未连接工作区。')
    const receipt = await shared.queryInput({ message_id: item.input.message_id, target: item.input.target, conversation_id: item.conversation_id })
    if (!('stage' in receipt) || !['entry_accepted', 'owner_accepted', 'refused'].includes(receipt.stage)) throw new Error('这条输入可能已提交，只能查询当前结果。')
    await shared.controlInput({ message_id: item.input.message_id, expected_event: item.control_event, action })
    await shared.readWorkspaceState()
    if (action.kind === 'edit') { await KeyboardController.dismiss(); setEditing(null) }
  })
  if (!queued.length) return null
  return <View className="gap-3"><Text role="heading" className="font-medium">稍后处理 {queued.length} 条</Text>{queued.map((item) => {
    const lane = queued.filter((other) => sameTarget(other.input.target, item.input.target))
    const index = lane.findIndex((other) => other.input.message_id === item.input.message_id)
    const pending = shared?.pendingMessages.find((value) => value.message_id === item.input.message_id)
    const locked = Boolean(busy) || !shared?.writable || pending?.receipt && ['submitting', 'provider_accepted', 'associated', 'unknown'].includes(pending.receipt.stage)
    return <View key={item.input.message_id} className="gap-2 rounded-xl border border-border bg-card p-3">{editing === item.input.message_id ? <View className="gap-2"><Text nativeID="queue-edit-label" className="text-meta font-medium">编辑稍后输入</Text><Input accessibilityLabel="编辑稍后输入" accessibilityLabelledBy="queue-edit-label" multiline value={text} onChangeText={setText} maxLength={100000} className="min-h-24 py-3" />{(item.input.attachments ?? []).map((attachment) => <Pressable key={attachment.hash} accessibilityRole="checkbox" accessibilityState={{ checked: attachments.some((value) => value.hash === attachment.hash) }} accessibilityLabel={`保留附件 ${attachment.name}`} className="min-h-11 justify-center" onPress={() => setAttachments((current) => current.some((value) => value.hash === attachment.hash) ? current.filter((value) => value.hash !== attachment.hash) : [...current, attachment])}><Text className="text-meta">{attachments.some((value) => value.hash === attachment.hash) ? '保留' : '移除'}附件 · {attachment.name}</Text></Pressable>)}<View className="flex-row flex-wrap gap-2"><Button disabled={Boolean(locked)} onPress={() => void control(item, { kind: 'edit', text, attachments })}><Text>保存输入修改</Text></Button><Button variant="ghost" onPress={() => setEditing(null)}><Text>放弃编辑</Text></Button></View></View> : <><Text selectable>{item.input.text || `${item.input.attachments?.length ?? 0} 个附件`}</Text><View className="flex-row flex-wrap gap-2"><Button variant="outline" disabled={Boolean(locked)} onPress={() => void control(item, { kind: 'release' })}><Text>现在处理</Text></Button><Button variant="ghost" disabled={Boolean(locked)} onPress={() => { setEditing(item.input.message_id); setText(item.input.text); setAttachments(item.input.attachments ?? []) }}><Text>编辑输入</Text></Button><Button variant="ghost" disabled={Boolean(locked)} onPress={() => void control(item, { kind: 'cancel' })}><Text>取消稍后输入</Text></Button><Button variant="ghost" disabled={Boolean(locked) || index === 0} onPress={() => void control(item, { kind: 'move', before_message_id: lane[index - 1]?.input.message_id })}><Text>上移</Text></Button><Button variant="ghost" disabled={Boolean(locked) || index === lane.length - 1} onPress={() => void control(item, { kind: 'move', ...(lane[index + 2] ? { before_message_id: lane[index + 2].input.message_id } : {}) })}><Text>下移</Text></Button></View></>}{locked ? <Text className="text-meta text-muted-foreground">输入状态变化或正在提交时，先查询结果再管理。</Text> : null}</View>
  })}{error ? <ErrorText error={error} /> : null}<Text className="text-meta text-muted-foreground">操作由执行设备确认。已开始处理或结果未知的输入，只能查询状态。</Text></View>
}

export function InputOptions({ label, options, selected, onSelect, intent, onIntent, pending }: { label: string; options: TargetOption[]; selected: string; onSelect: (option: TargetOption) => void; intent: InputIntent; onIntent: (intent: InputIntent) => void; pending: PendingMessage[] }) {
  const [open, setOpen] = useState(false)
  // Lifted from the queue so a swipe cannot close the sheet over an unsaved edit.
  const [editing, setEditing] = useState<string | null>(null)
  const shared = useLive()
  const [scopeContext, setScopeContext] = useState<ScopeContext | null>(null)
  const [contextError, setContextError] = useState('')
  const [contextBusy, setContextBusy] = useState(false)
  const contextRead = useRef(0)
  useEffect(() => { contextRead.current += 1; setScopeContext(null); setContextError(''); setContextBusy(false) }, [selected])
  const insets = useSafeAreaInsets()
  const reducedMotion = useReducedMotion()
  const intents: { value: InputIntent; label: string }[] = [{ value: 'continue', label: '补充当前工作' }, { value: 'new_task', label: '新任务' }, { value: 'defer', label: '稍后处理' }]
  const shown = intent === 'new_task' ? ListPlus : intent === 'defer' ? Clock : SlidersHorizontal
  return <><Button variant="ghost" size="icon" className={cn('rounded-full', intent !== 'continue' && 'bg-muted')} accessibilityLabel={`输入目标 ${label}，${intents.find((item) => item.value === intent)?.label}${pending.length ? `，${pending.length} 条待确认` : ''}，打开目标和队列`} onPress={() => setOpen(true)}>
    <Icon as={shown} className={intent === 'continue' ? 'text-muted-foreground' : 'text-foreground'} />
    {pending.length ? <View className="absolute end-1 top-1 size-2 rounded-full bg-warning" /> : null}
  </Button>
    <Modal visible={open} presentationStyle="pageSheet" allowSwipeDismissal={!editing} animationType={reducedMotion ? 'none' : 'slide'} onRequestClose={() => { if (!editing) setOpen(false) }}>{/* Android ignores pageSheet and draws this full screen under the status bar. */}<View testID="input-options-sheet" accessibilityViewIsModal className="flex-1 bg-background px-4" style={{ paddingTop: (Platform.OS === 'android' ? insets.top : 0) + 16, paddingBottom: insets.bottom + 16 }}><View className="mb-3 flex-row items-center"><Text role="heading" className="flex-1 text-heading font-semibold">目标和队列</Text><Button variant="ghost" size="icon" accessibilityLabel="关闭目标和队列" onPress={() => setOpen(false)}><Icon as={X} /></Button></View><ScrollView automaticallyAdjustKeyboardInsets keyboardDismissMode="interactive" keyboardShouldPersistTaps="handled" contentContainerClassName="gap-4 pb-4"><View className="gap-2"><Text className="text-meta font-medium">发送给</Text>{options.map((option) => <Button key={option.key} variant={selected === option.key ? 'default' : 'outline'} accessibilityState={{ selected: selected === option.key }} onPress={() => onSelect(option)}><Text className="shrink">{option.label}</Text></Button>)}</View><View className="gap-2"><Text className="text-meta font-medium">发送方式</Text>{intents.map((item) => <Button key={item.value} variant={intent === item.value ? 'default' : 'outline'} accessibilityState={{ selected: intent === item.value }} onPress={() => { onIntent(item.value); setOpen(false) }}><Text>{item.label}</Text></Button>)}</View><Button variant="outline" disabled={!shared || contextBusy} onPress={() => {
      const option = options.find((item) => item.key === selected)
      if (!option || !shared) return
      const ticket = ++contextRead.current
      setContextBusy(true); setScopeContext(null); setContextError('')
      void shared.readScopeContext({ target: option.target }).then((value) => { if (ticket === contextRead.current) setScopeContext(value) }, (cause) => { if (ticket === contextRead.current) setContextError(messageOf(cause)) }).finally(() => { if (ticket === contextRead.current) setContextBusy(false) })
    }}><Icon as={BrainCircuit} /><Text>{contextBusy ? '正在查询目标上下文' : '查看目标上下文'}</Text></Button>{contextError ? <ErrorText error={contextError} /> : null}{scopeContext?.available ? <ScopeSources context={scopeContext.context} /> : scopeContext ? <Text className="text-meta text-muted-foreground">{scopeContext.reason || '这个目标尚无已编译的上下文。'}</Text> : null}<InputQueue targets={options.map((option) => option.target)} editing={editing} setEditing={setEditing} /><View className="gap-2"><Text role="heading" className="text-meta font-medium">已保存输入</Text>{pending.length ? pending.map((item) => <View key={item.message_id} className="gap-2"><Text numberOfLines={3} className="text-meta">{item.text}</Text><InputStatus pending={item} /></View>) : <Text className="text-meta text-muted-foreground">没有待确认的本地输入。</Text>}</View></ScrollView></View></Modal>
  </>
}

export function InputStatus({ pending }: { pending: PendingMessage }) {
  const shared = useLive()
  const { error, busy, run } = useRun()
  const receipt = pending.receipt
  const queued = shared?.workspaceState?.inputs.find((item) => item.conversation_id === pending.conversation_id && item.input.message_id === pending.message_id)
  const releasePending = pending.release_expected_event !== undefined || pending.release_command_id !== undefined
  const canClose = pending.delivery === 'accepted' && receipt?.stage === 'refused' && pending.rejection_code !== 'input_conflict' && !releasePending
    && receipt.message_id === pending.message_id && inputTargetKey(receipt.target) === inputTargetKey(pending.target)
  const canDiscard = pending.delivery === 'saved' && receipt === undefined && pending.rejection_code !== 'input_conflict' && !releasePending
  const canRetry = shared?.writable && !releasePending && !queued?.cancelled && !queued?.deferred && (pending.delivery === 'saved' && pending.rejection_code === 'input_not_saved' || pending.delivery === 'accepted' && receipt?.stage === 'refused' && receipt.retry_safe === true && Boolean(receipt.control_event?.trim()) && pending.rejection_code !== 'input_conflict'
    && receipt.message_id === pending.message_id && inputTargetKey(receipt.target) === inputTargetKey(pending.target))
  const confirmEntry = pending.entry_device_id === undefined && (pending.delivery === 'unknown' || pending.delivery === 'submitting') && !releasePending && shared?.connectionOnline ? shared.info : null
  return <View role="status" className="gap-2 rounded-lg bg-muted/60 px-3 py-2"><Text className={cn('text-meta', pending.rejected || pending.delivery === 'unknown' || receipt?.stage === 'unknown' ? 'text-warning' : 'text-muted-foreground')}>{pending.rejected ? `已拒绝 · ${pending.rejected}` : pending.delivery === 'unknown' && receipt?.stage === 'refused' && receipt.retry_safe ? '重试请求待确认' : receipt ? receiptLabels[receipt.stage] : deliveryLabels[pending.delivery]}</Text>{receipt?.actual_turn_id ? <Text selectable className="text-meta text-muted-foreground">实际回合 · {receipt.actual_turn_id}</Text> : null}{receipt?.reason ? <Text className="text-meta text-muted-foreground">{receipt.reason}</Text> : null}{confirmEntry ? <Text className="text-meta text-muted-foreground">这条输入没有记录原设备。只有最初发给「{confirmEntry.name || '当前设备'} · {confirmEntry.device_id.slice(0, 8)}」时，才确认原设备并查询。查询不会重新发送。</Text> : null}{confirmEntry ? <Button variant="outline" className="self-start" disabled={Boolean(busy)} onPress={() => void run('confirm-input-entry', async () => { await shared?.confirmInputEntry(pending.message_id, confirmEntry.device_id) })}><Text>确认原设备并查询</Text></Button> : null}<View className="flex-row flex-wrap gap-2"><Button variant="ghost" disabled={Boolean(busy)} onPress={() => void run('query-input', async () => { await shared?.queryInput({ message_id: pending.message_id, target: pending.target, conversation_id: pending.conversation_id }) })}><Icon as={CircleQuestionMark} /><Text>查询结果</Text></Button>{canRetry ? <Button variant="outline" disabled={Boolean(busy)} onPress={() => { if (canRetry && shared) void run('retry-input', () => shared.retryPendingMessage(pending.message_id)) }}><Text>重试原输入</Text></Button> : null}{canClose || canDiscard ? <Button variant="ghost" disabled={Boolean(busy)} onPress={() => void run('release-input', async () => { await shared?.releaseMessagePending(pending.message_id) })}><Text>{canClose ? '关闭提醒' : '不再发送'}</Text></Button> : null}</View>{error ? <ErrorText error={error} /> : null}</View>
}

export function ScopedWorkspaceScreen({ taskId, channelId }: { taskId?: string; channelId?: string }) {
  const shared = useLive()
  const chrome = useChatChrome()
  const { ready } = useDevice()
  const insets = useSafeAreaInsets()
  const [history, setHistory] = useState<WorkspaceHistory | null>(null)
  const [historyQuery, setHistoryQuery] = useState('')
  const storage = storageOf(mobilePlatform.storage)
  const selectionKey = networkKey(`channel-target:${channelId}`, ready?.principal.network_id ?? '')
  const cached = channelId ? storage.readStorage<unknown>(selectionKey, null) : null
  const cachedBot = cached && typeof cached === 'object' && 'bot_id' in cached && typeof cached.bot_id === 'string' ? cached.bot_id : ''
  const cachedTask = cached && typeof cached === 'object' && 'task_id' in cached && typeof cached.task_id === 'string' ? cached.task_id : ''
  const [selected, setSelected] = useState(cachedTask ? `task:${cachedTask}` : cachedBot ? `member:${cachedBot}` : '')
  const [recipient, setRecipient] = useState(cachedBot)
  const [savedInput, setSavedInput] = useState<PendingMessage | null>(null)
  const selectionRevision = useRef(0)
  const savedSelection = selectionRevision.current
  const [intent, setIntent] = useState<InputIntent>('continue')
  const [files, setFiles] = useState<Attachment[]>([])
  const [error, setError] = useState('')
  const { error: actionError, busy: actionBusy, run } = useRun()
  const routeActive = useRef(false)
  useFocusEffect(useCallback(() => { routeActive.current = true; return () => { routeActive.current = false } }, [taskId, channelId, shared?.generation]))
  const latest = useRef(shared)
  latest.current = shared
  const channel = shared?.workspaceState?.channels.find((item) => item.channel_id === channelId)
  const tasks = (shared?.workspaceState?.tasks ?? []).filter((item) => channelId ? item.channel_id === channelId : item.task_id === taskId)
  const selectedTaskId = taskId ?? (selected.startsWith('task:') ? selected.slice(5) : undefined)
  const task = selectedTaskId ? tasks.find((item) => item.task_id === selectedTaskId) : tasks.findLast((item) => item.owner_bot === (selected.startsWith('member:') ? selected.slice(7) : recipient || channel?.coordinator_bot) && !['completed', 'cancelled'].includes(item.state))
  const pendingTask = savedInput ?? shared?.pendingMessages.find((input) => (input.target.kind === 'task' || input.target.kind === 'channel_task') && input.target.task_id === selectedTaskId)
  const bot = shared?.bots.find((item) => task ? item.bot_id === task.owner_bot : pendingTask ? item.conversation_id === pendingTask.conversation_id : item.bot_id === (recipient || (selected.startsWith('member:') ? selected.slice(7) : channel?.coordinator_bot)))
  const draftKey = networkKey(`draft:${channelId ? `channel:${channelId}` : `task:${taskId}`}`, ready?.principal.network_id ?? '')
  const [text, setText] = useState(() => { const saved = storage.readStorage<unknown>(draftKey, ''); return typeof saved === 'string' ? saved : '' })
  const draftRef = useRef<ComposerDraft | null>(null)
  const savingDrafts = useRef(new Set<ComposerDraft>())
  const changeText = (value: string) => { setText(value); storage.writeStorage(draftKey, value) }
  const read = async () => {
    const live = latest.current
    if (!live) return
    const filter = channelId ? { channel_id: channelId } : { task_id: taskId }
    await live.readWorkspaceState(filter)
    const result = await live.readWorkspaceHistory(filter)
    if (live.isCurrent()) { setHistory(result); setError('') }
  }
  useEffect(() => { let active = true; const refresh = () => { if (active) void read().catch((cause) => { if (active) setError(messageOf(cause)) }) }; refresh(); const timer = setInterval(refresh, 3000); return () => { active = false; clearInterval(timer) } }, [taskId, channelId, shared?.generation])
  if (!shared || !bot || taskId && !task && !pendingTask || channelId && !channel) return <View className="flex-1"><EmptyState icon={ListChecks} title={error || (shared?.workspaceState ? '尚未读到这个任务、频道或 Bot' : '正在读取')} className="flex-1 justify-center" action={<Button variant="outline" onPress={() => void read().catch((cause) => setError(messageOf(cause)))}><Text>重新读取</Text></Button>} /></View>
  const target: InputTarget = task ? task.channel_id ? { kind: 'channel_task', channel_id: task.channel_id, task_id: task.task_id, bot_id: task.owner_bot } : { kind: 'task', task_id: task.task_id } : taskId && pendingTask ? pendingTask.target : selectedTaskId && channelId ? { kind: 'channel_task', channel_id: channelId, task_id: selectedTaskId, bot_id: bot.bot_id } : { kind: 'channel_member', channel_id: channelId ?? '', bot_id: bot.bot_id }
  const targetKey = inputTargetKey(target), conversationId = bot.conversation_id
  if (!draftRef.current || draftRef.current.text !== text || draftRef.current.files !== files || draftRef.current.intent !== intent || draftRef.current.target !== targetKey || draftRef.current.conversationId !== conversationId) draftRef.current = { text, files, intent, target: targetKey, conversationId }
  const draft = draftRef.current
  const expectedRevision = task?.revision ?? pendingTask?.expected_revision
  const label = `${bot.name} · ${task?.title ?? (selectedTaskId ? '已保存的任务' : '新频道任务')}`
  const pending = shared.pendingMessages.filter((item) => channelId ? (item.target.kind === 'channel_task' || item.target.kind === 'channel_member') && item.target.channel_id === channelId : item.target.kind === 'task' && item.target.task_id === taskId || item.target.kind === 'channel_task' && item.target.task_id === taskId)
  const options: TargetOption[] = channel ? [...channel.members.map((id) => ({ key: `member:${id}`, label: shared.bots.find((item) => item.bot_id === id)?.name ?? id, target: { kind: 'channel_member' as const, channel_id: channel.channel_id, bot_id: id } })), ...tasks.map((item) => ({ key: `task:${item.task_id}`, label: `${shared.bots.find((value) => value.bot_id === item.owner_bot)?.name ?? 'Bot'} · ${item.title}`, target: { kind: 'channel_task' as const, channel_id: channel.channel_id, task_id: item.task_id, bot_id: item.owner_bot }, revision: item.revision })), ...(!task && selectedTaskId ? [{ key: `task:${selectedTaskId}`, label, target, revision: expectedRevision }] : [])] : [{ key: `task:${taskId ?? ''}`, label, target, revision: expectedRevision }]
  const posted = new Set(history?.events.flatMap((event) => event.body.type === 'input_posted' ? [event.body.message_id] : []) ?? [])
  const active = task && ['running', 'update_pending'].includes(task.state)
  const control = (action: 'continue' | 'defer' | 'cancel') => run(action, async () => { if (task) await shared.workspaceCommand({ kind: 'control_task', task_id: task.task_id, expected_revision: task.revision, action }); await read() })
  return <View className="flex-1"><Stack.Screen options={{ title: channel?.title ?? task?.title ?? '正在创建任务', headerRight: () => channel ? <Link href={`/channel/${channel.channel_id}/settings`} asChild><Button variant="ghost" size="icon" accessibilityLabel="频道设置"><Icon as={Settings} /></Button></Link> : task ? <Link href={`/task/${task.task_id}/context`} asChild><Button variant="ghost" size="icon" accessibilityLabel="任务上下文和历史"><Icon as={BrainCircuit} /></Button></Link> : null }} />
    {!task && pendingTask && taskId ? <Text role="status" className="px-4 pt-3 text-meta text-muted-foreground">任务已在本地保存，正在读取任务记录。后续补充保留在这个任务。</Text> : null}
    <KeyboardAwareScrollView bottomOffset={16} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-6">
      {channel ? <View className="gap-2"><Text className="text-meta text-muted-foreground">{channel.purpose}</Text><Text className="text-meta text-muted-foreground">{channel.members.map((id) => shared.bots.find((item) => item.bot_id === id)?.name ?? id).join('、')} · 默认协调 {shared.bots.find((item) => item.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</Text><Details label={`频道任务 · ${tasks.length}`}><View className="gap-3">{tasks.map((item) => <WorkspaceTaskRow key={item.task_id} task={item} />)}</View></Details></View> : task ? <View className="gap-2"><Text className="text-meta text-muted-foreground">{bot.name} · 第 {task.revision} 版 · {taskLabels[task.state]}</Text>{task.waiting_reason ? <Text role="status" className="text-meta text-warning">{task.waiting_reason}</Text> : null}<View className="flex-row flex-wrap gap-2"><Button variant="outline" disabled={Boolean(actionBusy)} onPress={() => void control('continue')}><Text>继续任务</Text></Button><Button variant="outline" disabled={Boolean(actionBusy)} onPress={() => void control('defer')}><Text>稍后处理</Text></Button><Button variant="ghost" disabled={Boolean(actionBusy)} onPress={() => void control('cancel')}><Text>取消任务</Text></Button></View></View> : null}
      <View className="gap-2"><Text nativeID="scope-history-search" className="text-meta text-muted-foreground">{channelId ? "搜索当前频道已加载的记录" : "搜索当前任务已加载的记录"}</Text><Input accessibilityLabel={channelId ? "搜索当前频道已加载的记录" : "搜索当前任务已加载的记录"} accessibilityLabelledBy="scope-history-search" value={historyQuery} onChangeText={setHistoryQuery} placeholder="输入关键词" /></View>
      {history?.events.filter((item) => !historyQuery.trim() || JSON.stringify(item.body).toLocaleLowerCase().includes(historyQuery.trim().toLocaleLowerCase())).map((event) => {
        const body = event.body
        const actor = event.speaker
        const speaker = actor.kind === 'bot' ? shared.bots.find((bot) => bot.bot_id === actor.bot_id)?.name ?? 'Bot 未加载' : actor.kind === 'human' ? '你' : shared.nodes.find((node) => node.device_id === actor.device_id)?.name ?? '执行设备'
        if (body.type === 'input_posted' || body.type === 'message_posted') return <View key={event.event_hash} className={cn('gap-2', body.type === 'input_posted' ? 'max-w-[90%] self-end rounded-2xl bg-secondary px-4 py-3' : 'w-full')}><Text className="text-meta text-muted-foreground">{speaker}</Text>{body.type === 'input_posted' ? <Text selectable>{body.text || `${body.attachments?.length ?? 0} 个附件`}</Text> : <Markdown text={body.text} />}{body.type === 'input_posted' && body.target.kind === 'channel_task' ? <Button variant="ghost" className="self-start" onPress={() => { if (body.target.kind === 'channel_task') { selectionRevision.current++; setSelected(`task:${body.target.task_id}`); setRecipient(body.target.bot_id); setSavedInput(null); storage.writeStorage(selectionKey, { bot_id: body.target.bot_id, task_id: body.target.task_id }); void shared.readWorkspaceState({ task_id: body.target.task_id }).catch((cause) => setError(messageOf(cause))) } }}><Icon as={Reply} /><Text>回复这个任务</Text></Button> : null}</View>
        if (body.type === 'work_finished') return <View key={event.event_hash} className="gap-2 rounded-xl border border-border bg-card p-4"><Text className="text-meta font-medium">工作结果 · {body.outcome === 'completed' ? '已完成' : body.outcome === 'interrupted' ? '已停止' : '需要留意'}</Text><Markdown text={body.result_text || body.failure_reason || '没有结果文本'} /></View>
        if (body.type === 'workspace_commanded') {
          const command = body.command
          if (command.kind === 'assignment_progress' && command.result) return <View key={event.event_hash} className="gap-2 rounded-xl border border-border bg-card p-4"><Text className="text-meta text-muted-foreground">第 {command.revision} 版提交的结果，执行状态以任务记录为准</Text><Markdown text={command.result} /></View>
          if (command.kind === 'post_channel_message') return <View key={event.event_hash} className="gap-2 rounded-xl border border-border bg-card p-4"><Text className="text-meta text-muted-foreground">{speaker} · 频道记录</Text><Markdown text={command.text} />{command.attachments.length ? <Text className="text-meta">{command.attachments.length} 个附件</Text> : null}</View>
          if (command.kind === 'create_task') { const item = tasks.find((task) => task.task_id === command.task_id); if (item) return <WorkspaceTaskRow key={event.event_hash} task={item} /> }
          if (command.kind === 'revise_task') return <Text key={event.event_hash} className="text-meta text-muted-foreground">任务交代已更新为第 {command.expected_revision + 1} 版。</Text>
        }
        return null
      })}
      {shared.questions.filter((question) => question.target && (channelId ? (question.target.kind === "channel_task" || question.target.kind === "channel_member") && question.target.channel_id === channelId : sameTarget(question.target, target))).map((question) => <QuestionCard key={question.question_id} question={question} inTask />)}
      {pending.map((item) => <View key={item.message_id} className="gap-2">{!posted.has(item.message_id) ? <View className="max-w-[90%] self-end rounded-2xl bg-secondary px-4 py-3"><Text selectable>{item.text}</Text></View> : null}<InputStatus pending={item} /></View>)}
      {history && !history.events.length ? <Text className="text-center text-meta text-muted-foreground">发送一条交代开始对话。</Text> : null}<Text className="text-meta text-muted-foreground">{history ? `已加载最近 ${history.events.length} 条${history.coverage.truncated ? '，更早的记录未加载' : ''}。` : '尚未读取历史。'}执行服务是否接收、输入归入哪个回合，请查看每条输入的状态。</Text>
      {error || actionError ? <ErrorText error={error || actionError} /> : null}
    </KeyboardAwareScrollView>
    <KeyboardStickyView offset={{ closed: 0, opened: insets.bottom }}><View style={{ paddingBottom: insets.bottom }}><Composer chrome={chrome} value={text} onChange={changeText} files={files} onFiles={setFiles} attachmentScope={JSON.stringify([ready?.principal.network_id ?? '', conversationId, targetKey, intent])} blocked={shared.canSaveMessages ? null : shared.readOnlyNotice || shared.writeNotice || '尚未完成首读'} sending={false} label={`发给 ${label}`} sendLabel={intent === 'defer' ? '保存为稍后处理' : intent === 'new_task' ? '发送为新任务' : active ? '补充当前工作' : '发送'} target={<InputOptions label={label} options={options} selected={selected || (task ? `task:${task.task_id}` : taskId ? `task:${taskId}` : `member:${bot.bot_id}`)} onSelect={(option) => { selectionRevision.current++; setSelected(option.key); setSavedInput(null); if (option.target.kind === 'channel_task' || option.target.kind === 'channel_member') { setRecipient(option.target.bot_id); storage.writeStorage(selectionKey, { bot_id: option.target.bot_id, ...(option.target.kind === 'channel_task' ? { task_id: option.target.task_id } : {}) }); if (option.target.kind === 'channel_task') void shared.readWorkspaceState({ task_id: option.target.task_id }).catch((cause) => setError(messageOf(cause))) } }} intent={intent} onIntent={setIntent} pending={pending} />} onStop={task && !['completed', 'cancelled', 'deferred'].includes(task.state) ? () => void shared.stopInputTarget({ target, expected_revision: task.revision }).catch((cause) => setError(messageOf(cause))) : undefined} onSend={() => {
      const submittedDraft = draft
      if (savingDrafts.current.has(submittedDraft)) return
      savingDrafts.current.add(submittedDraft)
      const submitted = text, selectedFiles = files
      setError('')
      void shared.sendMessage(submitted, selectedFiles.map((file) => ({ name: file.name, arrayBuffer: () => new File(file.uri).arrayBuffer() })), undefined, { target, intent, conversation_id: bot.conversation_id, ...(expectedRevision !== undefined ? { expected_revision: expectedRevision } : {}), onSaved: (input) => { if (savedSelection !== selectionRevision.current || !shared.isCurrent()) return; if (channelId && input.target.kind === 'channel_task' && input.target.channel_id === channelId) { setSavedInput(input); setSelected(`task:${input.target.task_id}`); setRecipient(input.target.bot_id); storage.writeStorage(selectionKey, { bot_id: input.target.bot_id, task_id: input.target.task_id }) } else if (draftRef.current === submittedDraft && routeActive.current && !channelId && (input.target.kind === 'task' || input.target.kind === 'channel_task') && input.target.task_id !== taskId) router.push(`/task/${input.target.task_id}`) } }).then((saved) => { if (saved && shared.isCurrent()) { if (draftRef.current === submittedDraft) { setText((current) => { if (current !== submitted) return current; storage.writeStorage(draftKey, ''); return '' }); setFiles((current) => current === selectedFiles ? [] : current); setIntent('continue') } void read().catch((cause) => setError(messageOf(cause))) } }).catch((cause) => setError(messageOf(cause))).finally(() => { savingDrafts.current.delete(submittedDraft) })
    }}>{error ? <ErrorText error={error} /> : null}</Composer></View></KeyboardStickyView>
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
  return <KeyboardAwareScrollView keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-4 p-4 pb-10"><Stack.Screen options={{ title: '任务上下文和历史' }} /><Text role="heading" className="text-heading font-semibold">{task.title}</Text><Text className="text-meta">独立任务对话 · 第 {task.revision} 版{assignment ? ` · 当前执行从第 ${assignment.started_revision} 版开始` : ''}</Text><View className="gap-2 rounded-xl border border-border bg-card p-4"><Text className="font-medium">本轮来源</Text><Text selectable>交代内容 · {task.request}</Text><Text>期望结果 · {task.expected_result}</Text>{assignment?.work_id ? <Link href={`/b/${task.owner_bot}/work/${assignment.work_id}`} asChild><Button variant="outline"><Text>查看执行工作</Text></Button></Link> : null}<Text>依赖任务 · {task.dependencies.length ? task.dependencies.map((id) => shared.workspaceState?.tasks.find((item) => item.task_id === id)?.title ?? id).join('、') : '无'}</Text><Details label="固定依据和引用来源"><Text selectable className="text-meta">任务来源事件 · {task.request_event}</Text>{task.source_events.map((source) => <Text selectable key={source} className="text-meta">引用来源 · {source}</Text>)}{assignment ? <Text selectable className="text-meta">本轮依据 · {JSON.stringify(assignment.basis)}</Text> : null}</Details></View><Text className="text-meta text-muted-foreground">共享历史、Bot 的长期记忆与执行服务的上下文分别保存。只有本轮来源记录中的内容才显示为引用。</Text>{contextError ? <ErrorText error={contextError} /> : null}{scopeContext?.available ? <ScopeSources context={scopeContext.context} /> : <Text className="text-meta text-muted-foreground">{scopeContext ? scopeContext.reason || "这个任务尚无已编译的上下文。" : contextError ? "本次未读到任务上下文。" : "正在查询这个任务的上下文。"}</Text>}<Link href={`/b/${task.owner_bot}/context`} asChild><Button variant="outline"><Icon as={BrainCircuit} /><Text>打开 Bot 上下文和持久记忆</Text></Button></Link>
    {shared.pendingMessages.filter((item) => item.target.kind === 'task' && item.target.task_id === taskId || item.target.kind === 'channel_task' && item.target.task_id === taskId).map((item) => <InputStatus key={item.message_id} pending={item} />)}
    <Button variant="outline" disabled={Boolean(busy)} onPress={() => void run("read-context", async () => { setScopeContext(await shared.readScopeContext({ target })) })}><Text>重新读取上下文</Text></Button>
    <Button variant="outline" onPress={() => { setCorrection(task.request); setEditing((value) => !value) }}><Text>纠正任务</Text></Button>{editing ? <View className="gap-3"><Text nativeID="revision-label" className="text-meta font-medium">更新任务交代</Text><Input accessibilityLabel="更新任务交代" accessibilityLabelledBy="revision-label" multiline className="min-h-36 py-3" textAlignVertical="top" value={correction} onChangeText={setCorrection} maxLength={100000} /><Text className="text-meta text-muted-foreground">保留任务身份并新增版本。旧版本结果不能完成这次更新。</Text><Button disabled={Boolean(busy) || !shared.writable} onPress={() => void run('revise-task', async () => { if (!correction.trim()) throw new Error('请填写更新后的交代。'); const result = await shared.workspaceCommand({ kind: 'revise_task', task_id: task.task_id, expected_revision: task.revision, request: correction.trim(), expected_result: task.expected_result, dependencies: task.dependencies }); const revised = result.tasks.find((item) => item.task_id === taskId); if (!revised) throw new Error('任务修订已提交，尚未确认新版本。'); await shared.sendMessage(correction.trim(), [], undefined, { target, expected_revision: revised.revision }); setEditing(false) })}><Text>保存修订并补充</Text></Button></View> : null}{error ? <ErrorText error={error} /> : null}
  </KeyboardAwareScrollView>
}
