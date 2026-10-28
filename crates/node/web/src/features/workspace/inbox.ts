import { Copy, KeyRound, MessageCircleQuestionMark, MonitorSmartphone, ShieldAlert, Sparkles, MessageSquareWarning, type LucideIcon } from "lucide-react"
import { terms } from "@/lib/glossary"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { inboxItems, type InboxItem } from "@vibeface/client/workspace/inbox.ts"

/** The icon each item shows and where its row leads; cards act on the rest in place, in 待处理. */
function presented(item: InboxItem, chat: string): { icon: LucideIcon; to: string } {
  switch (item.kind) {
    case "recovery": return { icon: ShieldAlert, to: "/inbox" }
    case "rotate": return { icon: KeyRound, to: "/inbox" }
    case "write": return { icon: item.write === "read_only" ? terms.readOnly.icon : Copy, to: "/inbox" }
    case "question": return { icon: MessageCircleQuestionMark, to: item.question.bot ? `/b/${item.question.bot.bot_id}` : "/inbox" }
    case "confirm": return { icon: terms.confirm.icon, to: "/inbox" }
    case "undelivered": return { icon: terms.undelivered.icon, to: chat }
    case "join": return { icon: MonitorSmartphone, to: "/devices" }
    case "attention": return { icon: MessageSquareWarning, to: `/b/${item.bot.bot_id}` }
    case "setup": return { icon: Sparkles, to: item.device ? `/devices/${item.device}` : "/devices" }
  }
}

/** Everything waiting on the person, as the phone lists it too. */
export function useInbox() {
  const workspace = useWorkspace()
  const device = useDevice()
  const bot = workspace.selectedBot
  const chat = bot ? `/b/${bot.bot_id}` : "/"
  return inboxItems({ device, workspace }).map((item) => ({ ...item, ...presented(item, chat) }))
}
