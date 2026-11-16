import type { ComponentProps } from 'react'
import { View } from 'react-native'
import { cn } from '@/lib/utils'

/** Arc popover panel; the native Modal owns portal placement and dismissal. */
export function PopoverPanel({ className, ...props }: ComponentProps<typeof View>) {
  return <View {...props} className={cn('rounded-panel border border-border bg-surface-raised p-2', className)} />
}
