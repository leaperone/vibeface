import * as Clipboard from 'expo-clipboard'
import { Check, ChevronDown, ChevronRight, Copy, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AccessibilityInfo, Pressable, View } from 'react-native'
import { oklch } from '@vibeface/client/avatar/face.ts'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { cn } from '@/lib/utils'

export type Tone = 'neutral' | 'success' | 'warning' | 'danger' | 'info'

export const toneText: Record<Tone, string> = {
  neutral: 'text-muted-foreground', success: 'text-success', warning: 'text-warning', danger: 'text-danger', info: 'text-info',
}
const toneSurface: Record<Tone, string> = {
  neutral: 'border-border bg-card', success: 'border-success-border bg-success-soft', warning: 'border-warning-border bg-card',
  danger: 'border-danger-border bg-danger-soft', info: 'border-info-border bg-info-soft',
}

/** Shared by questions, join requests and confirmations, as on the web. */
export function ActionCard({ icon, tone = 'warning', title, children, actions }: {
  icon: LucideIcon; tone?: Tone; title: ReactNode; children?: ReactNode; actions?: ReactNode
}) {
  return <View className={cn('gap-3 rounded-xl border p-4', toneSurface[tone])}>
    <View className="flex-row items-start gap-2">
      <Icon as={icon} className={cn('mt-0.5 size-5', toneText[tone])} />
      <Text role="heading" className="min-w-0 flex-1 text-heading font-semibold">{title}</Text>
    </View>
    {children ? <View className="min-w-0 gap-2">{children}</View> : null}
    {actions ? <View className="flex-row flex-wrap justify-end gap-2">{actions}</View> : null}
  </View>
}

/** A titled step on a full screen, as the web's <Panel>. */
export function Panel({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-4">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} className="text-muted-foreground" />
      <Text role="heading" className="text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}

export function EmptyState({ icon, title, action }: { icon: LucideIcon; title: string; action?: ReactNode }) {
  return <View className="items-center gap-3 px-4 py-12">
    <Icon as={icon} className="size-8 text-muted-foreground" />
    <Text role="heading" className="text-heading font-semibold">{title}</Text>
    {action}
  </View>
}

/** Six digits read as two groups of three, as on the web. */
export function SafetyCode({ code }: { code: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <Text accessibilityLabel={`安全码 ${code.split('').join(' ')}`} className="font-mono text-2xl tracking-widest" style={{ fontVariant: ['tabular-nums'] }}>{groups.join(' ')}</Text>
}

/** A long id on one line with its middle elided, and a copy button that turns into a check for a moment; the id keeps its width. */
export function CopyableId({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  return <View className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
    <Text numberOfLines={1} ellipsizeMode="middle" className="min-w-0 flex-1 font-mono text-meta">{value}</Text>
    <Button variant="ghost" size="icon" className="size-11" accessibilityLabel={`复制${label}`} onPress={() => {
      void Clipboard.setStringAsync(value)
      setCopied(true)
      AccessibilityInfo.announceForAccessibility('已复制')
      clearTimeout(timer.current)
      timer.current = setTimeout(() => setCopied(false), 1500)
    }}><Icon as={copied ? Check : Copy} className={cn('size-4', copied && 'text-success')} /></Button>
  </View>
}

/** Technical detail folded away by default, as the web's <Details>. */
export function Details({ children, label = '详细信息', open: initial = false }: { children: ReactNode; label?: string; open?: boolean }) {
  const [open, setOpen] = useState(initial)
  return <View>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: open }} className="min-h-11 flex-row items-center gap-1 self-start" onPress={() => setOpen(!open)}>
      <Icon as={open ? ChevronDown : ChevronRight} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">{label}</Text>
    </Pressable>
    {open ? <View className="gap-2">{children}</View> : null}
  </View>
}

export function Badge({ count }: { count: number }) {
  if (!count) return null
  return <View accessibilityLabel={`${count} 条未读`} className="min-w-5 items-center rounded-full bg-foreground px-1.5">
    <Text maxFontSizeMultiplier={1.4} className="text-[11px] leading-5 font-semibold text-background" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text>
  </View>
}

/** A bot's face hue as a solid fill with legible ink on it, and a soft edge for outlines, so its chat's accents read as that bot's. */
export function botTint(hue: number, dark: boolean) {
  return dark
    ? { fill: oklch(0.78, 0.09, hue), ink: oklch(0.2, 0.03, hue), edge: oklch(0.42, 0.05, hue) }
    : { fill: oklch(0.45, 0.1, hue), ink: '#fff', edge: oklch(0.86, 0.04, hue) }
}
