import * as Haptics from 'expo-haptics'
import type { LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { useWindowDimensions, type LayoutRectangle } from 'react-native'
import Animated, { ReduceMotion, useAnimatedStyle, useSharedValue, withSpring, type SharedValue } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { cn } from '@/lib/utils'

const AnimatedView = withUniwind(Animated.View)
// Critically damped: the thumb travels to the choice and settles without overshoot.
const slide = { dampingRatio: 1, duration: 350, reduceMotion: ReduceMotion.System }

type Option<T> = { value: T; label: string; icon?: LucideIcon }

/** One choice out of a few, with a thumb that slides to it. Stacks once large text no longer fits a row. */
export function Segmented<T extends string>({ options, value, onChange, disabled }: { options: readonly Option<T>[]; value: T; onChange: (value: T) => void; disabled?: boolean }) {
  const stacked = useWindowDimensions().fontScale >= 1.3
  const [frames, setFrames] = useState<Partial<Record<T, LayoutRectangle>>>({})
  // The thumb follows the tap at once; the owner may apply the choice later or put it back.
  const [shown, setShown] = useState(value)
  useEffect(() => setShown(value), [value])
  const frame = frames[shown]
  const x = useSharedValue(0), y = useSharedValue(0), width = useSharedValue(0), height = useSharedValue(0)
  const placed = useRef(false)
  useEffect(() => {
    if (!frame) return
    const to = (shared: SharedValue<number>, next: number) => { shared.value = placed.current ? withSpring(next, slide) : next }
    to(x, frame.x); to(y, frame.y); to(width, frame.width); to(height, frame.height)
    placed.current = true
  }, [frame?.x, frame?.y, frame?.width, frame?.height])
  const thumb = useAnimatedStyle(() => ({ width: width.value, height: height.value, transform: [{ translateX: x.value }, { translateY: y.value }] }))
  // A quick save never dims the group; only a wait long enough to notice does.
  const dim = { transitionProperty: 'opacity', transitionDuration: 150, transitionDelay: disabled ? 300 : 0 } as const
  return <AnimatedView accessibilityRole="radiogroup" style={dim} className={cn('gap-1 rounded-lg bg-muted p-1', stacked ? 'self-stretch' : 'flex-row self-start', disabled && 'opacity-50')}>
    <AnimatedView pointerEvents="none" style={thumb} className={cn('absolute top-0 left-0 rounded-md border border-border bg-background shadow-sm shadow-black/5 dark:border-input dark:bg-input/30', !frame && 'opacity-0')} />
    {options.map((option) => <Button key={option.value} variant="ghost" accessibilityRole="radio" accessibilityState={{ checked: option.value === shown }} disabled={disabled}
      className={cn('min-h-11 opacity-100 active:bg-transparent dark:active:bg-transparent', stacked && 'justify-start')}
      onLayout={(event) => { const next = event.nativeEvent.layout; setFrames((all) => ({ ...all, [option.value]: next })) }}
      onPress={() => { if (option.value === shown) return; void Haptics.selectionAsync(); setShown(option.value); onChange(option.value) }}>
      {option.icon ? <Icon as={option.icon} /> : null}<Text>{option.label}</Text>
    </Button>)}
  </AnimatedView>
}
