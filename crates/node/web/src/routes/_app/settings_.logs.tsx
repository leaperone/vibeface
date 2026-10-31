import { createFileRoute } from '@tanstack/react-router'
import { useCallback, useEffect, useRef, useState } from 'react'
import { Bot, ChevronDown, CircleX, Code, Copy, Globe, Layers, ListChecks, MessagesSquare, MousePointerClick, RefreshCw, ScrollText, Search, Terminal, TriangleAlert, X, type LucideIcon } from 'lucide-react'
import { cn } from '@/lib/utils'
import { terms } from '@/lib/glossary'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Details, EmptyState, InlineError, Segmented, Skeleton, toast } from '@/components/app/kit'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { knownError } from '@vibeface/client/lib/errors.ts'
import { nodeName } from '@vibeface/client/workspace/labels.ts'
import type { LogFilter, LogLevel, LogRecord, LogSource } from '@vibeface/client/lib/web-log.ts'

export const Route = createFileRoute('/_app/settings_/logs')({ component: LogsPage })

type LevelChoice = 'all' | Extract<LogLevel, 'warn' | 'error'>
const levelChoices: Array<{ value: LevelChoice; label: string; icon: LucideIcon }> = [
  { value: 'all', label: '全部', icon: Layers }, { value: 'warn', label: '有问题', icon: TriangleAlert }, { value: 'error', label: '错误', icon: CircleX },
]
const sourceChoices: Array<[LogSource, string, LucideIcon]> = [
  ['node', terms.thisComputer.label, terms.thisComputer.icon], ['web', '网页', Globe], ['agent', 'Bot', Bot], ['cli', '命令行', Terminal], ['codex', 'Codex', Code], ['cursor', 'Cursor', MousePointerClick],
]
const sourceLabel = Object.fromEntries(sourceChoices.map(([value, label]) => [value, label])) as Record<LogSource, string>
const sourceIcon = Object.fromEntries(sourceChoices.map(([value, , icon]) => [value, icon])) as Record<LogSource, LucideIcon>
/** Only problems get a color and their own icon; everything else stays quiet. */
const alertLook: Partial<Record<LogLevel, { icon: LucideIcon; tone: string; label: string }>> = {
  error: { icon: CircleX, tone: 'text-danger', label: '错误' },
  warn: { icon: TriangleAlert, tone: 'text-warning', label: '有问题' },
}

/** Codex and Cursor print their own output; its ERROR words are not this device's errors, so it stays grey and out of the error filter. */
const external = (record: LogRecord) => record.source === 'codex' || record.source === 'cursor'

