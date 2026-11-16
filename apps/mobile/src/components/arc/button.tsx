import type { ComponentProps } from 'react'
import { Platform, Pressable, View } from 'react-native'
import Animated, { useAnimatedStyle, useReducedMotion, useSharedValue, withTiming } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { TextClassContext } from './text'
import { cn } from '@/lib/utils'
import { tap } from '@/lib/motion'

const AnimatedPressable = withUniwind(Animated.createAnimatedComponent(Pressable))
const appearance = {
  primary: 'border-foreground bg-foreground active:opacity-[0.84]',
  secondary: 'border-border bg-surface active:bg-surface-muted',
  ghost: 'border-transparent bg-transparent active:bg-surface-muted',
  danger: 'border-border bg-surface active:border-danger active:bg-surface-muted',
}
const ink = { primary: 'text-background', secondary: 'text-foreground', ghost: 'text-secondary', danger: 'text-danger' }
const sizing = { sm: 'min-h-9 px-3', md: Platform.OS === 'android' ? 'min-h-12 min-w-12 px-4' : 'min-h-11 px-4', lg: 'min-h-[50px] px-5' }
const iconSizing = { sm: 'min-w-9 px-0', md: Platform.OS === 'android' ? 'min-w-12 px-0' : 'min-w-11 px-0', lg: 'min-w-[50px] px-0' }
export type ButtonProps = ComponentProps<typeof Pressable> & {
  variant?: keyof typeof appearance
  size?: keyof typeof sizing
  iconOnly?: boolean
}

/** Native port of Arc button: four variants, three sizes, border feedback and press scale. */
export function Button({ className, variant = 'primary', size = 'md', iconOnly, hitSlop, style, onPressIn, onPressOut, disabled, ...props }: ButtonProps) {
  const reduced = useReducedMotion()
  const scale = useSharedValue(1)
  const pressed = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }))
  const disabledOutline = disabled && (variant === 'secondary' || variant === 'danger')
  const minimumSlop = size === 'sm' ? (Platform.OS === 'android' ? 6 : 4) : 0
  const targetSlop = typeof hitSlop === 'number' ? Math.max(hitSlop, minimumSlop) : {
    top: Math.max(hitSlop?.top ?? 0, minimumSlop), bottom: Math.max(hitSlop?.bottom ?? 0, minimumSlop),
    left: Math.max(hitSlop?.left ?? 0, minimumSlop), right: Math.max(hitSlop?.right ?? 0, minimumSlop),
  }
  const control = <AnimatedPressable {...props} hitSlop={targetSlop} disabled={disabled} accessibilityRole={props.accessibilityRole ?? 'button'} accessibilityState={{ ...props.accessibilityState, disabled: Boolean(disabled) }}
      className={cn('shrink-0 flex-row items-center justify-center gap-2 rounded-control border py-0', appearance[variant], sizing[size], iconOnly && iconSizing[size], disabled && variant === 'primary' && 'opacity-[0.52]', disabledOutline && 'border-border bg-surface-muted', className)}
      style={[typeof style === 'function' ? undefined : style, pressed]}
      onPressIn={(event) => { scale.value = withTiming(reduced ? 1 : 0.97, tap); onPressIn?.(event) }}
      onPressOut={(event) => { scale.value = withTiming(1, tap); onPressOut?.(event) }} />
  return <TextClassContext.Provider value={cn('font-sans text-sm font-medium', ink[variant], disabled && variant === 'ghost' && 'text-muted', disabledOutline && 'text-muted')}>
    {size === 'sm' ? <View className={Platform.OS === 'android' ? 'min-h-12 min-w-12 justify-center p-1.5' : 'min-h-11 min-w-11 justify-center p-1'}>{control}</View> : control}
  </TextClassContext.Provider>
}
