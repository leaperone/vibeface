"use client"

import { useEffect, useId, useMemo, useRef, useState, useSyncExternalStore } from "react"
import { colors, faceOf, HALO, oklch, pose, thinkingDots, type Mood } from "@vibeface/client/avatar/face.ts"
import { ASLEEP, faceMotion, type Frame } from "@vibeface/client/avatar/motion.ts"
import { moodOf } from "@vibeface/client/avatar/mood.ts"
import type { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { questionOpen } from "@vibeface/client/workspace/questions.ts"
import type { BotRecord } from "@vibeface/client/workspace/types.ts"
import { cn } from "@/lib/utils"

/**
 * One clock for every face in view. It stops while the tab is hidden or the system asks for reduced motion; a face
 * scrolled out of view leaves the clock until it comes back.
 */
const ticks = new Set<(now: number) => void>()
let raf = 0
const reducedQuery = typeof window === "undefined" ? null : window.matchMedia("(prefers-reduced-motion: reduce)")
const running = () => ticks.size > 0 && !document.hidden && !reducedQuery?.matches
function loop(now: number) {
  for (const tick of ticks) tick(now)
  raf = running() ? requestAnimationFrame(loop) : 0
}
function kick() {
  if (!raf && running()) raf = requestAnimationFrame(loop)
}
if (typeof document !== "undefined") document.addEventListener("visibilitychange", kick)

const followReduced = (change: () => void) => {
  reducedQuery?.addEventListener("change", change)
  return () => reducedQuery?.removeEventListener("change", change)
}
const followDark = (change: () => void) => {
  const observer = new MutationObserver(change)
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] })
  return () => observer.disconnect()
}
export const useDark = () => useSyncExternalStore(followDark, () => document.documentElement.classList.contains("dark"), () => false)

/** A bot's face hue as a solid fill with legible ink on it, a soft edge for outlines, and a bright glow that stays that hue when faded, so its chat's accents read as that bot's. Same numbers as the phone app. */
export function botTint(hue: number, dark: boolean) {
  const glow = oklch(0.74, 0.14, hue)
  return dark
    ? { fill: oklch(0.78, 0.09, hue), ink: oklch(0.2, 0.03, hue), edge: oklch(0.42, 0.05, hue), glow }
    : { fill: oklch(0.45, 0.1, hue), ink: "#fff", edge: oklch(0.86, 0.04, hue), glow }
}

function apply(frame: Frame, parts: { g: SVGGElement; ring: SVGPathElement; body: SVGPathElement; grey: SVGPathElement; left: SVGPathElement; right: SVGPathElement; dots: SVGCircleElement[] }) {
  parts.g.setAttribute("transform", transformOf(frame))
  parts.ring.setAttribute("d", frame.ring)
  parts.ring.setAttribute("opacity", String(frame.ringOpacity))
  parts.body.setAttribute("d", frame.body)
  parts.grey.setAttribute("d", frame.body)
  parts.grey.setAttribute("opacity", String(frame.grey))
  parts.left.setAttribute("d", frame.left)
  parts.right.setAttribute("d", frame.right)
  frame.dots.forEach((opacity, i) => parts.dots[i].setAttribute("opacity", String(opacity)))
}

const transformOf = (frame: Frame) => `translate(${50 + frame.dx} ${50 + frame.dy}) rotate(${frame.rotate}) scale(${frame.scale}) translate(-50 -50)`

/** Who a face belongs to: its id gives what `avatar` leaves out. */
export type FaceBot = Pick<BotRecord, "bot_id" | "avatar">