type Fields = Record<string, unknown>
const str = (value: unknown) => typeof value === 'string' && value ? value : undefined
const turnEnded: Record<string, string> = { completed: 'Bot 回复完成', failed: 'Bot 回复失败', interrupted: 'Bot 回复中断', cancelled: 'Bot 回复已取消' }
const workStates: Record<string, string> = { accepted: '任务已接下', starting: '任务准备开始', running: '任务进行中', interrupting: '任务正在停止', unverified: '任务结果待确认' }
const workOutcomes: Record<string, string> = { completed: '任务完成', failed: '任务失败', interrupted: '任务已中断', rejected: '任务被拒绝', unverified: '任务结果待确认' }
/** Plain titles for the stable event names the device and this page write; others show their own message. */
const eventTitles: Record<string, string | ((fields: Fields, name: (device?: string) => string) => string)> = {
  'agent.turn.started': 'Bot 开始回复',
  'lan.off': '局域网发现没有启动',
  'agent.turn.finished': (f) => turnEnded[String(f.state)] ?? (f.error ? 'Bot 回复没能保存' : 'Bot 回复结束'),
  'agent.turn.fenced': '对话交给了其他设备，这里停止回复',
  'agent.turn.steer_refused': '新消息等下一次回复',
  'agent.turn.steer_unknown': '新消息是否插入回复待确认',
  'agent.tool': 'Bot 使用工具',
  'agent.tool_failed': 'Bot 使用工具失败',
  'bot.deleted': 'Bot 已删除',
  'bot.primary': '已设置主 Bot',
  'bot.stop_failed': '删除的 Bot 没能停下',
  'providers.discovered': '在这台电脑上找到 AI 工具',
  'providers.applied': 'AI 工具设置已更新',
  'question.asked': '向你提了一个问题',
  'question.answered': '问题已回答',
  'question.closed': '问题已关闭',
  'schedule.skipped_late': '错过了定时，已跳过',
  'session.attributed': '新会话归给主 Bot',
  'sync.event_rejected': '拒收了一条不合规的记录',
  'work.dispatched': (f, name) => f.target ? `任务已交给「${name(str(f.target))}」` : '收到任务',
  'work.dispatch_rejected': (f, name) => `「${name(str(f.target))}」拒绝了任务`,
  'work.turn_started': '任务开始执行',
  'work.start_failed': '任务没能开始',
  'work.start_unknown': '不确定任务是否开始',
  'work.thread_bound': '任务已连上 Codex',
  'work.state': (f) => (f.state === 'finished' ? workOutcomes[String(f.outcome).toLowerCase()] : workStates[String(f.state)]) ?? '任务状态变化',
  'work.start_rejected_pending': '任务没能开始，稍后再确认',
  'work.stop_pending': '任务稍后停止',
  'app_server.start_failed': 'Codex 没能启动',
  'app_server.exited': 'Codex 已退出',
  'app_server.restarted': 'Codex 按新设置重启',
  'app_server.leftover_stopped': '停止了上次留下的 Codex',
  'app_server.link_failed': 'Bot 没能换到新的 Codex 连接',
  'app_server.event_failed': '没能处理 Codex 的一条消息',
  'claude.turn_unrecorded': 'Claude Code 回复没能记下，下次会重看',
  'claude.work_update_pending': 'Claude Code 任务进度稍后记录',
  'claude.work_end_pending': 'Claude Code 任务结束稍后确认',
  'cursor.prewarm_failed': 'Cursor 没能提前启动',
  'opencode.leftover_stopped': '停止了上次留下的 opencode',
  'opencode.work_update_pending': 'opencode 任务进度稍后确认',
  'opencode.send_unknown': '不确定 opencode 是否接下任务',
  'pi.work_report_pending': 'pi 任务进度稍后确认',
  'providers.missing': '没找到 AI 工具的程序',
  'request.received': '收到 Codex 的请求',
  'request.asked_again': 'Codex 再次发来请求',
  'request.held': '会话的请求在等你',
  'request.answered': '已回复会话的请求',
  'request.lost': '连接断开，请求作废',
  'request.unrouted': '请求没能交给 Bot，已按默认处理',
  'request.question_lookup_failed': '没找到请求对应的问题',
  'request.expire_failed': '过期的请求没能回复',
  'triage.test_received': '收到测试提醒',
  'triage.wake_failed': '没能把会话动态交给 Bot',
  'webhook.failed': '外部聊天发送失败',
  'webhook.dead': '外部聊天多次发送失败，已放弃',
  'webhook.answers_failed': '没能把回答告诉外部聊天',
  'sessions.scan_failed': '没能扫描会话',
  'schedule.reconcile_pending': '定时任务稍后再确认',
  'schedule.runs_pending': '定时任务稍后执行',
  'schedule.drain_pending': '对话的定时任务稍后执行',
  'schedule.panicked': '定时任务出错，进度已保留',
  'relay.started': '转发已开启',
  'join.local_failed': '这台电脑没能加入',
  'network.removed': '这台电脑已被移出网络',
  'node.data_set_aside': '旧数据已改名保留',
  'node.data_set_aside_failed': '旧数据没能改名保留',
  'recovery.install_pending': '找回结果稍后生效',
  'devices.removed_work_pending': '移除设备上的任务稍后处理',
  'devices.removed_requests_pending': '发给移除设备的请求稍后清理',
  'sync.write_state': '写入状态已变化',
  'sync.write_state_pending': '写入状态稍后更新',
  'sync.write_confirmed': '已确认可以写入',
  'sync.forked': '这台设备的另一份副本写了不同记录，已停止写入',
  'sync.fork_evidence': '收到冲突的记录，已留作凭据',
  'codex.call': '调用 Codex',
  'codex.call_failed': '调用 Codex 失败',
  'web.rpc_failed': '网页请求失败',
  'web.codex_missing': '没找到 Codex',
  'web.action_failed': (f) => `${str(f.action) ?? '操作'}失败`,
  'web.uncaught': '网页出错',
  'web.unhandled_rejection': '网页出错',
}

