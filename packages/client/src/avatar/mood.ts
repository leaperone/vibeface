import type { Mood } from "./face.ts"
import type { Busy } from "../workspace/types.ts"

/** An unanswered question outranks work in progress, which outranks the bot's device being offline. */
export function moodOf({ busy, waiting, offline }: { busy: Busy; waiting: boolean; offline: boolean }): Mood {
  if (waiting) return "waiting"
  if (busy) return busy
  return offline ? "asleep" : "idle"
}

/** Read out after the bot's name; idle says nothing. */
export const moodLabel: Record<Mood, string> = { idle: "", thinking: "思考中", replying: "回复中", waiting: "等你决定", asleep: "离线" }
