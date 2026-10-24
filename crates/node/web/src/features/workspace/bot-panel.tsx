"use client"

import { useRef, useState } from "react"
import { useNavigate } from "@tanstack/react-router"
import { AlarmClock, BookOpen, Check, CirclePause, Cpu, Inbox, Laptop, NotebookPen, Pause, Play, Repeat, Server, Star, Trash2, WifiOff } from "lucide-react"
import { Button } from "@/components/ui/button"
import { BotAvatar, SettingsGroup, SettingsRow } from "@/components/app/kit"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { providerNames, type ProviderId } from "@vibeface/client/lib/node.ts"
import { formatIn, formatWhen, scheduleDetail, scheduleFrequency, sortSchedules } from "@vibeface/client/workspace/bot-settings.ts"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { providerIcons } from "@/features/devices/ai-settings"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { ActionButton } from "./present"
import type { ScheduleView } from "@vibeface/client/workspace/types.ts"

/** A deleted schedule keeps its place for a moment, so a second click does not land on the next row. */
type Gap = { conversation_id: string; schedule: ScheduleView; index: number }
const GAP_MS = 4000

/** Header button for the chat route; opens a view of the bot where schedules can be paused, resumed or deleted and the bot itself deleted (docs/design/bot-config.md section 5). */
export function BotPanelButton() {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const [open, setOpen] = useState(false)
  const trigger = useRef<HTMLButtonElement>(null)
  const [gaps, setGaps] = useState<Gap[]>([])
  const [confirming, setConfirming] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)
  const navigate = useNavigate()
  const bot = workspace.selectedBot
  const view = workspace.data?.bot_settings
  const owner = workspace.data?.conversation?.owner
  const ownerNode = workspace.nodes.find((node) => node.device_id === owner)
  if (!bot) return null
  const rows = sortSchedules(view?.schedules ?? []).map((schedule) => ({ schedule, deleted: false }))
  for (const gap of [...gaps].sort((a, b) => a.index - b.index)) {
    if (gap.conversation_id !== bot.conversation_id || rows.some((row) => row.schedule.schedule_id === gap.schedule.schedule_id)) continue
    rows.splice(Math.min(gap.index, rows.length), 0, { schedule: gap.schedule, deleted: true })
  }
  const remove = async (schedule: ScheduleView, index: number) => {
    const gap = { conversation_id: bot.conversation_id, schedule, index }
    setGaps((previous) => [...previous, gap])
    const drop = () => setGaps((previous) => previous.filter((item) => item !== gap))
    try { await workspace.controlSchedule(schedule.schedule_id, "delete") } catch (caught) { drop(); throw caught }
    setConfirming(null)
    window.setTimeout(drop, GAP_MS)
  }
  const models = Object.entries(bot.models) as [ProviderId, string][]
  const empty = view && !bot.instructions.trim() && !rows.length && !view.memory.length && !view.skills.length
  const deviceName = owner ? nodeName(owner, { nodes: workspace.nodes, directory }) : ""
  return <>
    <Button ref={trigger} type="button" variant="ghost" size="icon" className="-ms-1 shrink-0 rounded-lg hover:bg-transparent hover:brightness-95 dark:hover:brightness-110" aria-label="Bot 信息" title="Bot 信息" disabled={!view} onClick={() => setOpen(true)}>
      <BotAvatar id={bot.bot_id} name={bot.name} />
    </Button>
    <Sheet open={open && Boolean(view)} onOpenChange={(next) => { setOpen(next); setConfirming(null); setDeleting(false) }}>
      <SheetContent className="!w-full gap-0 overflow-y-auto bg-sidebar sm:!max-w-md" onCloseAutoFocus={(event) => { event.preventDefault(); trigger.current?.focus() }}>
        <SheetHeader className="flex-row items-center gap-3 px-4 pt-5 pb-6 pr-14">
          <BotAvatar id={bot.bot_id} name={bot.name} size="lg" />
          <span className="grid min-w-0 gap-0.5">
            <SheetTitle className="text-title font-semibold break-words">{bot.name}</SheetTitle>
            <SheetDescription className="text-meta">在聊天里说一声就能改</SheetDescription>
          </span>
        </SheetHeader>
        {view ? <div className="grid gap-6 px-4 pb-[max(1.5rem,env(safe-area-inset-bottom))]">
          <SettingsGroup>
            {owner ? <SettingsRow icon={owner === ready?.entry.device_id ? Laptop : Server} label={<span title={deviceName}>{deviceName}</span>}>
              {ownerNode && !ownerNode.online ? <span className="inline-flex items-center gap-1 text-warning"><WifiOff aria-hidden="true" className="size-4" />离线</span> : <span className="sr-only">运行在这台设备</span>}
            </SettingsRow> : null}
            {models.length ? models.map(([provider, model]) => <SettingsRow key={provider} icon={providerIcons[provider]} label={providerNames[provider]}>
              <span className="max-w-48 truncate font-mono" title={model}>{model}</span>
            </SettingsRow>) : <SettingsRow icon={Cpu} label="模型"><span>设备默认</span></SettingsRow>}
            <SettingsRow icon={Star} label="主 Bot">
              {workspace.primary === bot.conversation_id ? <span className="inline-flex items-center gap-1 text-foreground"><Check aria-hidden="true" className="size-4 text-brand" />是</span>
                : <ActionButton title={primaryHint} disabled={!workspace.writable} action={() => workspace.makePrimary(bot.conversation_id)}>设为主 Bot</ActionButton>}
            </SettingsRow>
          </SettingsGroup>
          {empty ? <p className="flex items-center justify-center gap-2 py-2 text-meta text-muted-foreground"><Inbox aria-hidden="true" className="size-4 shrink-0" />还没有说明、定时任务和笔记</p> : null}
          {bot.instructions.trim() ? <SettingsGroup title="说明">
            <p className="px-4 py-3 whitespace-pre-wrap [overflow-wrap:anywhere]">{bot.instructions}</p>
          </SettingsGroup> : null}
          {rows.length ? <SettingsGroup title={`定时任务 · ${view.schedules.length}`}>
            {rows.map(({ schedule, deleted }, index) => {
              const once = schedule.every_minutes === undefined
              const detail = scheduleDetail(schedule)
              const name = detail ? schedule.name : schedule.prompt
              if (deleted) return <SettingsRow key={schedule.schedule_id} icon={Trash2} className="text-muted-foreground" label={<span role="status"><span className="line-through">{schedule.name}</span> 已删除</span>} />
              return <SettingsRow key={schedule.schedule_id} icon={once ? AlarmClock : Repeat}
                label={<span title={name}>{name}</span>}
                description={<span className="flex flex-wrap items-center gap-x-1.5 tabular-nums">
                  {!schedule.enabled ? <span className="inline-flex items-center gap-1"><CirclePause aria-hidden="true" className="size-3.5" />已暂停</span>
                    : schedule.next_run_at_ms !== null ? <time dateTime={new Date(schedule.next_run_at_ms).toISOString()} title={formatWhen(schedule.next_run_at_ms)}>{formatIn(schedule.next_run_at_ms)}</time> : null}
                  {!schedule.enabled || schedule.next_run_at_ms !== null ? <span aria-hidden="true">·</span> : null}<span>{scheduleFrequency(schedule)}</span>
                  {detail ? <span className="basis-full truncate" title={detail}>{detail}</span> : null}
                </span>}>
                {confirming === schedule.schedule_id ? <>
                  <Button type="button" variant="ghost" autoFocus onClick={() => setConfirming(null)}>取消</Button>
                  <ActionButton variant="destructive" disabled={!workspace.writable} action={() => remove(schedule, index)}>删除</ActionButton>
                </> : <span className="-me-2 flex">
                  <ActionButton variant="ghost" label={schedule.enabled ? "暂停" : "恢复"} disabled={!workspace.writable} action={() => workspace.controlSchedule(schedule.schedule_id, schedule.enabled ? "pause" : "resume")}>
                    {schedule.enabled ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}
                  </ActionButton>
                  <Button type="button" variant="ghost" size="icon" aria-label="删除" title="删除" disabled={!workspace.writable} onClick={() => setConfirming(schedule.schedule_id)}>
                    <Trash2 aria-hidden="true" />
                  </Button>
                </span>}
              </SettingsRow>
            })}
          </SettingsGroup> : null}
          {view.memory.length ? <SettingsGroup title={`笔记 · ${view.memory.length}`}>
            {view.memory.map((note) => <p key={note.memory_id} className="flex gap-3 px-4 py-3">
              <NotebookPen aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
              <span className="line-clamp-3 min-w-0 [overflow-wrap:anywhere]" title={note.text}>{note.text}</span>
            </p>)}
          </SettingsGroup> : null}
          {view.skills.length ? <SettingsGroup title={`${bot.skills === null ? "skill · 全部" : "skill"} · ${view.skills.length}`}>
            {view.skills.map((skill) => <SettingsRow key={skill.skill_id} icon={BookOpen} label={skill.name} description={skill.description || undefined} />)}
          </SettingsGroup> : null}
          <SettingsGroup>
            <SettingsRow icon={Trash2} tone="danger" label="删除 Bot" description={deleting ? "不再回复，定时任务停止，聊天记录保留" : undefined} onClick={deleting || !workspace.writable ? undefined : () => setDeleting(true)}>
              {deleting ? <>
                <Button type="button" variant="ghost" autoFocus onClick={() => setDeleting(false)}>取消</Button>
                <ActionButton variant="destructive" disabled={!workspace.writable} action={async () => { await workspace.deleteBot(bot.conversation_id); setOpen(false); void navigate({ to: "/" }) }}>删除</ActionButton>
              </> : null}
            </SettingsRow>
          </SettingsGroup>
        </div> : null}
      </SheetContent>
    </Sheet>
  </>
}

const primaryHint = "没有 Bot 负责的 Codex 会话，由主 Bot 分派"
