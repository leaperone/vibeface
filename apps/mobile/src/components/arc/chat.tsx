import type { ComponentProps, ReactNode } from 'react'
import { TextInput, View } from 'react-native'
import { caret } from './input'
import { cn } from '@/lib/utils'

/** Arc ai-chat's input dock: one bordered panel with attachments, editor, and action row. */
export function ComposerPanel({ attachments, editor, actions }: { attachments?: ReactNode; editor: ReactNode; actions: ReactNode }) {
  return <View className="gap-2 rounded-panel border border-border bg-surface p-2">
    {attachments}
    {editor}
    <View className="flex-row items-center justify-between gap-2">{actions}</View>
  </View>
}
/** Arc chat bubbles stay neutral; assistant messages use the full content column. */
export function MessageBubble({ className, ...props }: ComponentProps<typeof View>) {
  return <View {...props} className={cn('rounded-[20px] bg-surface-muted px-4 py-3', className)} />
}

/** Arc ai-chat's multiline field. Keyboard and draft ownership remain in the feature. */
export function ComposerInput({ className, ...props }: ComponentProps<typeof TextInput>) {
  return <TextInput {...caret} multiline maxFontSizeMultiplier={2} placeholderTextColorClassName="accent-text-muted" {...props} className={cn('max-h-40 min-h-11 min-w-0 px-3 py-2 font-sans text-base leading-6 text-foreground', className)} />
}
