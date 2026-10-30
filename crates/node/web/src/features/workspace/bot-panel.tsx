"use client"

import { useRef, useState } from "react"
import { useNavigate } from "@tanstack/react-router"
import { AlarmClock, BookOpen, Check, ChevronDown, CirclePause, Cpu, FolderOpen, Laptop, MessageCircle, MessagesSquare, NotebookPen, Pause, Play, Repeat, Server, Trash2, WifiOff } from "lucide-react"
import { Button } from "@/components/ui/button"
import { EmptyState, SettingsGroup, SettingsRow, toast } from "@/components/app/kit"
import { BotFace, botMood } from "@/components/app/bot-face"
import { terms } from "@/lib/glossary"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { asProvider, providerNames, type ProviderId } from "@vibeface/client/lib/node.ts"
import { botCopy, folderLabels, formatIn, formatWhen, projectChoices, projectKey, projectPlace, scheduleDetail, scheduleFrequency, sortSchedules } from "@vibeface/client/workspace/bot-settings.ts"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { providerIcons } from "@/features/devices/ai-settings"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { ActionButton } from "./present"
import { AvatarPicker } from "./avatar-picker"
import { ContextPanel } from "./context-panel"
import { useAvatarPick } from "@vibeface/client/avatar/pick.ts"
import type { ScheduleView } from "@vibeface/client/workspace/types.ts"

/** A deleted schedule keeps its place for a moment, so a second click does not land on the next row. */
type Gap = { conversation_id: string; schedule: ScheduleView; index: number }
const GAP_MS = 4000

