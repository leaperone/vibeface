import type { TranscriptMessage } from "../chat/types.ts"
import type { BotChangeRecord, BotSchedule, BotSettings } from "./types.ts"

const weekdays = "日一二三四五六"
const pad = (value: number) => String(value).padStart(2, "0")
const clock = (date: Date) => `${pad(date.getHours())}:${pad(date.getMinutes())}`

/** "一次", "每天 09:00", "每周一 09:00", "每 2 小时", "每 30 分钟"; times in the browser's time zone. */
export function scheduleFrequency(schedule: Pick<BotSchedule, "first_run_at_ms" | "every_minutes">): string {
  const first = new Date(schedule.first_run_at_ms), every = schedule.every_minutes
  if (every === undefined) return "一次"
  if (every === 10080) return `每周${weekdays[first.getDay()]} ${clock(first)}`
  if (every === 1440) return `每天 ${clock(first)}`
  if (every % 1440 === 0) return `每 ${every / 1440} 天 ${clock(first)}`
  if (every % 60 === 0) return `每 ${every / 60} 小时`
  return `每 ${every} 分钟`
}

/** The prompt when it says more than the name; a reminder's name is the first line of its note. */
export function scheduleDetail(schedule: Pick<BotSchedule, "name" | "prompt">): string | null {
  return schedule.prompt.trim() === schedule.name.trim() ? null : schedule.prompt
}

/** "今天 09:00", "明天 09:00", "10月22日 09:00"; another year adds the year. */
export function formatWhen(ms: number, now = Date.now()): string {
  const date = new Date(ms), today = new Date(now)
  const day = (value: Date) => new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime()
  const days = Math.round((day(date) - day(today)) / 86_400_000)
  if (days === 0) return `今天 ${clock(date)}`
  if (days === 1) return `明天 ${clock(date)}`
  const year = date.getFullYear() === today.getFullYear() ? "" : `${date.getFullYear()}年`
  return `${year}${date.getMonth() + 1}月${date.getDate()}日 ${clock(date)}`
}

export type DiffLine = { kind: "removed" | "added"; text: string }

/** Lines removed and added between two texts, in reading order; unchanged and blank lines are left out. */
export function lineDiff(before: string, after: string): DiffLine[] {
  let a = before.split("\n"), b = after.split("\n")
  let head = 0, tail = 0
  while (head < a.length && head < b.length && a[head] === b[head]) head += 1
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail += 1
  a = a.slice(head, a.length - tail); b = b.slice(head, b.length - tail)
  const keep = (lines: DiffLine[]) => lines.filter((line) => line.text.trim())
  // ponytail: a huge rewrite reads as all-removed then all-added instead of a quadratic table.
  if (a.length * b.length > 1_000_000) return keep([...a.map((text) => ({ kind: "removed" as const, text })), ...b.map((text) => ({ kind: "added" as const, text }))])
  // Longest common subsequence table, filled from the end.
  const width = b.length + 1
  const common = new Uint16Array((a.length + 1) * width)
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      common[i * width + j] = a[i] === b[j] ? common[(i + 1) * width + j + 1] + 1 : Math.max(common[(i + 1) * width + j], common[i * width + j + 1])
    }
  }
  const lines: DiffLine[] = []
  let i = 0, j = 0
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) { i += 1; j += 1 }
    else if (j >= b.length || i < a.length && common[(i + 1) * width + j] >= common[i * width + j + 1]) lines.push({ kind: "removed", text: a[i++] })
    else lines.push({ kind: "added", text: b[j++] })
  }
  return keep(lines)
}

/** `schedule:<schedule_id>:<slot_ms>` marks a reply a schedule produced. */
export function scheduleTrigger(inReplyTo: string | undefined): string | null {
  if (!inReplyTo?.startsWith("schedule:")) return null
  const rest = inReplyTo.slice("schedule:".length), end = rest.lastIndexOf(":")
  return end > 0 ? rest.slice(0, end) : null
}

/** Current name, else the last name a change gave it (a removed or past schedule), else a generic label. */
export function scheduleName(scheduleId: string, bot: BotSettings | undefined, changes: readonly BotChangeRecord[] = []): string {
  const current = bot?.schedules.find((item) => item.schedule_id === scheduleId)
  if (current) return current.name
  for (const record of [...changes].reverse()) {
    const change = record.change
    if (change.kind === "schedule" && change.schedule_id === scheduleId) return (change.after ?? change.before)?.name ?? "定时任务"
  }
  return "定时任务"
}

/**
 * Where each turn's changes show: after the last reply to the person's message, else after that message.
 * Changes whose message is not on screen come back as `rest`, oldest first.
 */
export function placeChanges(messages: readonly Pick<TranscriptMessage, "message_id" | "role" | "in_reply_to">[], changes: readonly BotChangeRecord[]) {
  const anchor = new Map<string, string>()
  for (const message of messages) {
    if (message.role === "user") anchor.set(message.message_id, message.message_id)
    else if (message.in_reply_to && anchor.has(message.in_reply_to)) anchor.set(message.in_reply_to, message.message_id)
  }
  const turns = new Map<string, BotChangeRecord[]>()
  for (const record of changes) {
    const cause = record.cause?.message_id ?? ""
    turns.set(cause, [...turns.get(cause) ?? [], record])
  }
  const after = new Map<string, BotChangeRecord[]>()
  const rest: BotChangeRecord[][] = []
  for (const [cause, items] of turns) {
    const at = anchor.get(cause)
    if (at) after.set(at, items)
    else rest.push(items)
  }
  return { after, rest }
}
