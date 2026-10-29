import { useFocusEffect } from 'expo-router'
import { createContext, useCallback, useContext, useEffect, useId, useMemo, useState, type ReactNode } from 'react'
import { AccessibilityInfo, AppState, View } from 'react-native'
import Animated, { useAnimatedProps, useAnimatedStyle, useSharedValue } from 'react-native-reanimated'
import Svg, { Circle, Defs, Path, RadialGradient, Stop } from 'react-native-svg'
import { useUniwind } from 'uniwind'
import { colors, faceOf, HALO, pose, thinkingDots, type Mood } from '@vibeface/client/avatar/face.ts'
import { ASLEEP, faceMotion, type Frame } from '@vibeface/client/avatar/motion.ts'
import { moodOf } from '@vibeface/client/avatar/mood.ts'
import { questionOpen } from '@vibeface/client/workspace/questions.ts'
import type { BotRecord } from '@vibeface/client/workspace/types.ts'
import type { useLive } from '@/data/live'
import type { Workspace } from '@/data/types'

const AnimatedPath = Animated.createAnimatedComponent(Path)
const AnimatedCircle = Animated.createAnimatedComponent(Circle)

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

/** Faces inside a screen another screen covers stop drawing, as the web does for faces scrolled out of view. */
const Covered = createContext(false)
export function ScreenFaces({ children }: { children: ReactNode }) {
  const [covered, setCovered] = useState(false)
  useFocusEffect(useCallback(() => { setCovered(false); return () => setCovered(true) }, []))
  return <Covered.Provider value={covered}>{children}</Covered.Provider>
}

/** A bot's face, picked by its id and moving with its mood. Hidden from screen readers: the row says the mood. */
/** `halo`: a still glow in the bot's tint behind the face, 1.8 times its size. */
export function BotFace({ botId, mood, size, halo }: { botId: string; mood: Mood; size: number; halo?: string }) {
  const { theme } = useUniwind()
  const dark = theme === 'dark'
  const face = useMemo(() => faceOf(botId), [botId])
  const palette = useMemo(() => ({ awake: colors(face, dark, 1), asleep: colors(face, dark, ASLEEP) }), [face, dark])
  const reducedMotion = useReduced()
  const motionOff = useContext(Covered) || reducedMotion
  const gradient = `face${useId().replace(/[^a-zA-Z0-9]/g, '')}`
  const motion = useMemo(() => faceMotion(botId, mood), [botId])
  const frame = useSharedValue<Frame>(useState(() => motion.frame(null))[0])

  useEffect(() => {
    const draw = (now: number | null) => { frame.value = motion.frame(now) }
    draw(motionOff ? null : performance.now())
    if (motionOff) return
    ticks.add(draw)
    kick()
    return () => { ticks.delete(draw) }
  }, [motion, motionOff, frame])
  useEffect(() => {
    motion.follow(mood)
    if (motionOff || !active) frame.value = motion.frame(null)
  }, [motion, mood, motionOff, frame])

  const bodyStyle = useAnimatedStyle(() => {
    const f = frame.value
    return { transform: [{ translateX: f.dx * size / 100 }, { translateY: f.dy * size / 100 }, { rotate: `${f.rotate}deg` }, { scale: f.scale }] }
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
    {halo ? <Svg width={size * 1.8} height={size * 1.8} style={{ position: 'absolute', left: -size * 0.4, top: -size * 0.4 }}>
      <Defs>
        <RadialGradient id={`${gradient}halo`}>
          <Stop offset={0.5} stopColor={halo} stopOpacity={dark ? 0.18 : 0.14} />
          <Stop offset={1} stopColor={halo} stopOpacity={0} />
        </RadialGradient>
      </Defs>
      <Circle cx="50%" cy="50%" r="50%" fill={`url(#${gradient}halo)`} />
    </Svg> : null}
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
    waiting: workspace.questions.some((question) => question.conversation_id === bot.conversation_id && questionOpen(question) && !(question.question_id in (live?.answered ?? {}))),
    offline: Boolean(owner) && live?.nodes.find((node) => node.device_id === owner)?.online === false,
  })
}
