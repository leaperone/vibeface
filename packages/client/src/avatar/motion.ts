import { BODY, HALO, eyePaths, faceOf, mix, outlinePath, pose, thinkingDots, type Mood, type Pose } from './face.ts'

/** The pose drawn when motion is off: eyes open, mid-drift, so each mood still reads. */
const STILL_T = 2600
const BLEND_MS = 420
/** Asleep is the only mood that drains colour; renderers fade a body filled at this saturation over the coloured one. */
export const ASLEEP = 0.35

/** What one frame draws, in the 100 viewBox: path data, the body's transform, and what fades in. */
export type Frame = {
  body: string; ring: string; left: string; right: string
  ringOpacity: number; grey: number; dx: number; dy: number; rotate: number; scale: number; dots: number[]
}

/**
 * A face's clock. A mood change blends from the pose on screen over 420 ms, easing out; `frame(null)` draws the still pose
 * and ends any blend. Each id starts at its own phase so a list of faces does not blink in step.
 */
export function faceMotion(botId: string, mood: Mood) {
  const face = faceOf(botId)
  let offset = 0
  for (const c of botId) offset = (offset * 31 + c.charCodeAt(0)) >>> 0
  offset %= 10000
  let shown = mood, from: Pose | null = null, start = -1, last: Pose | null = null
  let radii: number[] = [], body = '', ring = ''
  return {
    follow(next: Mood) {
      if (next === shown) return
      from = last
      start = -1
      shown = next
    },
    frame(now: number | null): Frame {
      const t = now === null ? STILL_T : now + offset
      let p = pose(face, shown, t)
      if (from && now !== null) {
        if (start < 0) start = now
        const k = Math.min(1, Math.max(0, (now - start) / BLEND_MS))
        p = mix(from, p, 1 - (1 - k) ** 3)
        if (k >= 1) from = null
      } else from = null
      last = p
      // Idle, waiting and asleep keep the outline still, so its path is rebuilt only when a radius moved.
      if (p.radii.some((r, i) => r !== radii[i])) {
        radii = p.radii
        body = outlinePath(radii, 50, 50, BODY)
        ring = outlinePath(radii, 50, 50, HALO.r)
      }
      const [left, right] = eyePaths(face, p)
      return {
        body, ring, left, right, ringOpacity: p.ring, grey: Math.min(1, Math.max(0, (1 - p.saturation) / (1 - ASLEEP))),
        dx: p.dx, dy: p.dy, rotate: p.rotate, scale: p.scale, dots: thinkingDots(p, t).map((dot) => dot.opacity),
      }
    },
  }
}
