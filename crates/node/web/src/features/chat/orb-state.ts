import type { OrbState } from "thinking-orbs"

/** 九态都有生产入口，主界面一次只显示当前这一态。 */
export const thinkingOrbStates = [
  "connecting",
  "working",
  "searching",
  "solving",
  "listening",
  "weaving",
  "composing",
  "breathing",
  "shaping",
] as const satisfies readonly OrbState[]

export type ThinkingOrbState = (typeof thinkingOrbStates)[number]

export type OrbPlace =
  | "connection"
  | "wait"
  | "refresh"
  | "reconcile"
  | "idle-chat"
  | "idle-link"
  | "trace"
  | "reply"
  | "plan"

export function orbStateFor(place: OrbPlace): ThinkingOrbState {
  switch (place) {
    case "connection": return "connecting"
    case "wait": return "working"
    case "refresh": return "searching"
    case "reconcile": return "solving"
    case "idle-chat": return "listening"
    case "idle-link": return "breathing"
    case "trace": return "weaving"
    case "reply": return "composing"
    case "plan": return "shaping"
  }
}
