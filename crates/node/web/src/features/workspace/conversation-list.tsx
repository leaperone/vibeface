import { Link } from "@tanstack/react-router"
import { Bot, Plus, WifiOff, type LucideIcon } from "lucide-react"
import type { ReactNode } from "react"
import { CountBadge, EmptyState, PrimaryMark, Skeleton } from "@/components/app/kit"
import { BotFace, botMood } from "@/components/app/bot-face"
import { moodLabel } from "@vibeface/client/avatar/mood.ts"
import { Button } from "@/components/ui/button"
import { AiOffHint } from "@/features/devices/ai-off-hint"
import { SecondDeviceHint } from "@/features/devices/second-device"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { byRecent, listTime } from "@vibeface/client/workspace/home.ts"
import { useInbox } from "./inbox"
import { waitingItems } from "@vibeface/client/workspace/inbox.ts"
import { terms } from "@/lib/glossary"
import { awaitsAnswer } from "@vibeface/client/workspace/questions.ts"
import { unreadCounts } from "@vibeface/client/chat/timeline.ts"
import { ConnectionCapsule, useCreateBot } from "./workspace-shell"

/** The phone's home: 待处理 first, then every bot by its newest message. */
export function ConversationList() {
  const workspace = useWorkspace()
  const create = useCreateBot()
  const open = waitingItems(useInbox())
  const notifications = workspace.notifications.filter((item) => !workspace.ignoredNotifications.has(item.notification_id))
  const unread = unreadCounts(notifications, workspace.questions.filter((item) => awaitsAnswer(item, workspace.answered)))
  const now = Date.now()
  const bots = byRecent(workspace.bots)
  let list: ReactNode
  if (bots.length) list = <ul className="grid gap-0.5">{bots.map((bot) => {
    const notes = notifications.filter((item) => item.conversation_id === bot.conversation_id)
    const last = bot.last_message, mood = botMood(bot, workspace)
    const preview = notes.at(-1)?.text.trim() || (last ? `${last.role === "user" ? "你：" : ""}${last.text}` : "还没有消息")
    return <li key={bot.bot_id}>
      <Row to="/b/$botId" params={{ botId: bot.bot_id }} icon={Bot} avatar={<BotFace bot={bot} mood={mood} size={44} />} unread={unread[bot.conversation_id] ?? 0} time={last ? listTime(last.created_at_ms, now) : undefined} preview={preview}
        name={bot.name} mark={<>{workspace.primary === bot.conversation_id ? <PrimaryMark /> : null}{moodLabel[mood] ? <span className="sr-only">，{moodLabel[mood]}</span> : null}</>} />
    </li>
  })}</ul>
  else if (!workspace.connectionOnline) list = <EmptyState icon={WifiOff} title="正在重连" />
  else if (!workspace.synced) list = <div className="grid gap-2" aria-hidden="true">{[0, 1, 2].map((item) => <Skeleton key={item} className="h-16 rounded-xl" />)}</div>
  else list = <EmptyState icon={Bot} title="还没有 Bot" action={workspace.writable ? <Button type="button" onClick={create}><Plus aria-hidden="true" />新 Bot</Button> : undefined} />
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-2 max-md:-mx-3 max-md:w-auto">
    <Row to="/inbox" icon={terms.inbox.icon} name={terms.inbox.label} unread={open.length} preview={open[0]?.title ?? ""} />
    <div className="flex items-center justify-between px-3">
      <h2 className="text-meta font-medium text-muted-foreground">Bot</h2>
      <Button type="button" variant="ghost" size="icon" className="-me-2.5 max-md:-me-3.5 max-md:size-11" aria-label="新 Bot" title="新 Bot" disabled={!workspace.writable && !workspace.pendingBot} onClick={create}><Plus aria-hidden="true" /></Button>
    </div>
    {list}
    <footer className="relative mt-6 grid gap-1 px-1 pt-2 before:absolute before:inset-x-3 before:top-0 before:border-t before:border-border md:hidden"><AiOffHint /><SecondDeviceHint /><ConnectionCapsule side="bottom" /></footer>
  </div>
}

type RowProps = { icon: LucideIcon; avatar?: ReactNode; name: string; mark?: ReactNode; preview: string; unread: number; time?: string } & ({ to: "/inbox"; params?: never } | { to: "/b/$botId"; params: { botId: string } })

function Row({ icon: Icon, avatar, name, mark, preview, unread, time, ...link }: RowProps) {
  return <Link {...link} className="flex min-h-16 items-center gap-3 interactive-row rounded-xl px-3 py-2.5">
    {avatar ?? <span className="grid size-11 shrink-0 place-items-center rounded-xl bg-muted text-muted-foreground ring-1 ring-border"><Icon aria-hidden="true" className="size-5" /></span>}
    <span className="grid min-w-0 flex-1 gap-0.5">
      <span className="flex min-w-0 items-center gap-2">
        <span className="flex min-w-0 flex-1 items-center gap-1.5"><span className="min-w-0 truncate font-medium">{name}</span>{mark}</span>
        {time ? <span className="shrink-0 text-meta text-muted-foreground tabular-nums">{time}</span> : null}
      </span>
      {preview || unread ? <span className="flex min-w-0 items-center gap-2">
        <span className="min-w-0 flex-1 truncate text-meta text-muted-foreground">{preview}</span>
        {unread ? <CountBadge count={unread} label={`${unread} 条未读`} /> : null}
      </span> : null}
    </span>
  </Link>
}
