import { Stack, useFocusEffect, useLocalSearchParams } from 'expo-router'
import { Bot, CalendarClock, CircleHelp, Laptop, ListChecks, MessagesSquare, Send, type LucideIcon } from 'lucide-react-native'
import { useCallback, useRef, useState, type ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { messageOf, providerNames } from '@vibeface/client/lib/node.ts'
import { formatWhen, scheduleFrequency } from '@vibeface/client/workspace/bot-settings.ts'
import { externalSessionLabels, externalSessionStatus, nodeName, schedulerStatus, shownState, staleTaskLabel, taskLabel, taskStale } from '@vibeface/client/workspace/labels.ts'
import { awaitsAnswer } from '@vibeface/client/workspace/questions.ts'
import type { BotRecord, ConversationPayload, ManagerJob } from '@vibeface/client/workspace/types.ts'
import { Details, EmptyState, providerIcons, SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'

/** Read the selected Bot's parsed shared snapshot without acknowledging its chat. */
export default function BotContext() {
  const { botId } = useLocalSearchParams<{ botId: string }>()
  const shared = useLive()
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
        if (current()) setError({ generation: shared.generation, botId, text: messageOf(cause, '读取上下文') })
      })
    }
    return () => { alive = false }
  }, [bot?.conversation_id, shared?.generation, shared?.connectionOnline, retry]))
  const data = shared && bot && shared.conversationId === bot.conversation_id && shared.data?.conversation?.conversation_id === bot.conversation_id ? shared.data : null
  const readError = error?.generation === shared?.generation && error?.botId === botId ? error.text : null
  if (!shared || !bot || !data) return <><Stack.Screen options={{ title: '上下文和协作' }} /><EmptyState icon={MessagesSquare}
    title={readError ?? (!shared || !shared.connectionOnline ? '上下文状态未知，连接后再读取' : !shared.synced ? '正在读取上下文' : !bot ? '找不到这个 Bot' : '正在读取这个 Bot 的上下文')}
    action={shared?.connectionOnline && bot ? <Button variant="outline" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button> : undefined} className="flex-1 justify-center" /></>
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-5 p-4 pb-10">
    <Stack.Screen options={{ title: '上下文和协作' }} />
    <View className="gap-1"><Text role="heading" className="text-title font-semibold">{bot.name}</Text><Text className="text-meta text-muted-foreground">当前 Bot 的共享记录，只读查看。</Text></View>
    {!shared.synced || !shared.connectionOnline ? <Text className="text-meta text-warning">{!shared.connectionOnline ? '连接已断开，显示最近记录；当前状态未知。' : '当前连接尚未完成首读，显示已知记录。'}</Text> : null}
    {readError ? <View className="gap-2"><Text className="text-danger">{readError}</Text><Button variant="outline" className="self-start" onPress={() => setRetry((value) => value + 1)}><Text>重新读取</Text></Button></View> : null}
    <ContextRecords bot={bot} data={data} shared={shared} />
  </ScrollView>
}

