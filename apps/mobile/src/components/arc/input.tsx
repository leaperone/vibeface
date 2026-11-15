import { useState, type ComponentProps } from 'react'
import { Platform, TextInput } from 'react-native'
import { cn } from '@/lib/utils'

export const caret = Platform.OS === 'ios'
  ? { selectionColorClassName: 'accent-foreground' }
  : { cursorColorClassName: 'accent-foreground', selectionHandleColorClassName: 'accent-foreground', selectionColorClassName: 'accent-foreground/25' }

/** Arc input uses a border-only focus state and a stable 44px minimum tap target. */
export function Input({ className, onFocus, onBlur, editable, ...props }: ComponentProps<typeof TextInput>) {
  const [focused, setFocused] = useState(false)
  return <TextInput {...caret} {...props} editable={editable} placeholderTextColorClassName="accent-text-muted"
    className={cn('min-h-11 w-full min-w-0 rounded-control border border-strong bg-surface px-3 py-2 font-sans text-sm text-foreground', focused && 'border-foreground', editable === false && 'bg-surface-muted opacity-50', className)}
    onFocus={(event) => { setFocused(true); onFocus?.(event) }} onBlur={(event) => { setFocused(false); onBlur?.(event) }} />
}
