import type { LucideIcon } from 'lucide-react-native'
import { useEffect, useState } from 'react'
import { Pressable, useWindowDimensions, View, type LayoutRectangle } from 'react-native'
import Animated, { useAnimatedStyle, useSharedValue, withTiming } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { Icon } from './icon'
import { Text } from './text'
import { glide } from '@/lib/motion'
import { cn } from '@/lib/utils'

const Selection = withUniwind(Animated.View)
type Option<T> = { value: T; label: string; icon?: LucideIcon }
/** Arc segmented-control. Only the selection moves; labels keep their weight and scale. */
export function Segmented<T extends string>({ options, value, onChange, disabled, snap }: { options: readonly Option<T>[]; value: T; onChange: (value: T) => void; disabled?: boolean; snap?: boolean }) {
  const stacked = useWindowDimensions().fontScale >= 1.3
  const [frames, setFrames] = useState<Partial<Record<T, LayoutRectangle>>>({})
  const selected = frames[value]
  const x = useSharedValue(0), y = useSharedValue(0), width = useSharedValue(0), height = useSharedValue(0)
  useEffect(() => {
    if (!selected) return
    x.value = snap ? selected.x : withTiming(selected.x, glide)
    y.value = snap ? selected.y : withTiming(selected.y, glide)
    width.value = snap ? selected.width : withTiming(selected.width, glide)
    height.value = snap ? selected.height : withTiming(selected.height, glide)
  }, [selected?.x, selected?.y, selected?.width, selected?.height, snap])
  const frame = useAnimatedStyle(() => ({ width: width.value, height: height.value, transform: [{ translateX: x.value }, { translateY: y.value }] }))
  return <View accessibilityRole="radiogroup" className={cn('min-w-0 gap-0.5 rounded-control border border-border bg-surface-muted p-[3px]', stacked ? 'self-stretch' : 'flex-row self-start', disabled && 'opacity-50')}>
    <Selection pointerEvents="none" style={frame} className={cn('absolute left-0 top-0 rounded-[14px] border border-border bg-surface', !selected && 'opacity-0')} />
    {options.map((option) => <Pressable key={option.value} accessibilityRole="radio" accessibilityState={{ checked: value === option.value, disabled: Boolean(disabled) }} disabled={disabled}
      onLayout={(event) => { const measured = event.nativeEvent.layout; setFrames((previous) => ({ ...previous, [option.value]: measured })) }}
      onPress={() => { if (value !== option.value) onChange(option.value) }} className="min-h-11 flex-row items-center justify-center gap-2 px-3 py-2">
      {option.icon ? <Icon as={option.icon} className={value === option.value ? 'text-foreground' : 'text-muted'} /> : null}
      <Text className={cn('text-sm font-medium', value === option.value ? 'text-foreground' : 'text-muted')}>{option.label}</Text>
    </Pressable>)}
  </View>
}
