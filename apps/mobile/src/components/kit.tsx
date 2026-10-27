import * as Clipboard from 'expo-clipboard'
import { Check, ChevronDown, ChevronRight, Copy, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AccessibilityInfo, Pressable, View } from 'react-native'
import { useUniwind } from 'uniwind'
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
export function Details({ children, label = '详细信息' }: { children: ReactNode; label?: string }) {
  const [open, setOpen] = useState(false)
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

// Hand-spaced so no two read alike; even 45° steps put two pinks next to each other.
const hues = [8, 32, 52, 100, 160, 200, 235, 280]

/** A bot's own hue, shared by its monogram and its chat's accents so each conversation reads as that bot's. */
export function botHue(seed: string) {
  // FNV-1a: a plain `* 31` hash leaves `% 8` set by the last characters, so similar ids shared a tint.
  let hash = 2166136261
  for (const char of seed) hash = Math.imul(hash ^ char.charCodeAt(0), 16777619)
  hash = (hash ^ (hash >>> 16)) >>> 0
  return hues[hash % hues.length]
}

/** The bot's hue as a solid fill with legible ink on it, and a soft edge for outlines. */
export function botTint(hue: number, dark: boolean) {
  return dark
    ? { fill: `hsl(${hue}, 40%, 72%)`, ink: `hsl(${hue}, 30%, 12%)`, edge: `hsl(${hue}, 25%, 34%)` }
    : { fill: `hsl(${hue}, 45%, 34%)`, ink: '#fff', edge: `hsl(${hue}, 35%, 80%)` }
}

/** A bot's first letter on a soft tint picked from its id, so each bot is recognisable at a glance. */
export function Monogram({ name, seed, hero }: { name: string; seed: string; hero?: boolean }) {
  const { theme } = useUniwind()
  const hue = botHue(seed)
  const dark = theme === 'dark'
  return <View importantForAccessibility="no-hide-descendants" accessibilityElementsHidden className={cn('items-center justify-center rounded-full', hero ? 'size-20' : 'size-11')} style={{ backgroundColor: `hsl(${hue}, ${dark ? 22 : 40}%, ${dark ? 24 : 90}%)` }}>
    <Text maxFontSizeMultiplier={1} className={cn('font-semibold', hero ? 'text-3xl' : 'text-heading')} style={{ color: `hsl(${hue}, ${dark ? 45 : 35}%, ${dark ? 82 : 32}%)` }}>{Array.from(name.trim())[0]?.toUpperCase() ?? '?'}</Text>
  </View>
}
