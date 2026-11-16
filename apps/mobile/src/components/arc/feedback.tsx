import type { ReactNode } from 'react'
import { View } from 'react-native'
import Animated, { useReducedMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { cn } from '@/lib/utils'
import { cssEaseOut } from '@/lib/motion'

const Block = withUniwind(Animated.View)
const pulse = { from: { opacity: 1 }, '50%': { opacity: 0.52 }, to: { opacity: 1 } }
/** Arc skeleton's finite 1.8s pulse stops after eleven cycles. Reduced motion is static. */
export function Skeleton({ className }: { className?: string }) {
  const reduced = useReducedMotion()
  return <Block accessible={false} className={cn('rounded-control bg-border-strong/40', className)}
    style={reduced ? undefined : { animationName: pulse, animationDuration: '1.8s', animationIterationCount: 11, animationTimingFunction: cssEaseOut }} />
}
/** Foreground errors remain inline and visible until their owner clears them. */
export function InlineError({ children }: { children: ReactNode }) {
  return <View role="alert" accessible className="flex-row items-start gap-3 rounded-control border border-border bg-surface p-4">{children}</View>
}
/** Arc toast floating layer. Announcements are dispatched by the product queue. */
export function ToastNotice({ children }: { children: ReactNode }) {
  return <View className="w-full max-w-sm flex-row items-center gap-3 rounded-panel border border-border bg-surface-raised px-4 py-3">{children}</View>
}
