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

/**
 * One choice out of a few, with a thumb that slides to it. Stacks once large text no longer fits a row.
 * `snap` moves the thumb at once, for choices whose effect repaints the whole app and would stall a slide.
 */
export function Segmented<T extends string>({ options, value, onChange, disabled, snap }: { options: readonly Option<T>[]; value: T; onChange: (value: T) => void; disabled?: boolean; snap?: boolean }) {
  const stacked = useWindowDimensions().fontScale >= 1.3
  const [frames, setFrames] = useState<Partial<Record<T, LayoutRectangle>>>({})
  // The thumb follows the tap at once; the owner may apply the choice later or put it back.
  const [shown, setShown] = useState(value)
  useEffect(() => setShown(value), [value])
  const frame = frames[shown]
  const x = useSharedValue(0), y = useSharedValue(0), width = useSharedValue(0), height = useSharedValue(0)
  const placed = useRef<LayoutRectangle>(undefined)
  const move = (next: LayoutRectangle) => {
    const at = placed.current
    if (at && at.x === next.x && at.y === next.y && at.width === next.width && at.height === next.height) return
    const to = (shared: SharedValue<number>, value: number) => { shared.value = at && !snap ? withSpring(value, slide) : value }
    to(x, next.x); to(y, next.y); to(width, next.width); to(height, next.height)
    placed.current = next
  }
  useEffect(() => { if (frame) move(frame) }, [frame?.x, frame?.y, frame?.width, frame?.height])
  const thumb = useAnimatedStyle(() => ({ width: width.value, height: height.value, transform: [{ translateX: x.value }, { translateY: y.value }] }))
  // A quick save never dims the group; only a wait long enough to notice does.
  const dim = { transitionProperty: 'opacity', transitionDuration: 150, transitionDelay: disabled ? 300 : 0 } as const
  return <AnimatedView accessibilityRole="radiogroup" style={dim} className={cn('gap-1 rounded-lg bg-muted p-1', stacked ? 'self-stretch' : 'flex-row self-start', disabled && 'opacity-50')}>
    <AnimatedView pointerEvents="none" style={thumb} className={cn('absolute top-0 left-0 rounded-md border border-border bg-background shadow-sm shadow-black/5 dark:border-input dark:bg-input/30', !frame && 'opacity-0')} />
    {options.map((option) => <Button key={option.value} variant="ghost" accessibilityRole="radio" accessibilityState={{ checked: option.value === shown }} disabled={disabled}
      className={cn('min-h-11 opacity-100 active:bg-transparent dark:active:bg-transparent', stacked && 'justify-start')}
      onLayout={(event) => { const next = event.nativeEvent.layout; setFrames((all) => ({ ...all, [option.value]: next })) }}
      // The thumb starts on the tap itself, not after a render that a theme repaint can hold up.
      onPress={() => { if (option.value === shown) return; void Haptics.selectionAsync(); const next = frames[option.value]; if (next) move(next); setShown(option.value); onChange(option.value) }}>
      {option.icon ? <Icon as={option.icon} /> : null}<Text>{option.label}</Text>
    </Button>)}
  </AnimatedView>
}