/** What each Bot tool does, for 'Bot 使用工具'; the tool's own name stays under 技术详情. */
const toolNames: Record<string, string> = {
  apply_provider_config: '应用 AI 设置', bind_work: '关联任务', call_lookup: '查调用结果', configure_bot: '配置 Bot', context: '读取上下文',
  create_bot: '创建 Bot', delete_bot: '删除 Bot', dispatch_work: '派任务', fetch_url: '打开网页', forget: '忘掉一条记忆', hand_over_session: '交接会话',
  interrupt_work: '停止任务', list_bots: '查看 Bot', list_sessions: '查看会话', message_bot: '给 Bot 发消息', nodes: '查看设备',
  propose_provider_config: '提议 AI 设置', read_bot: '读取 Bot', read_skill: '读取技能', recall: '回想记忆', remember: '记下一条记忆', remind: '设提醒',
  remove_schedule: '删除定时任务', remove_skill: '删除技能', save_schedule: '保存定时任务', save_skill: '保存技能', track_work: '跟进任务',
  update_profile: '更新资料', wait_work: '等任务结果', work_status: '查任务状态',
}

/** A short title, plus the reason when the record carries one; English the page cannot translate stays under 技术详情. */
function describe(record: LogRecord, name: (device?: string) => string): { title: string; detail?: string } {
  const fields = record.fields ?? {}
  const known = eventTitles[record.event]
  const title = typeof known === 'function' ? known(fields, name) : known
  if (!title) return { title: knownError(record.message) ?? record.message ?? record.event }
  const tool = str(fields.tool)
  const reason = str(fields.error) ?? str(fields.reason) ?? str(fields.detail) ?? (record.source === 'web' ? record.message : undefined)
  const detail = [tool ? toolNames[tool] : undefined, reason ? knownError(reason) : undefined].filter(Boolean).join('：') || undefined
  return { title, detail: detail === title ? undefined : detail }
}
const SHOWN = 500

function LogsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const allowed = device.ready?.principal.can_manage || device.ready?.principal.source === 'local_session'
  const [level, setLevel] = useState<LogLevel | undefined>()
  const [source, setSource] = useState<LogSource | undefined>()
  const [query, setQuery] = useState('')
  const [text, setText] = useState('')
  const [records, setRecords] = useState<LogRecord[] | null>(null)
  const [error, setError] = useState<unknown>(null)
  const [busy, setBusy] = useState(false)
  const [open, setOpen] = useState<number | null>(null)
  const read = useRef(workspace.logs)
  read.current = workspace.logs
  useEffect(() => { const timer = setTimeout(() => setText(query.trim()), 250); return () => clearTimeout(timer) }, [query])
  const load = useCallback(async () => {
    setBusy(true)
    try {
      const filter: LogFilter = { level, sources: source ? [source] : [], limit: SHOWN, ...(text ? { text } : {}) }
      const found = await read.current(filter)
      setRecords((level === 'error' ? found.filter((record) => !external(record)) : found).reverse()); setError(null); setOpen(null)
    } catch (cause) { setError(cause) } finally { setBusy(false) }
  }, [level, source, text])
  useEffect(() => { if (allowed) void load() }, [allowed, load])
  const copyAll = async (filter: LogFilter, what: string) => {
    try {
      const all = await read.current({ ...filter, limit: 20000 })
      await navigator.clipboard.writeText(all.map((record) => JSON.stringify(record)).join('\n'))
      toast(`已复制 ${all.length} 条${what}日志`, { tone: 'success' })
    } catch (cause) { setError(cause) }
  }
  const works = [...workspace.openWorks, ...(workspace.data?.works ?? [])]
  if (!allowed) return <EmptyState icon={ScrollText} title="看不了日志" description="只有管理设备或本机页面能看" />
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-4">
    <div className="flex items-center gap-2">
      <div className="relative min-w-0 flex-1">
        <Search aria-hidden="true" className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
        <Input type="search" aria-label="搜索日志" placeholder="搜索日志" className="h-10 ps-9 max-md:h-11" value={query} onChange={(event) => setQuery(event.target.value)} />
      </div>
      <Button type="button" variant="ghost" size="icon" className="-me-2.5 max-md:-me-3.5" aria-label="刷新" title="刷新" disabled={busy} onClick={() => { void load() }}>
        <RefreshCw aria-hidden="true" className={cn(busy && 'animate-spin')} />
      </Button>
    </div>
    <div className="flex flex-wrap items-center gap-2">
      <Segmented label="级别" labels fill value={level ?? 'all'} options={levelChoices} onChange={(value) => setLevel(value === 'all' ? undefined : value)} />
      <NativeSelect aria-label="来源" className="ms-auto max-sm:w-full [&>select]:h-9 max-md:[&>select]:h-12" value={source ?? ''} onChange={(event) => setSource((event.target.value || undefined) as LogSource | undefined)}>
        <NativeSelectOption value="">全部来源</NativeSelectOption>
        {sourceChoices.map(([value, label]) => <NativeSelectOption key={value} value={value}>{label}</NativeSelectOption>)}
      </NativeSelect>
    </div>
    <InlineError error={error} />
    {records === null ? error ? null : <div className="grid gap-2" aria-busy="true"><Skeleton className="h-12" /><Skeleton className="h-12" /><Skeleton className="h-12" /></div> : records.length === 0
      ? <EmptyState icon={ScrollText} title={level || source || query ? "没有符合条件的记录" : "还没有日志"}
        action={level || source || query ? <Button type="button" variant="outline" onClick={() => { setLevel(undefined); setSource(undefined); setQuery('') }}><X aria-hidden="true" />清除筛选</Button> : undefined} />
      : <>
        <p className="text-meta text-muted-foreground" aria-live="polite">{records.length >= SHOWN ? `最新 ${SHOWN} 条` : `共 ${records.length} 条`}</p>
        <ul className="divide-y overflow-hidden rounded-xl border bg-card shadow-xs">
          {records.map((record, index) => <LogRow key={`${record.ts_ms}-${index}`} record={record} name={(id) => nodeName(id, { nodes: workspace.nodes, directory: device.directory })} chat={workspace.bots.find((bot) => bot.conversation_id === record.conversation_id)?.name} task={record.work_id ? works.find((work) => work.work_id === record.work_id)?.title : undefined} open={open === index} onToggle={() => setOpen(open === index ? null : index)} onCopyAll={copyAll} />)}
        </ul>
      </>}
  </div>
}

function time(ms: number) {
  const date = new Date(ms), today = new Date().toDateString() === date.toDateString()
  const clock = date.toLocaleTimeString('zh-CN', { hour12: false })
  return today ? clock : `${date.toLocaleDateString('zh-CN', { month: '2-digit', day: '2-digit' })} ${clock}`
}