/** A bot's face, picked by its id and avatar and moving with its mood. Hidden from screen readers: the name next to it says who, and lists say the mood. `halo`: a still glow in the bot's tint behind the face, 1.8 times its size. */
export function BotFace({ bot, mood, size, halo, className }: { bot: FaceBot; mood: Mood; size: number; halo?: string; className?: string }) {
  const dark = useDark()
  const reduced = useSyncExternalStore(followReduced, () => reducedQuery?.matches ?? false, () => false)
  const { bot_id: botId, avatar } = bot
  const look = avatar ? `${avatar.outline}:${avatar.eyes}:${avatar.hue}` : ""
  const face = useMemo(() => faceOf(botId, avatar), [botId, look])
  const palette = useMemo(() => ({ awake: colors(face, dark, 1), asleep: colors(face, dark, ASLEEP) }), [face, dark])
  const motion = useMemo(() => faceMotion(botId, mood, avatar), [botId, look])
  // The first paint draws the still pose; after that only the clock writes these attributes, so React never resets them.
  const [first] = useState(() => motion.frame(null))
  const dots = useMemo(() => thinkingDots(pose(face, "thinking", 0), 0), [face])
  const gradient = `face${useId().replace(/[^a-zA-Z0-9]/g, "")}`
  const root = useRef<SVGSVGElement>(null)

  useEffect(() => {
    const svg = root.current!
    const [g, ring, body, grey, left, right] = ["g", "[data-ring]", "[data-body]", "[data-grey]", "[data-left]", "[data-right]"].map((selector) => svg.querySelector(selector)!) as [SVGGElement, SVGPathElement, SVGPathElement, SVGPathElement, SVGPathElement, SVGPathElement]
    const parts = { g, ring, body, grey, left, right, dots: [...svg.querySelectorAll("circle")] }
    const draw = (now: number | null) => apply(motion.frame(now), parts)
    motion.follow(mood)
    if (reduced) { draw(null); return }
    draw(performance.now())
    const observer = new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting) { ticks.add(draw); kick() } else ticks.delete(draw)
    })
    observer.observe(svg)
    return () => { observer.disconnect(); ticks.delete(draw) }
  }, [motion, mood, reduced])

  const ink = mood === "asleep" ? palette.asleep.ink : palette.awake.ink
  return <svg ref={root} aria-hidden="true" viewBox="0 0 100 100" width={size} height={size} overflow="visible" className={cn("shrink-0 select-none", className)}>
    <defs>
      {(["awake", "asleep"] as const).map((key) => <radialGradient key={key} id={`${gradient}${key}`} cx="38%" cy="30%" r="75%">
        <stop offset={0} stopColor={palette[key].top} />
        <stop offset={0.55} stopColor={palette[key].base} />
        <stop offset={1} stopColor={palette[key].bottom} />
      </radialGradient>)}
      {halo ? <radialGradient id={`${gradient}halo`}>
        <stop offset={0.5} stopColor={halo} stopOpacity={dark ? 0.18 : 0.14} />
        <stop offset={1} stopColor={halo} stopOpacity={0} />
      </radialGradient> : null}
    </defs>
    {halo ? <rect x={-40} y={-40} width={180} height={180} fill={`url(#${gradient}halo)`} /> : null}
    <g transform={transformOf(first)}>
      <path data-ring="" d={first.ring} opacity={first.ringOpacity} fill="none" stroke={palette.awake.ring} strokeWidth={HALO.width} />
      <path data-body="" d={first.body} fill={`url(#${gradient}awake)`} />
      <path data-grey="" d={first.body} opacity={first.grey} fill={`url(#${gradient}asleep)`} />
      <path data-left="" d={first.left} fill={ink} />
      <path data-right="" d={first.right} fill={ink} />
    </g>
    {dots.map((dot, i) => <circle key={i} cx={dot.x} cy={dot.y} r={dot.r} opacity={first.dots[i]} fill={palette.awake.thought} />)}
  </svg>
}

/**
 * What a bot's face shows. The open chat knows its jobs and its scheduling device; any other bot has only the list's busy
 * state, read from the device serving the list.
 */
export function botMood(bot: BotRecord, workspace: ReturnType<typeof useWorkspace>): Mood {
  const open = workspace.conversationId === bot.conversation_id ? workspace.data : null
  const jobs = open?.scheduler?.jobs ?? []
  const busy = !open ? bot.busy
    : jobs.some((job) => job.state === "running" || job.state === "cancelling") ? "replying"
    : jobs.some((job) => ["pending_delivery", "queued", "steering", "claimed"].includes(job.state)) ? "thinking" : null
  const owner = open?.conversation?.owner
  return moodOf({
    busy,
    waiting: workspace.questions.some((question) => question.conversation_id === bot.conversation_id && questionOpen(question) && !(question.question_id in workspace.answered)),
    offline: Boolean(owner) && workspace.nodes.find((node) => node.device_id === owner)?.online === false,
  })
}
