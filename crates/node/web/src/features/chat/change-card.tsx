import { useId, type ReactNode } from 'react'
import { AlarmClock, ArrowRight, BookOpen, Bot, CircleCheck, CirclePause, Minus, NotebookPen, Plus, Repeat, ScrollText, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { Details } from '@/components/app/kit'
import { formatWhen, lineDiff, scheduleFrequency, type DiffLine } from '@/features/workspace/bot-settings'
import type { BotChange, BotChangeRecord, BotSchedule, BotSkill, ScheduleView } from '@/features/workspace/types'

function cardTitle(changes: BotChange[]) {
  const kind = changes[0]?.kind
  if (changes.some((change) => change.kind !== kind)) return '已更新'
  if (kind === 'profile') return '已更新 Bot'
  const items = changes as Extract<BotChange, { kind: 'skill' | 'schedule' | 'memory' }>[]
  const added = items.every((item) => !item.before), removed = items.every((item) => !item.after)
  if (kind === 'skill') return added ? '已存为 skill' : removed ? '已删除 skill' : '已更新 skill'
  if (kind === 'memory') return added ? '已记下' : removed ? '已忘记' : '已更新笔记'
  return added ? '已设定定时任务' : removed ? '已删除定时任务' : '已更新定时任务'
}

/** What one turn of the conversation changed, item by item, before → after. `schedules` are the conversation's schedules now. */
export function ChangeCard({ records, schedules }: { records: BotChangeRecord[]; schedules: readonly ScheduleView[] }) {
  const id = useId()
  const changes = records.map((record) => record.change)
  return <section aria-labelledby={id} className="grid gap-3 rounded-xl border bg-card p-4">
    <header className="flex items-center gap-2">
      <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />
      <h3 id={id} className="text-meta font-medium text-muted-foreground">{cardTitle(changes)}</h3>
    </header>
    <ul className="grid gap-4">
      {records.map((record) => <ChangeRows key={record.change_id} change={record.change} schedules={schedules} />)}
    </ul>
  </section>
}

function ChangeRows({ change, schedules }: { change: BotChange; schedules: readonly ScheduleView[] }) {
  if (change.kind === 'profile') return <>
    {change.name ? <Row icon={Bot} label="名称"><BeforeAfter before={change.name.before} after={change.name.after} /></Row> : null}
    {change.instructions ? <Row icon={ScrollText} label="说明"><DiffLines lines={lineDiff(change.instructions.before, change.instructions.after)} /></Row> : null}
  </>
  if (change.kind === 'skill') return <SkillRow before={change.before} after={change.after} />
  if (change.kind === 'memory') return <MemoryRow before={change.before} after={change.after} />
  return <ScheduleRow before={change.before} after={change.after} current={schedules.find((schedule) => schedule.schedule_id === change.schedule_id)} />
}

function Row({ icon: Icon, label, tag, removed, children }: { icon: LucideIcon; label: ReactNode; tag?: string; removed?: boolean; children?: ReactNode }) {
  return <li className="grid grid-cols-[1rem_minmax(0,1fr)] gap-x-3">
    <Icon aria-hidden="true" className="mt-1 size-4 text-muted-foreground" />
    <div className="grid min-w-0 gap-1">
      <p className="flex min-w-0 flex-wrap items-baseline gap-x-2 [overflow-wrap:anywhere]">
        <span className={cn('font-medium', removed && 'text-muted-foreground line-through')}>{label}</span>
        {tag ? <span className="text-meta text-muted-foreground">{tag}</span> : null}
      </p>
      {children}
    </div>
  </li>
}

/** "Old → new" inline, wrapping when long. */
function BeforeAfter({ before, after, className }: { before: string; after: string; className?: string }) {
  return <p className={cn('flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 [overflow-wrap:anywhere]', className)}>
    <span className="text-muted-foreground">{before}</span>
    <ArrowRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
    <span className="sr-only">改为</span>
    <span>{after}</span>
  </p>
}

/** Removed lines struck through, added lines highlighted; long diffs fold after a few lines. */
function DiffLines({ lines, limit = 6 }: { lines: DiffLine[]; limit?: number }) {
  if (!lines.length) return <p className="text-meta text-muted-foreground">只调整了空行</p>
  const shown = lines.length > limit ? lines.slice(0, limit - 2) : lines
  const rest = lines.slice(shown.length)
  return <>
    <DiffList lines={shown} />
    {rest.length ? <Details label={`还有 ${rest.length} 行`}><DiffList lines={rest} /></Details> : null}
  </>
}

function DiffList({ lines }: { lines: DiffLine[] }) {
  return <ul className="grid gap-0.5 text-body">
    {lines.map((line, index) => {
      const added = line.kind === 'added'
      return <li key={index} className={cn('flex min-w-0 gap-2 rounded-md px-2 py-0.5 whitespace-pre-wrap [overflow-wrap:anywhere] [word-break:normal]', added ? 'bg-success-soft text-foreground' : 'text-muted-foreground')}>
        {added ? <Plus aria-hidden="true" className="mt-1 size-4 shrink-0 text-success" /> : <Minus aria-hidden="true" className="mt-1 size-4 shrink-0" />}
        <span className="sr-only">{added ? '新增：' : '删去：'}</span>
        <span className={cn('min-w-0', !added && 'line-through')}>{line.text}</span>
      </li>
    })}
  </ul>
}

function SkillRow({ before, after }: { before: BotSkill | null; after: BotSkill | null }) {
  const skill = after ?? before!
  const renamed = before && after && before.name !== after.name
  const body = !after ? null : !before ? <p className="whitespace-pre-wrap text-foreground [word-break:normal] [overflow-wrap:anywhere]">{after.body}</p>
    : before.body !== after.body ? <DiffLines lines={lineDiff(before.body, after.body)} limit={Infinity} /> : null
  return <Row icon={BookOpen} label={skill.name} tag={!before ? '新 skill' : !after ? '已删除' : '已更新'} removed={!after}>
    {renamed ? <BeforeAfter className="text-meta" before={before.name} after={after.name} /> : null}
    {after?.description ? <p className="text-meta text-muted-foreground">{after.description}</p> : null}
    {body ? <Details label={before ? '内容改动' : '内容'}>{body}</Details> : null}
  </Row>
}

/**
 * A one-time schedule shows when it runs. A repeating one shows its next run from the schedule as it is now,
 * so a card for a removed, paused or finished one shows none.
 */
function ScheduleRow({ before, after, current }: { before: BotSchedule | null; after: BotSchedule | null; current?: ScheduleView }) {
  const schedule = after ?? before!
  const once = schedule.every_minutes === undefined
  const icon = once ? AlarmClock : Repeat
  const tag = !before ? (once ? '新提醒' : '新定时任务') : !after ? '已删除' : before.enabled !== after.enabled ? (after.enabled ? '已恢复' : '已暂停') : '已更新'
  if (!after) return <Row icon={icon} label={schedule.name} tag={tag} removed />
  const frequency = scheduleFrequency(after), previous = before ? scheduleFrequency(before) : frequency
  const next = once ? after.first_run_at_ms : current?.enabled ? current.next_run_at_ms : null
  return <Row icon={icon} label={after.name} tag={tag}>
    {before && before.name !== after.name ? <BeforeAfter className="text-meta" before={before.name} after={after.name} /> : null}
    {previous !== frequency ? <BeforeAfter className="text-meta" before={previous} after={frequency} /> : null}
    <p className="flex flex-wrap items-center gap-x-2 text-meta text-muted-foreground tabular-nums">
      {previous === frequency ? <span>{frequency}</span> : null}
      {!after.enabled ? <span className="inline-flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />已暂停</span>
        : next !== null ? <span>{once ? '' : '下次 '}{formatWhen(next)}</span> : null}
    </p>
    <Details label="要做的事"><p className="whitespace-pre-wrap text-foreground [word-break:normal] [overflow-wrap:anywhere]">{after.prompt}</p></Details>
  </Row>
}

function MemoryRow({ before, after }: { before: string | null; after: string | null }) {
  if (!after) return <Row icon={NotebookPen} label={before} tag="已忘记" removed />
  if (!before) return <Row icon={NotebookPen} label={after} tag="新笔记" />
  return <Row icon={NotebookPen} label="笔记" tag="已更新"><BeforeAfter before={before} after={after} /></Row>
}