/** `chat` and `task`: the names shown instead of their IDs, which stay under the technical details. */
function LogRow({ record, name, chat, task, open, onToggle, onCopyAll }: { record: LogRecord; name: (device?: string) => string; chat?: string; task?: string; open: boolean; onToggle(): void; onCopyAll(filter: LogFilter, what: string): Promise<void> }) {
  const outside = external(record)
  const alert = outside ? undefined : alertLook[record.level]
  const quiet = outside || record.level === 'debug' || record.level === 'trace'
  const { title, detail } = describe(record, name)
  const Icon = alert?.icon ?? (record.source === 'node' ? undefined : sourceIcon[record.source])
  const names: Array<[string, string | undefined]> = [['对话', chat], ['任务', task]]
  const ids: Array<[string, string | undefined]> = [['事件', record.event], ['原文', record.message === title ? undefined : record.message], ['对话', record.conversation_id], ['任务', record.work_id], ['会话', record.thread_id]]
  return <li>
    <button type="button" aria-expanded={open} onClick={onToggle} className="interactive-row flex w-full items-start gap-3 px-4 py-3 text-left">
      {!Icon ? <span aria-hidden="true" className="size-4 shrink-0" /> : alert ? <Icon aria-label={alert.label} className={cn('mt-0.5 size-4 shrink-0', alert.tone)} /> : <Icon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" />}
      <span className="grid min-w-0 flex-1 gap-1">
        <span className={cn('break-words', !open && 'line-clamp-2', quiet && 'text-muted-foreground', outside && 'font-mono text-meta')}>{title}</span>
        {detail ? <span className={cn('break-words text-meta text-muted-foreground', !open && 'line-clamp-1')}>{detail}</span> : null}
        <span className="flex min-w-0 items-center gap-2 text-meta text-muted-foreground">
          <time className="shrink-0 tabular-nums" dateTime={new Date(record.ts_ms).toISOString()}>{time(record.ts_ms)}</time>
          {record.source === 'node' ? null : <span className="shrink-0">{sourceLabel[record.source]}</span>}
        </span>
      </span>
      <ChevronDown aria-hidden="true" className={cn('mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform duration-200', open && 'rotate-180')} />
    </button>
    {open ? <div className="grid gap-3 px-4 pb-4 ps-11 text-meta">
      {names.some(([, name]) => name) ? <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        {names.map(([label, name]) => name ? <div key={label} className="contents"><dt className="text-muted-foreground">{label}</dt><dd className="min-w-0 break-words">{name}</dd></div> : null)}
      </dl> : null}
      <Details label="技术详情">
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
          {ids.map(([label, id]) => id ? <div key={label} className="contents"><dt>{label}</dt><dd className="min-w-0 break-all font-mono text-meta text-foreground">{id}</dd></div> : null)}
        </dl>
        {record.fields && Object.keys(record.fields).length ? <pre className="max-h-64 overflow-auto rounded-lg bg-muted p-3 font-mono text-meta whitespace-pre-wrap break-all text-foreground">{JSON.stringify(record.fields, null, 2)}</pre> : null}
      </Details>
      <div className="flex flex-wrap gap-2">
        <Button type="button" size="sm" variant="outline" onClick={() => { void navigator.clipboard.writeText(JSON.stringify(record)).then(() => toast('已复制这条记录', { tone: 'success' })) }}><Copy aria-hidden="true" />复制这条</Button>
        {record.conversation_id ? <Button type="button" size="sm" variant="outline" onClick={() => { void onCopyAll({ conversation_id: record.conversation_id }, '对话') }}><MessagesSquare aria-hidden="true" />复制对话日志</Button> : null}
        {record.work_id ? <Button type="button" size="sm" variant="outline" onClick={() => { void onCopyAll({ work_id: record.work_id }, '任务') }}><ListChecks aria-hidden="true" />复制任务日志</Button> : null}
      </div>
    </div> : null}
  </li>
}
