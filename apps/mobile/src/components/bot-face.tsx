import { Star } from 'lucide-react-native'
import { useEffect, useId, useMemo, useRef, useState } from 'react'
import { AccessibilityInfo, AppState, View } from 'react-native'
import Animated, { useAnimatedProps, useAnimatedStyle, useSharedValue } from 'react-native-reanimated'
import Svg, { Circle, Defs, Path, RadialGradient, Stop } from 'react-native-svg'
import { useUniwind } from 'uniwind'
import { BODY, HALO, colors, eyePaths, faceOf, mix, outlinePath, pose, thinkingDots, type Face, type Mood, type Pose } from '@vibeface/client/avatar/face.ts'
import { moodOf } from '@vibeface/client/avatar/mood.ts'
import { questionOpen } from '@vibeface/client/workspace/questions.ts'
import type { BotRecord } from '@vibeface/client/workspace/types.ts'
import { Icon } from '@/components/ui/icon'
import type { useLive } from '@/data/live'
import type { Workspace } from '@/data/types'
import { cn } from '@/lib/utils'

const AnimatedPath = Animated.createAnimatedComponent(Path)
const AnimatedCircle = Animated.createAnimatedComponent(Circle)
/** The pose drawn when motion is off: eyes open, mid-drift, so each mood still reads. */
const STILL_T = 2600
const BLEND_MS = 420
/** Asleep is the only mood that drains colour; a grey body fades in over the coloured one as saturation drops. */
const ASLEEP = 0.35

type Frame = { body: string; ring: string; left: string; right: string; ringOpacity: number; grey: number; dx: number; dy: number; rotate: number; scale: number; dots: number[] }

/**
 * One JS-thread clock for every face on screen. face.ts is plain TypeScript shared with the web, not worklets, so each
 * frame is computed here and handed to the UI thread as a single shared value per face. It stops while the app is in
 * the background or the system asks for reduced motion.
 */
const ticks = new Set<(now: number) => void>()
let raf = 0
let active = AppState.currentState === 'active'
let reduced = false
const still = new Set<(reduced: boolean) => void>()
function loop(now: number) {
  for (const tick of ticks) tick(now)
  raf = ticks.size && active && !reduced ? requestAnimationFrame(loop) : 0
}
function kick() {
  if (!raf && ticks.size && active && !reduced) raf = requestAnimationFrame(loop)
}
AppState.addEventListener('change', (state) => { active = state === 'active'; kick() })
const setReduced = (next: boolean) => { reduced = next; for (const listener of still) listener(next); kick() }
void AccessibilityInfo.isReduceMotionEnabled().then(setReduced)
AccessibilityInfo.addEventListener('reduceMotionChanged', setReduced)

function useReduced() {
  const [value, setValue] = useState(reduced)
  useEffect(() => { still.add(setValue); setValue(reduced); return () => { still.delete(setValue) } }, [])
  return value
}

function phase(id: string) {
  let h = 0
  for (const c of id) h = (h * 31 + c.charCodeAt(0)) >>> 0
  return h % 10000
}

// Idle, waiting and asleep keep the outline still, so its path is rebuilt only when a radius moved.
function frameOf(face: Face, p: Pose, t: number, size: number, outline: { radii: number[]; body: string; ring: string }): Frame {
  if (p.radii.some((r, i) => r !== outline.radii[i])) {
    outline.radii = p.radii
    outline.body = outlinePath(p.radii, 50, 50, BODY)
    outline.ring = outlinePath(p.radii, 50, 50, HALO.r)
  }
  const [left, right] = eyePaths(face, p)
  return {
    body: outline.body, ring: outline.ring, left, right, ringOpacity: p.ring, grey: Math.min(1, Math.max(0, (1 - p.saturation) / (1 - ASLEEP))),
    dx: p.dx * size / 100, dy: p.dy * size / 100, rotate: p.rotate, scale: p.scale, dots: thinkingDots(p, t).map((dot) => dot.opacity),
  }
}

