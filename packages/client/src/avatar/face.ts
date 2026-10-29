export type Outline = 'round' | 'squircle' | 'drop' | 'bean' | 'cloud'
export type Eyes = 'dot' | 'pill' | 'oval' | 'wide'
export type Mood = 'idle' | 'thinking' | 'replying' | 'waiting' | 'asleep'
export type Hue = typeof HUES[number]
/** The parts of a face a person or the bot picked; anything unpicked comes from the bot's id. */
export type Avatar = { outline: Outline; eyes: Eyes; hue: Hue }
/** `seed` sets the blink phase; it always comes from the id, so a new look keeps the bot's rhythm. */
export type Face = Avatar & { gap: number; tilt: number; seed: number }
export type Pose = {
  radii: number[]; dx: number; dy: number; scale: number; rotate: number
  open: number; gazeX: number; gazeY: number; squint: number; saturation: number; ring: number; dots: number
}
export type Colors = { top: string; base: string; bottom: string; ink: string; ring: string; thought: string }

export const SAMPLES = 48
/** Body radius in the 100 viewBox; the halo ring sits a 1.5 gap outside it. */
export const BODY = 36
export const HALO = { r: BODY + 1.5 + 1.2, width: 2.4 }
export const OUTLINES: readonly Outline[] = ['round', 'squircle', 'drop', 'bean', 'cloud']
export const EYES: readonly Eyes[] = ['dot', 'pill', 'oval', 'wide']
// OKLCH hues. 60 to 90 is left to the amber waiting halo so no face blurs into it. The Node accepts these same values.
export const HUES = [20, 45, 100, 130, 155, 180, 205, 230, 255, 285, 315, 345] as const
const YELLOW = 100
const ANGLES = Array.from({ length: SAMPLES }, (_, i) => 2 * Math.PI * i / SAMPLES)

export function faceOf(id: string, avatar?: Avatar | null): Face {
  let hash = 2166136261
  for (const char of id) hash = Math.imul(hash ^ char.charCodeAt(0), 16777619)
  hash = (hash ^ (hash >>> 16)) >>> 0
  const more = unit(hash, 7) * 2 ** 32 >>> 0
  const hue = HUES[hash % HUES.length], outline = OUTLINES[(hash >>> 3) % OUTLINES.length]
  const gap = 10 + (more >>> 9) % 41 / 10, tilt = (more >>> 17) % 13 - 6
  const seed = Math.round(gap * 10) * 7919 + (tilt + 6) * 131 + hue + OUTLINES.indexOf(outline) * 31
  return { hue, outline, eyes: EYES[(more >>> 4) % EYES.length], gap, tilt, seed, ...avatar }
}

export const sameAvatar = (a: Avatar | null, b: Avatar | null) =>
  a === b || Boolean(a && b && a.outline === b.outline && a.eyes === b.eyes && a.hue === b.hue)

const shapes: Record<Outline, (a: number) => number> = {
  round: () => 1,
  squircle: a => (Math.abs(Math.cos(a)) ** 4 + Math.abs(Math.sin(a)) ** 4) ** -0.25,
  // y points down: sin(a) = -1 is the top, which narrows to a soft point while the bottom fills out.
  drop: a => 1 + 0.13 * Math.max(0, -Math.sin(a)) ** 6 + 0.04 * Math.max(0, Math.sin(a)),
  bean: a => 1 + 0.06 * Math.cos(2 * a) - 0.05 * Math.sin(a),
  cloud: a => 1 + 0.07 * (Math.abs(Math.cos(3.5 * a)) - 0.6),
}

// Every outline is scaled to the circle's area so no shape looks heavier than another.
const BASE = Object.fromEntries(OUTLINES.map(outline => {
  const raw = ANGLES.map(shapes[outline])
  const k = 1 / Math.sqrt(raw.reduce((sum, r) => sum + r * r, 0) / SAMPLES)
  return [outline, raw.map(r => r * k)]
})) as Record<Outline, number[]>

type Motion = Omit<Pose, 'radii' | 'rotate'> & { lean: number; ripple?: (a: number) => number }
const still = { dx: 0, dy: 0, scale: 1, lean: 0, open: 1, gazeX: 0, gazeY: 0, squint: 0, saturation: 1, ring: 0, dots: 0 }

