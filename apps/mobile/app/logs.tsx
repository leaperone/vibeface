import { CircleAlert, CircleX, RefreshCw, ScrollText } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { ScrollView, View } from 'react-native'
import type { LogFilter, LogLevel, LogRecord, LogSource } from '@vibeface/client/lib/web-log.ts'
import { nodeName } from '@vibeface/client/workspace/labels.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { ErrorText } from '@/components/action'
import { Details, EmptyState, Skeleton } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useRun } from '@/lib/use-run'
import { useLive } from '@/data/live'

const sourceLabels: Record<LogSource, string> = { node: '设备', web: '网页', agent: 'Bot', cli: '命令行', codex: 'Codex', cursor: 'Cursor' }
const levelLabels: Record<LogLevel, string> = { trace: '跟踪', debug: '调试', info: '信息', warn: '有问题', error: '错误' }
const external = (record: LogRecord) => record.source === 'codex' || record.source === 'cursor'
const eventTitles: Record<string, string> = {
  'agent.turn.started': 'Bot 开始回复',
  'agent.turn.finished': 'Bot 回复结束',
  'agent.turn.fenced': '对话交给了其他设备',
  'app_server.start_failed': 'Codex 没能启动',
  'app_server.exited': 'Codex 已退出',
  'input.accepted': '收到一条消息',
  'input.admitted': '消息已排队',
  'input.delivered': '另一个节点转来消息或任务结果',
  'input.rejected': '消息没有收下',
  'join.local_failed': '设备没能加入',
  'mesh.neighbor_down': '一个全网邻居断开了',
  'mesh.neighbor_up': '连上一个全网邻居',
  'net.accept_failed': '一个连进来的连接出错断开',
  'network.removed': '设备已被移出网络',
  'node.started': '节点已启动',
  'node.start_failed': '节点没能启动',
  'node.stopping': '节点正在停止',
  'node.stopped': '节点已停止',
  'peer.online': '和另一个节点同步上了',
  'peer.offline': '和另一个节点同步不上',
  'question.asked': '向你提了一个问题',
  'question.answered': '问题已回答',
  'schedule.fired': '定时任务到点，已排队',
  'sync.write_state': '写入状态已变化',
  'sync.forked': '设备副本发生冲突',
  'triage.concluded': 'Bot 看过会话动态',
  'triage.queued': '收到一条会话动态',
  'update.applied': '已更新',
  'update.checked': '检查了更新',
  'update.deferred': '节点正忙，更新推迟',
  'update.failed': '更新失败',
  'web.action_failed': '操作失败',
  'web.rpc_failed': '网页请求失败',
  'work.start_failed': '任务没能开始',
  'work.state': '任务状态变化',
}

const textField = (fields: Record<string, unknown> | undefined, keys: string[]) =>
  keys.map((key) => fields?.[key]).find((value): value is string => typeof value === 'string' && value.length > 0)

function titleOf(record: LogRecord) {
  if (Object.prototype.hasOwnProperty.call(eventTitles, record.event)) return eventTitles[record.event]
  return /[一-鿿]/.test(record.message) ? record.message.split(/\r?\n/)[0] || '设备日志' : '收到一条设备日志'
}

function timeOf(ms: number) {
  return new Date(ms).toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false })
}