function ContextRecords({ bot, data, shared }: { bot: BotRecord; data: ConversationPayload; shared: NonNullable<ReturnType<typeof useLive>> }) {
  const nodes = shared.nodes
  const owner = data.conversation?.owner
  const remote = data.scheduler?.remote
  const scheduler = data.scheduler
  const jobs = scheduler?.jobs ?? []
  const open = jobs.filter((job) => schedulerStatus(job.state).group !== 'history')
  const history = jobs.filter((job) => schedulerStatus(job.state).group === 'history')
  const ownerNode = nodes.find((node) => node.device_id === owner)
  const connected = shared.connectionOnline && shared.synced
  const reach = !connected ? '状态未知，显示最近归属' : remote?.reachable === false ? '远端不可达，调度状态未知' : ownerNode?.revoked ? '设备已撤权' : ownerNode?.online === false ? '设备不可达' : ownerNode?.online ? '设备可达' : '设备可达性未知'
  const botNames = new Map(shared.bots.map((item) => [item.conversation_id, item]))
  const incoming = data.messages.filter((message) => message.role === 'bot' && message.sender)
  return <>
    <SettingsGroup title="Bot 身份">
      <SettingsRow icon={Bot} label={bot.name} description="共享聊天身份" href={`/b/${bot.bot_id}`} />
      <SettingsRow icon={Laptop} label="负责设备" description={reach}><Text className="max-w-[50%] text-muted-foreground">{owner ? nodeName(owner, { nodes }) : '归属未知'}</Text></SettingsRow>
      <SettingsRow icon={MessagesSquare} label="Bot 自身原生会话" description="当前快照未提供其状态，外部归属记录不能证明它可续接。" />
    </SettingsGroup>
    <SettingsGroup title="共享摘要">
      <SettingsRow icon={MessagesSquare} label="消息" description={`${data.messages.length} 条已加载消息`} href={`/b/${bot.bot_id}`} />
      <SettingsRow icon={ListChecks} label="工作" description={`${data.works.length} 项已加载工作`} />
      {data.works.map((work) => {
        const state = shownState(work, nodes)
        return <SettingsRow key={work.work_id} icon={ListChecks} label={work.title || '任务'} description={taskStale(state, connected) ? staleTaskLabel : taskLabel(state)} href={`/b/${bot.bot_id}/work/${work.work_id}`} />
      })}
      <Fold label="最近消息摘要">{data.messages.length ? data.messages.slice(-4).map((message) => <View key={message.message_id} className="gap-1"><Text className="text-meta font-medium">{message.sender?.name ?? (message.role === 'user' ? '你' : 'Bot')} · {formatWhen(message.created_at_ms)}</Text><Text selectable>{message.text}</Text></View>) : <Text className="text-muted-foreground">当前记录没有聊天消息</Text>}</Fold>
    </SettingsGroup>
    <SettingsGroup title="持久记忆和定时任务">
      <Fold label={`持久记忆 · ${data.bot_settings.memory.length}`}>{data.bot_settings.memory.length ? data.bot_settings.memory.map((entry) => <Text key={entry.memory_id} selectable>{entry.text}</Text>) : <Text className="text-muted-foreground">当前记录没有持久记忆</Text>}</Fold>
      <Fold label={`定时任务 · ${data.bot_settings.schedules.length}`}>{data.bot_settings.schedules.length ? data.bot_settings.schedules.map((schedule) => <View key={schedule.schedule_id} className="gap-1"><Text className="font-medium">{schedule.name} · {schedule.enabled ? '启用' : '已暂停'}</Text><Text className="text-meta text-muted-foreground">{scheduleFrequency(schedule)}{schedule.next_run_at_ms === null ? ' · 尚未安排下一次运行' : ` · 下次 ${formatWhen(schedule.next_run_at_ms)}`}</Text><Text selectable>{schedule.prompt}</Text></View>) : <Text className="text-muted-foreground">当前记录没有待执行定时任务</Text>}</Fold>
    </SettingsGroup>
    <SettingsGroup title="问题和检查">
      {data.questions.length ? data.questions.map((question) => <SettingsRow key={question.question_id} icon={CircleHelp} label={question.text} description={awaitsAnswer(question, shared.answered) ? '等待回答' : question.closed ? '已失效' : '已回答'} href={`/b/${bot.bot_id}`} />) : <SettingsRow icon={CircleHelp} label="当前记录没有问题" />}
      {data.checks.map((check) => {
        const target = botNames.get(check.conversation_id)
        return <SettingsRow key={check.check_id} icon={CircleHelp} label="程序状态待确认" description={check.detail} href={target ? `/b/${target.bot_id}` : undefined} />
      })}
    </SettingsGroup>
    <SettingsGroup title="Bot 关注的外部会话">
      {data.sessions.length ? data.sessions.map((session) => <View key={`${session.node}:${session.session_id}`} className="gap-2 p-4">
        <SettingsRow icon={providerIcons[session.agent]} label={providerNames[session.agent]} description={`${session.muted ? '已静音' : '关注中'} · ${!connected ? '设备可达性未知' : externalSessionLabels[externalSessionStatus(session, nodes)]}`} />
        <Text className="text-meta text-muted-foreground">{nodeName(session.node, { nodes })} · {session.cwd || '未记录项目'} · {formatWhen(session.created_at_ms)}</Text>
        {session.summary ? <Text selectable>{session.summary}</Text> : null}
        <Details label="归属记录详情"><Text selectable className="font-mono text-meta">{session.session_id}</Text><Text className="text-meta text-muted-foreground">这是外部会话的归属记录。设备可达不代表会话仍在运行，也不提供 Bot 自身会话的恢复状态。</Text></Details>
      </View>) : <SettingsRow icon={MessagesSquare} label="尚无外部会话归属记录" description="Bot 自身运行不会自动建立这类外部记录。" />}
    </SettingsGroup>
    <SettingsGroup title="调度观察">
      <SettingsRow icon={CalendarClock} label={scheduler ? remote?.reachable === false || !connected ? '调度状态未知' : open.length ? `${open.length} 项未结观察` : '当前没有未结观察' : '调度状态未提供'} description="显示最近观察，不代表实际执行顺序。远端记录可能只包含近期部分项目。" />
      {open.map((job) => <JobObservation key={job.trigger} job={job} unknown={!connected || remote?.reachable === false} />)}
      {history.length ? <Fold label={`调度历史 · ${history.length}`}>{history.map((job) => <JobObservation key={job.trigger} job={job} unknown={false} />)}</Fold> : null}
      {scheduler?.unanswered.length ? <SettingsRow icon={CircleHelp} label={`${scheduler.unanswered.length} 条消息未等到回复`} description="回到对话查看和处理。" href={`/b/${bot.bot_id}`} /> : null}
    </SettingsGroup>
    <SettingsGroup title="Bot 协作消息">
      {incoming.map((message) => {
        const sender = message.sender
        const target = sender && botNames.get(sender.conversation_id)
        return <MessageRecord key={`received:${message.message_id}`} icon={MessagesSquare} label={`收到 ${sender?.name ?? '另一个 Bot'} 的消息`} text={message.text} time={message.created_at_ms} bot={target} />
      })}
      {data.sent.map((message) => <MessageRecord key={`sent:${message.message_id}`} icon={Send} label={`发给 ${botNames.get(message.to)?.name ?? '另一个 Bot'}`} text={message.text} time={message.created_at_ms} bot={botNames.get(message.to)} sent />)}
      {!incoming.length && !data.sent.length ? <SettingsRow icon={MessagesSquare} label="当前记录没有 Bot 协作消息" /> : null}
    </SettingsGroup>
    <Text className="text-meta text-muted-foreground">这里显示当前快照中的记录。共享检查点、完整执行历史和 Bot 原生会话状态未提供。回答、停止和结算仍在原有对话与任务详情中进行。</Text>
  </>
}

function Fold({ label, children }: { label: string; children: ReactNode }) {
  return <View className="px-4 py-2"><Details label={label}>{children}</Details></View>
}

function JobObservation({ job, unknown }: { job: ManagerJob; unknown: boolean }) {
  return <View className="gap-1 px-4 py-3"><Text className="font-medium">{unknown ? '状态未知' : schedulerStatus(job.state).label}{job.pending_check ? ' · 需要确认' : ''}</Text><Details label="调度记录详情"><Text selectable className="font-mono text-meta">{job.trigger}</Text>{job.failure ? <Text selectable className="text-danger">{job.failure.detail}</Text> : null}</Details></View>
}

function MessageRecord({ icon, label, text, time, bot, sent }: { icon: LucideIcon; label: string; text: string; time: number; bot?: BotRecord; sent?: boolean }) {
  return <View className="gap-2 p-4"><SettingsRow icon={icon} label={label} description={formatWhen(time)} href={bot ? `/b/${bot.bot_id}` : undefined} /><Text selectable>{text}</Text>{sent ? <Text className="text-meta text-muted-foreground">这是已写入的消息记录，投递和回复状态未提供。</Text> : null}</View>
}
