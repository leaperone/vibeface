import type { Link } from "../lib/link.ts"
import type { BotRecord } from "./types.ts"

type Home = { narrow: boolean; conversation?: string; bots: readonly BotRecord[]; current: string }

/**
 * The bot `/` opens, or null to show the conversation list. A notification's conversation always opens; a
 * phone otherwise lands on the list, and a wide screen, whose sidebar is the list, opens the last one used.
 */
export function homeBot({ narrow, conversation, bots, current }: Home): BotRecord | null {
  const linked = conversation ? bots.find((bot) => bot.conversation_id === conversation) : undefined
  if (linked) return linked
  if (narrow) return null
  return bots.find((bot) => bot.conversation_id === current) ?? bots[0] ?? null
}

/**
 * What a list with no bot rows shows: placeholders while the first read is on its way, an unreachable network once a
 * connect attempt has failed (so a stopped Node never looks like an endless load), else no bots yet.
 */
export function emptyHome({ synced, link }: { synced: boolean; link: Link }): "loading" | "unreachable" | "empty" {
  if (synced) return "empty"
  return link.kind === "offline" || (link.kind === "connecting" && link.attempt > 0) ? "unreachable" : "loading"
}

/** Bots by their newest message, newest first; ones without messages keep their order at the end. */
export function byRecent(bots: readonly BotRecord[]): BotRecord[] {
  return [...bots].sort((a, b) => (b.last_message?.created_at_ms ?? -1) - (a.last_message?.created_at_ms ?? -1))
}

/** Today shows the time, this year the date, older ones the year too. */
export function listTime(at: number, now: number): string {
  const date = new Date(at), today = new Date(now)
  if (date.toDateString() === today.toDateString()) return date.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false })
  if (date.getFullYear() === today.getFullYear()) return `${date.getMonth() + 1}月${date.getDate()}日`
  return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()}`
}

/** On a phone every page but the list goes back one level; null on the list itself. */
export function backTo(pathname: string, bot?: string): string | null {
  if (pathname === "/") return null
  // A device page opened from a chat goes back to that chat.
  if (bot && pathname.startsWith("/devices")) return `/b/${encodeURIComponent(bot)}`
  const work = /^(\/b\/[^/]+)\/work\//.exec(pathname)
  if (work) return work[1]
  if (/^\/devices\/[^/]+/.test(pathname)) return "/devices"
  if (pathname.startsWith("/settings/")) return "/settings"
  return "/"
}

/** `?bot=`: the chat a device page was opened from. */
export function botSearch(search: Record<string, unknown>): { bot?: string } {
  return typeof search.bot === "string" && search.bot ? { bot: search.bot } : {}
}
