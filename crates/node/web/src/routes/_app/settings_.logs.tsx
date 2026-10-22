import { createFileRoute } from '@tanstack/react-router'
import { useCallback, useEffect, useRef, useState } from 'react'
import { Bot, ChevronDown, CircleX, Code, Copy, Globe, Info, Layers, ListChecks, MessagesSquare, MousePointerClick, RefreshCw, ScrollText, Search, Server, Terminal, TriangleAlert, X, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { EmptyState, InlineError, toast } from '@/components/app/kit'
import { useDevice } from '@/features/identity/context'
import { useWorkspace } from '@/features/workspace/context'
import type { LogFilter, LogLevel, LogRecord, LogSource } from '@/lib/web-log'

export const Route = createFileRoute('/_app/settings_/logs')({ component: LogsPage })

const levelChoices: Array<[LogLevel | undefined, string, LucideIcon]> = [
  [undefined, '全部', Layers], ['info', '信息', Info], ['warn', '警告', TriangleAlert], ['error', '错误', CircleX],
]
const sourceChoices: Array<[LogSource, string, LucideIcon]> = [
  ['node', '设备', Server], ['web', '网页', Globe], ['agent', 'Bot', Bot], ['cli', '命令行', Terminal], ['codex', 'Codex', Code], ['cursor', 'Cursor', MousePointerClick],
]
const sourceLabel = Object.fromEntries(sourceChoices.map(([value, label]) => [value, label])) as Record<LogSource, string>
const levelLook: Record<LogLevel, { icon: LucideIcon; tone: string; label: string }> = {
  error: { icon: CircleX, tone: 'text-danger', label: '错误' },
  warn: { icon: TriangleAlert, tone: 'text-warning', label: '警告' },
  info: { icon: Info, tone: 'text-info', label: '信息' },
  debug: { icon: Info, tone: 'text-muted-foreground', label: '调试' },
  trace: { icon: Info, tone: 'text-muted-foreground', label: '跟踪' },
}
const SHOWN = 500

function LogsPage() {
  const workspace = useWorkspace()
  const device = useDevice()
  const allowed = device.ready?.principal.can_manage || device.ready?.principal.source === 'local_session'
  const [level, setLevel] = useState<LogLevel | undefined>()
  const [sources, setSources] = useState<LogSource[]>([])
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
      const filter: LogFilter = { level, sources, limit: SHOWN, ...(text ? { text } : {}) }
      setRecords((await read.current(filter)).reverse()); setError(null); setOpen(null)
    } catch (cause) { setError(cause) } finally { setBusy(false) }
  }, [level, sources, text])
  useEffect(() => { if (allowed) void load() }, [allowed, load])
  const copyAll = async (filter: LogFilter, what: string) => {
    try {
      const all = await read.current({ ...filter, limit: 20000 })
      await navigator.clipboard.writeText(all.map((record) => JSON.stringify(record)).join('\n'))
      toast(`已复制这个${what}的 ${all.length} 条记录`, { tone: 'success' })
    } catch (cause) { setError(cause) }
  }
  if (!allowed) return <EmptyState icon={ScrollText} title="看不了日志" description="只有管理设备或本机打开的页面可以查看日志。" />
  return <div className="mx-auto grid w-full max-w-3xl gap-4">
    <div className="flex items-center justify-end gap-2">
      <Button type="button" variant="ghost" size="icon" className="-me-2.5 max-md:-me-3.5" aria-label="刷新" title="刷新" disabled={busy} onClick={() => { void load() }}>
        <RefreshCw aria-hidden="true" className={cn(busy && 'animate-spin')} />
      </Button>
    </div>
    <div className="relative">
      <Search aria-hidden="true" className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
      <Input type="search" aria-label="搜索日志" placeholder="搜索内容或编号" className="h-10 ps-9" value={query} onChange={(event) => setQuery(event.target.value)} />
    </div>
    <div role="radiogroup" aria-label="级别" className="flex w-full gap-1 rounded-lg bg-muted p-1 sm:w-fit">
      {levelChoices.map(([value, label, Icon]) => <Button key={label} type="button" role="radio" aria-checked={level === value} variant={level === value ? 'outline' : 'ghost'} className="flex-1 sm:flex-none" onClick={() => setLevel(value)}>
        <Icon aria-hidden="true" />{label}
      </Button>)}
    </div>
    <div role="group" aria-label="来源" className="flex flex-wrap gap-2">
      {sourceChoices.map(([value, label, Icon]) => {
        const on = sources.includes(value)
        return <button key={value} type="button" aria-pressed={on} onClick={() => setSources(on ? sources.filter((item) => item !== value) : [...sources, value])}
          className={cn('inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-meta font-medium transition-colors max-md:h-10 focus-visible:ring-3 focus-visible:ring-ring/50 outline-none',
            on ? 'border-primary bg-primary text-primary-foreground' : 'bg-background text-muted-foreground hover:bg-muted hover:text-foreground')}>
          <Icon aria-hidden="true" className="size-3.5" />{label}
        </button>
      })}
    </div>
    <InlineError error={error} />
    {records === null ? null : records.length === 0
      ? <EmptyState icon={ScrollText} title="没有符合条件的记录"
        action={level || sources.length || query ? <Button type="button" variant="outline" onClick={() => { setLevel(undefined); setSources([]); setQuery('') }}><X aria-hidden="true" />清除筛选</Button> : undefined} />
      : <>
        <p className="text-meta text-muted-foreground" aria-live="polite">{records.length >= SHOWN ? `最新 ${SHOWN} 条` : `共 ${records.length} 条`}</p>
        <ul className="divide-y overflow-hidden rounded-xl border bg-card">
          {records.map((record, index) => <LogRow key={`${record.ts_ms}-${index}`} record={record} chat={workspace.bots.find((bot) => bot.conversation_id === record.conversation_id)?.name} open={open === index} onToggle={() => setOpen(open === index ? null : index)} onCopyAll={copyAll} />)}
        </ul>
      </>}
  </div>
}