/** The chat route's title: the bot's avatar and name as one button that opens a view of the bot where its face can be picked, schedules paused, resumed or deleted and the bot itself deleted (docs/design/bot-config.md section 5). */
export function BotPanelButton() {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const [open, setOpen] = useState(false)
  // While the panel is open, rows keep their places: a paused schedule moves to the end only on the next open, not from under the pointer.
  const order = useRef<string[]>([])
  // 取消 removes itself; focus goes back to the control that asked, or it would fall into the sheet's frame.
  const refocus = useRef<string | null>(null)
  const returnFocus = (key: string) => (node: HTMLElement | null) => { if (node && refocus.current === key) { refocus.current = null; node.focus() } }
  const [gaps, setGaps] = useState<Gap[]>([])
  const [confirming, setConfirming] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [contextOpen, setContextOpen] = useState(false)
  // 去聊天里说 closes the panel onto the composer instead of the title button that opened it.
  const toComposer = useRef(false)
  const navigate = useNavigate()
  const bot = workspace.selectedBot
  const view = workspace.data?.bot_settings
  const owner = workspace.data?.conversation?.owner
  const ownerNode = workspace.nodes.find((node) => node.device_id === owner)
  // The pick on show belongs to one bot: switching bots drops it (useAvatarPick compares bot_id).
  const look = useAvatarPick(bot ?? { bot_id: "", avatar: null, avatar_version: null }, (version, avatar) => workspace.setBotAvatar(bot!.conversation_id, version, avatar),
    (caught) => toast(messageOf(caught, "换长相"), { tone: "danger" }))
  if (!bot) return null
  const shown = { bot_id: bot.bot_id, avatar: look.avatar }
  const rank = (id: string) => { const at = order.current.indexOf(id); return at < 0 ? order.current.length : at }
  const schedules = sortSchedules(view?.schedules ?? []).sort((a, b) => rank(a.schedule_id) - rank(b.schedule_id))
  order.current = schedules.map((schedule) => schedule.schedule_id)
  const rows = schedules.map((schedule) => ({ schedule, deleted: false }))
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
  // The provider the bot runs on now, with its model; without one known, every model the bot chose.
  const running = asProvider(ownerNode?.capabilities?.manager)
  const models = running ? [[running, bot.models[running] ?? ""]] as [ProviderId, string][] : Object.entries(bot.models) as [ProviderId, string][]
  const empty = view && !bot.instructions.trim() && !rows.length && !view.memory.length && !view.skills.length
  const deviceName = owner ? nodeName(owner, { nodes: workspace.nodes, directory }) : ""
  const mood = botMood(bot, workspace)
  const lookup = { nodes: workspace.nodes, directory }
  const choices = projectChoices(workspace.nodes, (node, index) => nodeName(node.device_id, lookup, node.local ? "这台电脑" : `设备 ${index + 1}`))
  const projectNow = bot.project ? projectKey(bot.project) : ""
  const projectLabel = bot.project ? projectPlace(bot.project, owner, (id) => nodeName(id, lookup)) : "不选"
  const moveProject = (key: string) => void workspace.setBotProject(bot.conversation_id, choices.find((item) => item.key === key)?.project ?? null)
    .catch((caught: unknown) => toast(messageOf(caught, "换文件夹"), { tone: "danger" }))
  return <>
    <button type="button" className="-ms-1.5 flex min-h-11 min-w-0 items-center gap-2 rounded-lg ps-1 pe-1.5 outline-none transition-colors focus-visible:ring-3 focus-visible:ring-ring/50 enabled:hover:bg-muted enabled:active:bg-muted md:min-h-10 dark:enabled:hover:bg-muted/50" aria-haspopup="dialog" title={bot.name} disabled={!view} onClick={() => { order.current = []; setOpen(true) }}>
      <BotFace bot={shown} mood={mood} size={32} />
      <span className="min-w-0 truncate">{bot.name}</span>
      <ChevronDown aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
    </button>
    <Sheet open={open && Boolean(view)} onOpenChange={(next) => { setOpen(next); setConfirming(null); setDeleting(false); setContextOpen(false) }}>
      <SheetContent className="!w-full max-sm:!border-l-0 gap-0 overflow-y-auto bg-sidebar sm:!max-w-md" onCloseAutoFocus={(event) => {
        if (!toComposer.current) return
        toComposer.current = false
        const composer = document.getElementById("message-text")
        if (composer) { event.preventDefault(); composer.focus() }
      }}>
        <SheetHeader className="flex-row items-center gap-3 px-4 pt-4 pb-6 pr-14">
          <BotFace bot={shown} mood={mood} size={48} />
          <span className="grid min-w-0 gap-0.5">
            <SheetTitle className="text-title font-semibold break-words">{bot.name}</SheetTitle>
            <SheetDescription className="text-meta">在聊天里说一声就能改</SheetDescription>
          </span>
        </SheetHeader>
        {view ? contextOpen && workspace.data ? <ContextPanel bot={bot} data={workspace.data} nodes={workspace.nodes} onBack={() => { refocus.current = "context"; setContextOpen(false) }} /> : <div className="grid gap-6 px-4 pb-[max(1.5rem,env(safe-area-inset-bottom))]">
          <SettingsGroup>
            <AvatarPicker botId={bot.bot_id} avatar={look.avatar} face={look.face} disabled={!workspace.writable} busy={look.pending} pick={look.pick} />
          </SettingsGroup>
          <SettingsGroup>
            {owner ? <SettingsRow icon={owner === ready?.entry.device_id ? Laptop : Server} label="运行设备">
              {ownerNode && !ownerNode.online ? <span className="inline-flex items-center gap-1 text-warning"><WifiOff aria-hidden="true" className="size-4" />离线</span> : null}
              <span className="max-w-48 truncate" title={deviceName}>{deviceName}</span>
            </SettingsRow> : null}
            {bot.project || choices.length ? <SettingsRow icon={FolderOpen} label={folderLabels.project}>
              {workspace.writable && choices.length ? <NativeSelect size="sm" aria-label={folderLabels.project} className="max-w-48" value={projectNow} onChange={(event) => moveProject(event.currentTarget.value)}>
                <NativeSelectOption value="">不选</NativeSelectOption>
                {bot.project && !choices.some((item) => item.key === projectNow) ? <NativeSelectOption value={projectNow}>{projectLabel}</NativeSelectOption> : null}
                {choices.map((item) => <NativeSelectOption key={item.key} value={item.key}>{item.label}</NativeSelectOption>)}
              </NativeSelect> : <span className="max-w-48 truncate" title={bot.project?.cwd}>{projectLabel}</span>}
            </SettingsRow> : null}
            {models.length ? models.map(([provider, model]) => <SettingsRow key={provider} icon={providerIcons[provider]} label={providerNames[provider]}>
              {model ? <span className="max-w-48 truncate font-mono" title={model}>{model}</span> : <span>设备默认</span>}
            </SettingsRow>) : <SettingsRow icon={Cpu} label="模型"><span>设备默认</span></SettingsRow>}
            <SettingsRow icon={terms.primaryBot.icon} label={terms.primaryBot.label} description={botCopy.primaryHint}>
              {workspace.primary === bot.conversation_id ? <span className="inline-flex items-center gap-1 text-foreground"><Check aria-hidden="true" className="size-4 text-brand" />是</span>
                : <ActionButton disabled={!workspace.writable} action={() => workspace.makePrimary(bot.conversation_id)}>设为主 Bot</ActionButton>}
            </SettingsRow>
          </SettingsGroup>
          <SettingsGroup>
            <SettingsRow ref={returnFocus("context")} icon={MessagesSquare} label="上下文和会话" description="查看聊天、工作和会话摘要" onClick={() => setContextOpen(true)}>
              <span aria-hidden="true">查看</span>
            </SettingsRow>
          </SettingsGroup>
          {empty ? <EmptyState icon={NotebookPen} className="py-4" title={botCopy.empty}
            action={<Button type="button" size="sm" variant="outline" onClick={() => { toComposer.current = true; setOpen(false) }}><MessageCircle aria-hidden="true" />去聊天里说</Button>} /> : null}
          {bot.instructions.trim() ? <SettingsGroup title="说明">
            <p className="px-4 py-3 whitespace-pre-wrap [overflow-wrap:anywhere]">{bot.instructions}</p>
          </SettingsGroup> : null}
          {rows.length ? <SettingsGroup title={`定时任务 · ${view.schedules.length}`}>
            {rows.map(({ schedule, deleted }, index) => {
              const once = schedule.every_minutes === undefined
              const detail = scheduleDetail(schedule)
              const name = detail ? schedule.name : schedule.prompt
              if (deleted) return <SettingsRow key={schedule.schedule_id} icon={Trash2} className="text-muted-foreground" label={<span role="status"><span className="line-through">{schedule.name}</span> 已删除</span>} />
              return <SettingsRow key={schedule.schedule_id} icon={once ? AlarmClock : Repeat} className={schedule.enabled ? undefined : "text-muted-foreground"}
                label={<span title={name}>{name}</span>}
                description={<span className="flex flex-wrap items-center gap-x-1.5 tabular-nums">
                  {!schedule.enabled ? <span className="inline-flex items-center gap-1"><CirclePause aria-hidden="true" className="size-3.5" />已暂停</span>
                    : schedule.next_run_at_ms !== null ? <time dateTime={new Date(schedule.next_run_at_ms).toISOString()} title={formatWhen(schedule.next_run_at_ms)}>{formatIn(schedule.next_run_at_ms)}</time> : null}
                  {!schedule.enabled || schedule.next_run_at_ms !== null ? <span aria-hidden="true">·</span> : null}<span>{scheduleFrequency(schedule)}</span>
                  {detail ? <span className="basis-full truncate" title={detail}>{detail}</span> : null}
                </span>}>
                {confirming === schedule.schedule_id ? <>
                  <Button type="button" variant="ghost" autoFocus onClick={() => { refocus.current = schedule.schedule_id; setConfirming(null) }}>取消</Button>
                  <ActionButton variant="destructive" disabled={!workspace.writable} action={() => remove(schedule, index)}>删除</ActionButton>
                </> : <span className="-me-2 flex">
                  <ActionButton variant="ghost" label={schedule.enabled ? "暂停" : "恢复"} disabled={!workspace.writable} action={() => workspace.controlSchedule(schedule.schedule_id, schedule.enabled ? "pause" : "resume")}>
                    {schedule.enabled ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}
                  </ActionButton>
                  <Button ref={returnFocus(schedule.schedule_id)} type="button" variant="ghost" size="icon" aria-label="删除" title="删除" disabled={!workspace.writable} onClick={() => setConfirming(schedule.schedule_id)}>
                    <Trash2 aria-hidden="true" />
                  </Button>
                </span>}
              </SettingsRow>
            })}
          </SettingsGroup> : null}
          {view.memory.length ? <SettingsGroup title={`笔记 · ${view.memory.length}`}>
            {view.memory.map((note) => <p key={note.memory_id} className="line-clamp-3 px-4 py-3 [overflow-wrap:anywhere]" title={note.text}>{note.text}</p>)}
          </SettingsGroup> : null}
          {view.skills.length ? <SettingsGroup title={botCopy.skills(bot.skills, view.skills.length)}>
            {view.skills.map((skill) => <SettingsRow key={skill.skill_id} icon={BookOpen} label={<span title={skill.name}>{skill.name}</span>} description={skill.description || undefined} />)}
          </SettingsGroup> : null}
          <SettingsGroup>
            <SettingsRow ref={returnFocus("bot")} icon={Trash2} tone="danger" label="删除 Bot" description={deleting ? "不再回复，定时任务停止，聊天记录一并移除" : undefined} onClick={deleting || !workspace.writable ? undefined : () => setDeleting(true)}>
              {deleting ? <>
                <Button type="button" variant="ghost" autoFocus onClick={() => { refocus.current = "bot"; setDeleting(false) }}>取消</Button>
                <ActionButton variant="destructive" disabled={!workspace.writable} action={async () => { await workspace.deleteBot(bot.conversation_id); toast(`已删除「${bot.name}」`, { tone: "success" }); setOpen(false); void navigate({ to: "/" }) }}>删除</ActionButton>
              </> : null}
            </SettingsRow>
          </SettingsGroup>
        </div> : null}
      </SheetContent>
    </Sheet>
  </>
}