export default function Logs() {
  const device = useDevice()
  const shared = useLive()
  const allowed = device.ready?.principal.can_manage === true || device.ready?.principal.source === 'local_session'
  const [records, setRecords] = useState<LogRecord[] | null>(null)
  const { busy, error, run } = useRun()
  const read = useRef(shared?.logs)
  read.current = shared?.logs
  const readLogs = (filter: LogFilter) => {
    const logs = read.current
    if (!logs) throw new Error('设备暂时离线。')
    return logs(filter)
  }
  useEffect(() => {
    if (!allowed || !shared) return
    void run('load', async () => setRecords((await readLogs({ limit: 100 })).reverse()))
  }, [allowed, shared?.generation])
  if (!shared) return <EmptyState icon={ScrollText} title="设备暂时离线" className="flex-1 justify-center" />
  if (!allowed) return <EmptyState icon={ScrollText} title="只有管理设备能看日志" className="flex-1 justify-center" />
  const works = [...shared.openWorks, ...(shared.data?.works ?? [])]
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-3 p-4 pb-10">
    <View className="flex-row items-center justify-between gap-3">
      <Text className="text-meta text-muted-foreground">只读显示最近 100 条</Text>
      <Button variant="ghost" size="icon" accessibilityLabel="刷新日志" disabled={busy !== null} onPress={() => void run('load', async () => setRecords((await readLogs({ limit: 100 })).reverse()))}>
        <Icon as={RefreshCw} className={busy === 'load' ? 'animate-spin' : undefined} />
      </Button>
    </View>
    {error ? <ErrorText error={error} /> : null}
    {records === null ? error ? null : <View className="gap-2"><Skeleton className="h-16 rounded-xl" /><Skeleton className="h-16 rounded-xl" /><Skeleton className="h-16 rounded-xl" /></View>
      : records.length ? records.map((record, index) => {
        const bot = record.conversation_id ? shared.bots.find((item) => item.conversation_id === record.conversation_id)?.name : undefined
        const work = record.work_id ? works.find((item) => item.work_id === record.work_id)?.title : undefined
        const target = textField(record.fields, ['target', 'target_node', 'device_id'])
        const computer = target ? nodeName(target, { nodes: shared.nodes, directory: device.directory }) : undefined
        return <LogRow key={`${record.ts_ms}:${index}`} record={record} bot={bot} work={work} computer={computer} />
      }) : <EmptyState icon={ScrollText} title="还没有日志" />}
  </ScrollView>
}

function LogRow({ record, bot, work, computer }: { record: LogRecord; bot?: string; work?: string; computer?: string }) {
  const title = titleOf(record)
  const quiet = external(record)
  const message = record.message !== title && /[一-鿿]/.test(record.message) ? record.message : undefined
  const fields = record.fields && Object.keys(record.fields).length ? JSON.stringify(record.fields, null, 2) : ''
  return <View className="rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">
    <View accessible accessibilityLabel={`${title}，${sourceLabels[record.source]}`} className="min-h-16 flex-row items-start gap-3 px-4 py-3">
      <Icon as={quiet ? ScrollText : record.level === 'error' ? CircleX : record.level === 'warn' ? CircleAlert : ScrollText} className={quiet ? 'mt-0.5 size-4 text-muted-foreground' : record.level === 'error' ? 'mt-0.5 size-4 text-danger' : record.level === 'warn' ? 'mt-0.5 size-4 text-warning' : 'mt-0.5 size-4 text-muted-foreground'} />
      <View className="min-w-0 flex-1 gap-1">
        <Text className="font-medium">{title}</Text>
        {message ? <Text numberOfLines={2} className="text-meta text-muted-foreground">{message}</Text> : null}
        <View className="flex-row flex-wrap gap-x-2 gap-y-0.5">
          <Text className="text-meta text-muted-foreground">{timeOf(record.ts_ms)}</Text>
          <Text className="text-meta text-muted-foreground">{sourceLabels[record.source]}</Text>
          {typeof record.fields?.repeated === 'number' ? <Text className="text-meta text-muted-foreground">又出现 {record.fields.repeated} 次</Text> : null}
          {bot ? <Text numberOfLines={1} className="max-w-[40%] text-meta text-muted-foreground">Bot · {bot}</Text> : null}
          {work ? <Text numberOfLines={1} className="max-w-[40%] text-meta text-muted-foreground">任务 · {work}</Text> : null}
          {computer ? <Text numberOfLines={1} className="max-w-[40%] text-meta text-muted-foreground">设备 · {computer}</Text> : null}
        </View>
      </View>
    </View>
    <Details label="技术详情">
      <View className="gap-1 px-4 pb-4">
        <Text className="text-meta text-muted-foreground">级别 · {quiet ? '外部输出' : levelLabels[record.level]}</Text>
        <Text selectable className="font-mono text-meta text-muted-foreground">事件 · {record.event}</Text>
        {record.message !== title ? <Text selectable className="font-mono text-meta text-muted-foreground">原文 · {record.message}</Text> : null}
        {record.conversation_id ? <Text selectable className="font-mono text-meta text-muted-foreground">对话 · {record.conversation_id}</Text> : null}
        {record.work_id ? <Text selectable className="font-mono text-meta text-muted-foreground">任务 · {record.work_id}</Text> : null}
        {record.thread_id ? <Text selectable className="font-mono text-meta text-muted-foreground">会话 · {record.thread_id}</Text> : null}
        {fields ? <Text selectable className="max-h-64 rounded-lg bg-muted p-3 font-mono text-meta">{fields}</Text> : null}
      </View>
    </Details>
  </View>
}
