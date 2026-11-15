import type { ComponentProps, ReactNode } from 'react'
import { useWindowDimensions, View } from 'react-native'
import { arcTokens } from '@vibeface/theme/tokens'
import { cn } from '@/lib/utils'
import { Text } from './text'

/** Arc card is a resting bordered boundary. It never owns a shadow. */
export function Card({ className, ...props }: ComponentProps<typeof View>) {
  return <View {...props} className={cn('gap-4 overflow-hidden rounded-panel border border-border bg-surface p-5', className)} />
}
/** Arc settings groups use proximity and dividers instead of nested cards. */
export function SettingsSection({ title, children }: { title?: string; children: ReactNode }) {
  return <View className="gap-2">
    {title ? <Text role="heading" className="text-sm font-medium text-secondary">{title}</Text> : null}
    <View>{children}</View>
  </View>
}
/** Arc alert keeps its surface neutral; the status glyph carries the tone. */
export function AlertPanel({ className, ...props }: ComponentProps<typeof View>) {
  return <View {...props} className={cn('gap-3 rounded-control border border-border bg-surface p-4', className)} />
}
/** Arc agent-approvals uses its own 24px boundary and 20px content inset. */
export function ApprovalPanel({ className, ...props }: ComponentProps<typeof View>) {
  return <View {...props} className={cn('gap-4 rounded-[24px] border border-border bg-surface p-5', className)} />
}
/** Arc empty-state has a plain glyph, a heading and one foreground action. */
export function Empty({ title, glyph, action, className }: { title: string; glyph: ReactNode; action?: ReactNode; className?: string }) {
  const { width } = useWindowDimensions()
  const paddingVertical = Math.min(arcTokens.space[12], Math.max(arcTokens.space[8], width * 0.08))
  return <View style={{ paddingVertical }} className={cn('items-center justify-center px-5', className)}>
    {glyph}
    <Text role="heading" className="mt-5 text-center text-base font-medium">{title}</Text>
    {action ? <View className="mt-5 flex-row flex-wrap justify-center gap-3">{action}</View> : null}
  </View>
}
