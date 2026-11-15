import { useEffect } from 'react'
import { Pressable, View, type SwitchProps } from 'react-native'
import Animated, { useAnimatedStyle, useSharedValue, withTiming } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { glide } from '@/lib/motion'
import { cn } from '@/lib/utils'

const Thumb = withUniwind(Animated.View)
/** Arc switch's 42×24 track and 18px moving thumb, with a native switch accessibility contract. */
export function Switch({ value = false, onValueChange, disabled, style, accessibilityLabel, accessibilityHint, testID }: SwitchProps) {
  const x = useSharedValue(value ? 18 : 0)
  useEffect(() => { x.value = withTiming(value ? 18 : 0, glide) }, [value])
  const movement = useAnimatedStyle(() => ({ transform: [{ translateX: x.value }] }))
  return <Pressable accessibilityRole="switch" accessibilityLabel={accessibilityLabel} accessibilityHint={accessibilityHint} accessibilityState={{ checked: value, disabled: Boolean(disabled) }} disabled={disabled} onPress={() => onValueChange?.(!value)} testID={testID} style={style} className={cn('min-h-11 min-w-11 justify-center', disabled && 'opacity-50')}>
    <View className={cn('h-6 w-[42px] justify-center rounded-pill p-[3px]', value ? 'bg-control-on' : 'bg-control-track')}>
      <Thumb style={movement} className={cn('size-[18px] rounded-pill', value ? 'bg-control-thumb-on' : 'bg-control-thumb')} />
    </View>
  </Pressable>
}