const moods: Record<Mood, (t: number, seed: number) => Motion> = {
  idle: (t, seed) => ({
    ...still, dy: 1.2 * Math.sin(t / 1400), open: blink(t, seed, false),
    gazeX: 0.8 * Math.sin(t / 2300 + seed % 7), gazeY: 0.5 * Math.sin(t / 3100),
  }),
  thinking: (t, seed) => ({
    ...still, scale: 1 + 0.02 * Math.sin(t / 500), open: blink(t, seed, false), dots: 1,
    gazeX: 3 * Math.sin(t / 700), gazeY: -4, ripple: a => 0.02 * Math.cos(3 * a - t / 400),
  }),
  replying: (t, seed) => {
    const beat = Math.abs(Math.sin(t / 220))
    return {
      ...still, dy: -1.5 * beat, scale: 1 + 0.015 * beat, open: blink(t, seed, false),
      squint: 0.85, ripple: a => 0.03 * Math.cos(4 * a + t / 180),
    }
  },
  // open above 1 is the "eyes wide, looking at you" read.
  waiting: (t, seed) => ({ ...still, lean: 4 * Math.sin(t / 900), open: 1.15 * blink(t, seed, true), ring: 1 }),
  asleep: t => ({ ...still, dy: 1, gazeY: 1, scale: 1 + 0.025 * Math.sin(t / 1600), open: 0.05, saturation: 0.35 }),
}

export function pose(face: Face, mood: Mood, t: number): Pose {
  const { lean, ripple, ...motion } = moods[mood](t, face.seed)
  const base = BASE[face.outline]
  const radii = ripple ? base.map((r, i) => clamp(r + ripple(ANGLES[i]), 0.85, 1.15)) : base.map(r => clamp(r, 0.85, 1.15))
  return { ...motion, radii, rotate: face.tilt + lean }
}

export function mix(a: Pose, b: Pose, k: number): Pose {
  const l = (x: number, y: number) => x + (y - x) * k
  return {
    radii: a.radii.map((r, i) => l(r, b.radii[i])),
    dx: l(a.dx, b.dx), dy: l(a.dy, b.dy), scale: l(a.scale, b.scale), rotate: l(a.rotate, b.rotate),
    open: l(a.open, b.open), gazeX: l(a.gazeX, b.gazeX), gazeY: l(a.gazeY, b.gazeY), squint: l(a.squint, b.squint),
    saturation: l(a.saturation, b.saturation), ring: l(a.ring, b.ring), dots: l(a.dots, b.dots),
  }
}

export function outlinePath(radii: number[], cx: number, cy: number, r: number): string {
  return closedPath(radii.map((rho, i) => [cx + r * rho * Math.cos(ANGLES[i]), cy + r * rho * Math.sin(ANGLES[i])]))
}

const THOUGHT = [[82, 15, 4.4], [90, 7.5, 3.4], [95.5, 2.6, 2.5]]

/** Three dots up and to the right of the body, outside its rotation, pulsing in turn. */
export function thinkingDots(p: Pose, t: number): { x: number; y: number; r: number; opacity: number }[] {
  const shown = clamp(p.dots, 0, 1)
  return THOUGHT.map(([x, y, r], i) => ({ x, y, r, opacity: shown * (0.35 + 0.65 * (0.5 + 0.5 * Math.sin(t / 260 - i * 1.3))) }))
}

const EYE_POINTS = 24
const contours: Record<Eyes, number[][]> = {
  dot: ellipse(5.9, 5.9),
  pill: ellipse(4.8, 8.7),
  oval: ellipse(7, 5.6),
  wide: ellipse(7.3, 8.1),
}
const happy: Record<Eyes, number[][]> = {
  dot: crescent(5.9), pill: crescent(5.3), oval: crescent(7), wide: crescent(7.3),
}
// A shut eye still reads as a line, not as nothing.
const LID = 1.2

/** Both eyes as filled path data; squint and blink are folded into the contour so renderers only fill. */
export function eyePaths(face: Face, p: Pose): string[] {
  const open = clamp(p.open, 0, 1.2)
  const squint = clamp(p.squint, 0, 1)
  const contour = contours[face.eyes].map(([x, y], i) => [x + (happy[face.eyes][i][0] - x) * squint, y + (happy[face.eyes][i][1] - y) * squint])
  const half = Math.max(...contour.map(([, y]) => Math.abs(y)))
  const k = Math.max(open, LID / half)
  const cy = 46 + p.gazeY
  return [-1, 1].map(side => {
    const cx = 50 + side * face.gap + p.gazeX
    const body = closedPath(contour.map(([x, y]) => [cx + x, cy + y * k]))
    if (face.eyes !== 'wide' || open < 0.6 || squint > 0.3) return body
    // Wound against the eye so the nonzero fill leaves a highlight hole.
    return `${body}${closedPath(ellipse(2, 2).reverse().map(([x, y]) => [cx + 2.5 + x, cy - 2.8 * k + y]))}`
  })
}