/** A bot's face, picked by its id and moving with its mood. Hidden from screen readers: the row says the mood. */
export function BotFace({ botId, mood, size, primary }: { botId: string; mood: Mood; size: 44 | 80; primary?: boolean }) {
  const { theme } = useUniwind()
  const dark = theme === 'dark'
  const face = useMemo(() => faceOf(botId), [botId])
  const palette = useMemo(() => ({ awake: colors(face, dark, 1), asleep: colors(face, dark, ASLEEP) }), [face, dark])
  const motionOff = useReduced()
  const gradient = `face${useId().replace(/[^a-zA-Z0-9]/g, '')}`
  const frame = useSharedValue<Frame>(useState(() => frameOf(face, pose(face, mood, STILL_T), STILL_T, size, { radii: [], body: '', ring: '' }))[0])
  const follow = useRef((_: Mood) => {})

  useEffect(() => {
    const offset = phase(botId), outline = { radii: [] as number[], body: '', ring: '' }
    // start < 0: a blend waits for its first frame to start its clock.
    let shown = mood, from: Pose | null = null, start = 0, last: Pose | null = null
    const draw = (now: number, t: number) => {
      let p = pose(face, shown, t)
      if (from) {
        if (start < 0) start = now
        const k = Math.min(1, (now - start) / BLEND_MS)
        p = mix(from, p, 1 - (1 - k) ** 3)
        if (k >= 1) from = null
      }
      last = p
      frame.value = frameOf(face, p, t, size, outline)
    }
    const tick = (now: number) => draw(now, now + offset)
    draw(0, motionOff ? STILL_T : performance.now() + offset)
    follow.current = (next: Mood) => {
      if (next === shown) return
      from = last
      start = -1
      shown = next
      if (motionOff || !active) { from = null; draw(0, STILL_T) }
    }
    if (motionOff) return
    ticks.add(tick)
    kick()
    return () => { ticks.delete(tick) }
    // A mood change goes through follow, so it blends from the current pose instead of restarting the clock.
  }, [face, size, motionOff, frame, botId])
  useEffect(() => { follow.current(mood) }, [mood])

  const bodyStyle = useAnimatedStyle(() => {
    const f = frame.value
    return { transform: [{ translateX: f.dx }, { translateY: f.dy }, { rotate: `${f.rotate}deg` }, { scale: f.scale }] }
  })
  const bodyProps = useAnimatedProps(() => ({ d: frame.value.body }))
  const greyProps = useAnimatedProps(() => ({ d: frame.value.body, opacity: frame.value.grey }))
  const ringProps = useAnimatedProps(() => ({ d: frame.value.ring, opacity: frame.value.ringOpacity }))
  const leftProps = useAnimatedProps(() => ({ d: frame.value.left }))
  const rightProps = useAnimatedProps(() => ({ d: frame.value.right }))
  const dot0 = useAnimatedProps(() => ({ opacity: frame.value.dots[0] }))
  const dot1 = useAnimatedProps(() => ({ opacity: frame.value.dots[1] }))
  const dot2 = useAnimatedProps(() => ({ opacity: frame.value.dots[2] }))
  const dots = useMemo(() => thinkingDots(pose(face, 'thinking', 0), 0), [face])
  const ink = mood === 'asleep' ? palette.asleep.ink : palette.awake.ink

  return <View importantForAccessibility="no-hide-descendants" accessibilityElementsHidden style={{ width: size, height: size }}>
    <Animated.View style={[{ position: 'absolute', inset: 0 }, bodyStyle]}>
      <Svg viewBox="0 0 100 100" width={size} height={size}>
        <Defs>
          {(['awake', 'asleep'] as const).map((key) => <RadialGradient key={key} id={`${gradient}${key}`} cx="38%" cy="30%" r="75%">
            <Stop offset={0} stopColor={palette[key].top} />
            <Stop offset={0.55} stopColor={palette[key].base} />
            <Stop offset={1} stopColor={palette[key].bottom} />
          </RadialGradient>)}
        </Defs>
        <AnimatedPath animatedProps={ringProps} fill="none" stroke={palette.awake.ring} strokeWidth={HALO.width} />
        <AnimatedPath animatedProps={bodyProps} fill={`url(#${gradient}awake)`} />
        <AnimatedPath animatedProps={greyProps} fill={`url(#${gradient}asleep)`} />
        <AnimatedPath animatedProps={leftProps} fill={ink} />
        <AnimatedPath animatedProps={rightProps} fill={ink} />
      </Svg>
    </Animated.View>
    <Svg viewBox="0 0 100 100" width={size} height={size} style={{ position: 'absolute' }}>
      {[dot0, dot1, dot2].map((props, i) => <AnimatedCircle key={i} animatedProps={props} cx={dots[i].x} cy={dots[i].y} r={dots[i].r} fill={palette.awake.thought} />)}
    </Svg>
    {primary ? <View className={cn('absolute items-center justify-center rounded-full bg-background', size === 80 ? '-right-0.5 -bottom-0.5 size-6' : '-right-1 -bottom-1 size-4')}>
      <Icon as={Star} className={cn('fill-amber-500 text-amber-500', size === 80 ? 'size-4' : 'size-3')} />
    </View> : null}
  </View>
}

/**
 * What a bot's face shows: the open chat knows whether a reply is streaming and whether its device is offline; any
 * other bot has only the list's busy state, read from the device serving the list.
 */
export function botMood(bot: BotRecord, workspace: Workspace, live: ReturnType<typeof useLive>): Mood {
  const chat = workspace.conversations[bot.conversation_id]
  const busy = !chat ? bot.busy : !chat.replying ? null : chat.messages.some((message) => message.state === 'streaming') ? 'replying' : 'thinking'
  const owner = live?.conversationId === bot.conversation_id ? live.data?.conversation?.owner : undefined
  return moodOf({
    busy,
    waiting: workspace.questions.some((question) => question.conversation_id === bot.conversation_id && questionOpen(question)),
    offline: Boolean(owner) && live?.nodes.find((node) => node.device_id === owner)?.online === false,
  })
}
