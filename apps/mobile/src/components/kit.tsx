import type { LucideIcon } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { View } from 'react-native'
import { useUniwind } from 'uniwind'
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

export function Badge({ count }: { count: number }) {
  if (!count) return null
  return <View accessibilityLabel={`${count} 条未读`} className="min-w-5 items-center rounded-full bg-foreground px-1.5">
    <Text maxFontSizeMultiplier={1.4} className="text-[11px] leading-5 font-semibold text-background" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text>
  </View>
}

/** A bot's first letter on a soft tint picked from its id, so each bot is recognisable at a glance. */
export function Monogram({ name, seed }: { name: string; seed: string }) {
  const { theme } = useUniwind()
  // FNV-1a: a plain `* 31` hash leaves `% 8` set by the last characters, so similar ids shared a tint.
  let hash = 2166136261
  for (const char of seed) hash = Math.imul(hash ^ char.charCodeAt(0), 16777619)
  hash = (hash ^ (hash >>> 16)) >>> 0
  // Eight hues 45° apart, so neighbouring bots never land on near-identical tints.
  const hue = (hash % 8) * 45
  const dark = theme === 'dark'
  return <View importantForAccessibility="no-hide-descendants" accessibilityElementsHidden className="size-11 items-center justify-center rounded-full" style={{ backgroundColor: `hsl(${hue}, ${dark ? 22 : 40}%, ${dark ? 24 : 90}%)` }}>
    <Text maxFontSizeMultiplier={1} className="text-heading font-semibold" style={{ color: `hsl(${hue}, ${dark ? 45 : 35}%, ${dark ? 82 : 32}%)` }}>{Array.from(name.trim())[0]?.toUpperCase() ?? '?'}</Text>
  </View>
}
