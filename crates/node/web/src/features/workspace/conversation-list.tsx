import { Link } from "@tanstack/react-router"
import { Bot, Plus, Star, WifiOff, type LucideIcon } from "lucide-react"
import type { ReactNode } from "react"
import { BotAvatar, CountBadge, EmptyState, Skeleton } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { SecondDeviceHint } from "@/features/devices/second-device"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { byRecent, listTime } from "@vibeface/client/workspace/home.ts"
import { useInbox } from "./inbox"
import { terms } from "@/lib/glossary"
import { openQuestions } from "@vibeface/client/workspace/questions.ts"
import { ConnectionCapsule, useCreateBot } from "./workspace-shell"

/** The phone's home: 待处理 first, then every bot by its newest message. */
export function ConversationList() {
  const workspace = useWorkspace()
  const create = useCreateBot()
  const open = useInbox().filter((item) => !item.done)
  const questions = openQuestions(workspace.questions)
  const now = Date.now()
  const bots = byRecent(workspace.bots)
  let list: ReactNode
  if (bots.length) list = <ul className="grid gap-0.5">{bots.map((bot) => {
    const notes = workspace.notifications.filter((item) => item.conversation_id === bot.conversation_id)
    const unread = notes.length + questions.filter((item) => item.conversation_id === bot.conversation_id && !(item.question_id in workspace.answered)).length
    const last = bot.last_message
    const preview = notes.at(-1)?.text.trim() || (last ? `${last.role === "user" ? "你：" : ""}${last.text}` : "还没有消息")
    return <li key={bot.bot_id}>
      <Row to="/b/$botId" params={{ botId: bot.bot_id }} icon={Bot} avatar={<BotAvatar name={bot.name} primary={workspace.primary === bot.conversation_id} size="lg" className="size-11 rounded-xl" />} unread={unread} time={last ? listTime(last.created_at_ms, now) : undefined} preview={preview}
        name={bot.name} mark={workspace.primary === bot.conversation_id ? <Star aria-label="主 Bot" className="size-3.5 shrink-0 fill-current text-muted-foreground" /> : null} />
    </li>
  })}</ul>
  else if (!workspace.joined) list = <EmptyState icon={Bot} title="还没有 Bot" />
  else if (!workspace.connectionOnline) list = <EmptyState icon={WifiOff} title="正在重连" />
  else if (!workspace.writable) list = <div className="grid gap-2" aria-hidden="true">{[0, 1, 2].map((item) => <Skeleton key={item} className="h-16 rounded-xl" />)}</div>
  else list = <EmptyState icon={Bot} title="还没有 Bot" action={<Button type="button" onClick={create}><Plus aria-hidden="true" />新 Bot</Button>} />
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-2 max-md:-mx-3 max-md:w-auto">
    <Row to="/inbox" icon={terms.inbox.icon} name={terms.inbox.label} unread={open.length} preview={open[0]?.title ?? "没有待处理"} />
    <div className="flex items-center justify-between px-3">
      <h2 className="text-meta font-medium text-muted-foreground">Bot</h2>
      <Button type="button" variant="ghost" size="icon" className="-me-2.5 max-md:-me-3.5 max-md:size-11" aria-label="新 Bot" title="新 Bot" disabled={!workspace.writable && !workspace.pendingBot} onClick={create}><Plus aria-hidden="true" /></Button>
    </div>
    {list}
    <footer className="mt-6 grid gap-1 border-t px-1 pt-2 md:hidden"><SecondDeviceHint /><ConnectionCapsule side="bottom" /></footer>
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
      <span className="flex min-w-0 items-center gap-2">
        <span className="min-w-0 flex-1 truncate text-meta text-muted-foreground">{preview}</span>
        {unread ? <CountBadge count={unread} label={`${unread} 条未读`} /> : null}
      </span>
    </span>
  </Link>
}
