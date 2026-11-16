import type { ComponentProps } from 'react'
import { Pressable, View } from 'react-native'
import { cn } from '@/lib/utils'

/** Arc radio-group and checkbox rows answer with color while the whole native hit target stays fixed. */
export function SelectionItem({ className, accessibilityState, ...props }: ComponentProps<typeof Pressable>) {
  return <Pressable {...props} accessibilityState={accessibilityState} className={cn('min-h-11 flex-row items-center gap-3 rounded-control border border-border bg-surface p-3 active:bg-surface-muted', accessibilityState?.checked && 'border-control-on bg-accent-subtle', props.disabled && 'opacity-50', className)} />
}

/** Arc radio-group's 17px mark and 7px dot, separated from the accessible hit target. */
export function RadioMark({ checked }: { checked: boolean }) {
  return <View accessible={false} pointerEvents="none" className={cn('size-[17px] items-center justify-center rounded-pill border', checked ? 'border-control-on bg-control-on' : 'border-strong bg-surface')}>
    {checked ? <View accessible={false} pointerEvents="none" className="size-[7px] rounded-pill bg-control-glyph" /> : null}
  </View>
}