function time(ms: number) {
  const date = new Date(ms), today = new Date().toDateString() === date.toDateString()
  const clock = date.toLocaleTimeString('zh-CN', { hour12: false })
  return today ? clock : `${date.toLocaleDateString('zh-CN', { month: '2-digit', day: '2-digit' })} ${clock}`
}

/** `chat`: the bot whose chat the record belongs to, shown instead of its ID. */
function LogRow({ record, chat, open, onToggle, onCopyAll }: { record: LogRecord; chat?: string; open: boolean; onToggle(): void; onCopyAll(filter: LogFilter, what: string): Promise<void> }) {
  const look = levelLook[record.level]
  const ids: Array<[string, string | undefined]> = [['对话', chat ?? record.conversation_id], ['任务', record.work_id], ['Codex 会话', record.thread_id]]
  return <li>
    <button type="button" aria-expanded={open} onClick={onToggle} className="flex w-full items-start gap-3 px-4 py-3 text-left hover:bg-muted/50 focus-visible:bg-muted/50 outline-none">
      <look.icon aria-label={look.label} className={cn('mt-0.5 size-4 shrink-0', look.tone)} />
      <span className="grid min-w-0 flex-1 gap-1">
        <span className={cn('break-words', !open && 'line-clamp-2')}>{record.message || record.event}</span>
        <span className="flex min-w-0 items-center gap-2 text-meta text-muted-foreground">
          <time className="shrink-0 tabular-nums" dateTime={new Date(record.ts_ms).toISOString()}>{time(record.ts_ms)}</time>
          <span className="shrink-0">{sourceLabel[record.source]}</span>
        </span>
      </span>
      <ChevronDown aria-hidden="true" className={cn('mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform', open && 'rotate-180')} />
    </button>
    {open ? <div className="grid gap-3 px-4 pb-4 ps-11 text-meta">
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        <dt className="text-muted-foreground">事件</dt><dd className="min-w-0 break-all font-mono text-xs leading-5">{record.event}</dd>
        {ids.map(([label, id]) => id ? <div key={label} className="contents"><dt className="text-muted-foreground">{label}</dt><dd className="min-w-0 break-all font-mono text-xs leading-5">{id}</dd></div> : null)}
      </dl>
      {record.fields && Object.keys(record.fields).length ? <pre className="max-h-64 overflow-auto rounded-lg bg-muted p-3 font-mono text-xs whitespace-pre-wrap break-all">{JSON.stringify(record.fields, null, 2)}</pre> : null}
      <div className="flex flex-wrap gap-2">
        <Button type="button" size="sm" variant="outline" onClick={() => { void navigator.clipboard.writeText(JSON.stringify(record)).then(() => toast('已复制这条记录', { tone: 'success' })) }}><Copy aria-hidden="true" />复制这条</Button>
        {record.conversation_id ? <Button type="button" size="sm" variant="outline" onClick={() => { void onCopyAll({ conversation_id: record.conversation_id }, '对话') }}><MessagesSquare aria-hidden="true" />复制整个对话</Button> : null}
        {record.work_id ? <Button type="button" size="sm" variant="outline" onClick={() => { void onCopyAll({ work_id: record.work_id }, '任务') }}><ListChecks aria-hidden="true" />复制整个任务</Button> : null}
      </div>
    </div> : null}
  </li>
}
