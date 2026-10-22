"use client"

import { useRef, useState, type ReactNode } from "react"
import { AlarmClock, BookOpen, Bot, CalendarClock, CirclePause, NotebookPen, Pause, Play, Repeat, ScrollText, Star, Trash2, type LucideIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { formatWhen, scheduleDetail, scheduleFrequency } from "./bot-settings"
import { useWorkspace } from "./context"
import { ActionButton } from "./present"
import type { ScheduleView } from "./types"

/** A deleted schedule keeps its place for a moment, so a second click does not land on the next row. */
type Gap = { conversation_id: string; schedule: ScheduleView; index: number }
const GAP_MS = 4000

/** Header button for the chat route; opens a view of the bot where only schedules can be paused, resumed or deleted (docs/design/bot-config.md section 5). */
export function BotPanelButton() {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const trigger = useRef<HTMLButtonElement>(null)
  const [gaps, setGaps] = useState<Gap[]>([])
  const bot = workspace.selectedBot
  const view = workspace.data?.bot_settings
  if (!bot) return null
  const rows = (view?.schedules ?? []).map((schedule) => ({ schedule, deleted: false }))
  for (const gap of [...gaps].sort((a, b) => a.index - b.index)) {
    if (gap.conversation_id !== bot.conversation_id || rows.some((row) => row.schedule.schedule_id === gap.schedule.schedule_id)) continue
    rows.splice(Math.min(gap.index, rows.length), 0, { schedule: gap.schedule, deleted: true })
  }
  const remove = async (schedule: ScheduleView, index: number) => {
    const gap = { conversation_id: bot.conversation_id, schedule, index }
    setGaps((previous) => [...previous, gap])
    const drop = () => setGaps((previous) => previous.filter((item) => item !== gap))
    try { await workspace.controlSchedule(schedule.schedule_id, "delete") } catch (caught) { drop(); throw caught }
    window.setTimeout(drop, GAP_MS)
  }
  return <>
    <Button ref={trigger} type="button" variant="ghost" size="icon" className="shrink-0 rounded-full bg-muted text-muted-foreground hover:bg-accent" aria-label="Bot 信息" title="Bot 信息" disabled={!view} onClick={() => setOpen(true)}>
      <Bot aria-hidden="true" />
    </Button>
    <Sheet open={open && Boolean(view)} onOpenChange={setOpen}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-md" onCloseAutoFocus={(event) => { event.preventDefault(); trigger.current?.focus() }}>
        <SheetHeader className="pr-14">
          <SheetTitle className="break-words">{bot.name}</SheetTitle>
          <SheetDescription>在聊天里说一声就能改</SheetDescription>
          {workspace.primary === bot.conversation_id
            ? <p className="flex items-center gap-1.5 text-meta text-muted-foreground" title={primaryHint}><Star aria-hidden="true" className="size-3.5 fill-current" />主 Bot</p>
            : <ActionButton className="justify-self-start" title={primaryHint} disabled={!workspace.writable} action={() => workspace.makePrimary(bot.conversation_id)}><Star aria-hidden="true" />设为主 Bot</ActionButton>}
        </SheetHeader>
        {view ? <div className="grid gap-8 px-4 pb-[max(1.5rem,env(safe-area-inset-bottom))]">
          <Section icon={ScrollText} title="说明">
            {bot.instructions.trim()
              ? <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{bot.instructions}</p>
              : <Empty>还没有说明</Empty>}
          </Section>
          <Section icon={CalendarClock} title="定时任务" count={view.schedules.length}>
            {rows.length ? <ul className="grid gap-4">{rows.map(({ schedule, deleted }, index) => {
              const Kind = schedule.every_minutes === undefined ? AlarmClock : Repeat
              const detail = scheduleDetail(schedule)
              if (deleted) return <li key={schedule.schedule_id} className="flex min-h-9 items-start gap-3 text-muted-foreground">
                <Trash2 aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
                <p role="status" className="min-w-0 flex-1 [overflow-wrap:anywhere]"><span className="line-through">{schedule.name}</span> 已删除</p>
              </li>
              return <li key={schedule.schedule_id} className="flex items-start gap-3">
                <Kind aria-label={schedule.every_minutes === undefined ? "一次" : "重复"} className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                <div className="grid min-w-0 flex-1 gap-0.5">
                  <p className={`font-medium [overflow-wrap:anywhere] ${detail ? "" : "line-clamp-3"}`} title={detail ? undefined : schedule.prompt}>{detail ? schedule.name : schedule.prompt}</p>
                  <p className="flex flex-wrap items-center gap-x-2 text-meta text-muted-foreground tabular-nums">
                    {!schedule.enabled ? <span className="inline-flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />已暂停</span>
                      : schedule.next_run_at_ms !== null ? <span>{formatWhen(schedule.next_run_at_ms)}</span> : null}
                    <span>{scheduleFrequency(schedule)}</span>
                  </p>
                  {detail ? <p className="line-clamp-2 text-meta text-muted-foreground [overflow-wrap:anywhere]" title={detail}>{detail}</p> : null}
                </div>
                <div className="flex shrink-0 gap-1">
                  <ActionButton variant="ghost" label={schedule.enabled ? "暂停" : "恢复"} disabled={!workspace.writable} action={() => workspace.controlSchedule(schedule.schedule_id, schedule.enabled ? "pause" : "resume")}>
                    {schedule.enabled ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}
                  </ActionButton>
                  <ActionButton variant="ghost" label="删除" disabled={!workspace.writable} action={() => remove(schedule, index)}>
                    <Trash2 aria-hidden="true" />
                  </ActionButton>
                </div>
              </li>
            })}</ul> : <Empty>还没有定时任务</Empty>}
          </Section>
          <Section icon={NotebookPen} title="笔记" count={view.memory.length}>
            {view.memory.length ? <ul className="grid gap-2">{view.memory.map((note) => <li key={note.memory_id} className="line-clamp-2 text-meta [overflow-wrap:anywhere]" title={note.text}>{note.text}</li>)}</ul>
              : <Empty>还没有笔记</Empty>}
          </Section>
          <Section icon={BookOpen} title="共用 skill" count={view.skills.length}>
            {view.skills.length ? <ul className="grid gap-3">{view.skills.map((skill) => <li key={skill.skill_id} className="grid gap-0.5">
              <p className="font-medium [overflow-wrap:anywhere]">{skill.name}</p>
              {skill.description ? <p className="text-meta text-muted-foreground [overflow-wrap:anywhere]">{skill.description}</p> : null}
            </li>)}</ul> : <Empty>还没有 skill</Empty>}
          </Section>
        </div> : null}
      </SheetContent>
    </Sheet>
  </>
}

const primaryHint = "没有 Bot 负责的 Codex 会话，由主 Bot 分派"

function Section({ icon: Icon, title, count, children }: { icon: LucideIcon; title: string; count?: number; children: ReactNode }) {
  return <section className="grid gap-3">
    <h3 className="flex items-center gap-2 text-meta font-medium text-muted-foreground">
      <Icon aria-hidden="true" className="size-4 shrink-0" />{title}
      {count ? <span className="tabular-nums">{count}</span> : null}
    </h3>
    {children}
  </section>
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-meta text-muted-foreground">{children}</p>
}