/** Body gradient stops (radial, centred at 38% 30%), ink and the warning halo, all at one perceived lightness. */
export function colors(face: Face, dark: boolean, saturation: number): Colors {
  const h = face.hue
  const s = clamp(saturation, 0, 1)
  // Yellow only reads clean near its natural lightness; at the shared one it turns olive and blurs into the amber halo.
  const lift = h === YELLOW ? 0.08 : 0
  const [l, c] = dark ? [0.70 + lift, 0.10] : [0.74 + lift, 0.11]
  const base = oklch(l, c * s, h)
  const ink = oklch(dark ? 0.18 : 0.24, 0.04 * s, h)
  // Thinking dots sit on the page, not the body, so dark ink would vanish on a dark page.
  return {
    top: oklch(l + 0.07, c * s, h),
    base,
    bottom: oklch(l - 0.07, c * s, h),
    ink,
    thought: dark ? base : ink,
    ring: dark ? oklch(0.83, 0.15, 80) : oklch(0.77, 0.17, 70),
  }
}

// OKLCH to sRGB hex; chroma shrinks until the colour fits the gamut so hue and lightness hold.
export function oklch(l: number, c: number, h: number) {
  const a = h * Math.PI / 180
  for (let chroma = c; ; chroma = Math.max(0, chroma - 0.005)) {
    const rgb = oklabToSrgb(l, chroma * Math.cos(a), chroma * Math.sin(a))
    if (chroma === 0 || rgb.every(v => v >= -1e-4 && v <= 1 + 1e-4)) {
      return `#${rgb.map(v => Math.round(clamp(gamma(v), 0, 1) * 255).toString(16).padStart(2, '0')).join('')}`
    }
  }
}

function oklabToSrgb(L: number, a: number, b: number) {
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ]
}

function gamma(v: number) {
  return v <= 0.0031308 ? 12.92 * v : 1.055 * Math.max(0, v) ** (1 / 2.4) - 0.055
}

function ellipse(rx: number, ry: number) {
  return Array.from({ length: EYE_POINTS }, (_, i) => {
    const a = Math.PI + 2 * Math.PI * i / EYE_POINTS
    return [rx * Math.cos(a), ry * Math.sin(a)]
  })
}

// An upward ∩ band sampled at the same x positions as ellipse(), so the two blend point for point.
function crescent(w: number) {
  const height = 0.74 * w
  return Array.from({ length: EYE_POINTS }, (_, i) => {
    const top = i < EYE_POINTS / 2
    const x = w * Math.cos(Math.PI + 2 * Math.PI * i / EYE_POINTS)
    const u = x / w
    const mid = -height * (1 - u * u) + height * 0.45
    const half = 0.28 * w * (0.6 + 0.4 * (1 - u * u))
    return [x, top ? mid - half : mid + half]
  })
}

function closedPath(points: number[][]) {
  const n = points.length
  const f = (v: number) => Math.round(v * 100) / 100
  let d = `M${f(points[0][0])} ${f(points[0][1])}`
  for (let i = 0; i < n; i++) {
    const [p0, p1, p2, p3] = [points[(i + n - 1) % n], points[i], points[(i + 1) % n], points[(i + 2) % n]]
    d += `C${f(p1[0] + (p2[0] - p0[0]) / 6)} ${f(p1[1] + (p2[1] - p0[1]) / 6)} ${f(p2[0] - (p3[0] - p1[0]) / 6)} ${f(p2[1] - (p3[1] - p1[1]) / 6)} ${f(p2[0])} ${f(p2[1])}`
  }
  return `${d}Z`
}

const BLINK_SLOT = 4400
// One blink per 4.4 s slot at a jittered offset gives gaps of 2.8–6 s, and t = 0 is never mid-blink.
function blink(t: number, seed: number, double: boolean) {
  const slot = Math.floor(t / BLINK_SLOT)
  const at = slot * BLINK_SLOT + 200 + unit(seed, slot) * 1600
  const shut = (d: number) => {
    const x = Math.min(1, Math.abs(d) / 70)
    return x * x * (3 - 2 * x)
  }
  return double && unit(seed + 1, slot) < 0.5 ? Math.min(shut(t - at), shut(t - at - 260)) : shut(t - at)
}

function unit(seed: number, n: number) {
  let h = Math.imul(seed ^ Math.imul(n, 0x9e3779b1), 0x85ebca6b)
  h ^= h >>> 13
  h = Math.imul(h, 0xc2b2ae35)
  h ^= h >>> 16
  return (h >>> 0) / 2 ** 32
}

function clamp(v: number, lo: number, hi: number) {
  return Math.min(hi, Math.max(lo, v))
}
