import * as Clipboard from 'expo-clipboard'
import { Link, type Href } from 'expo-router'
import { Asterisk, Braces, Check, ChevronDown, ChevronRight, Copy, MousePointer2, Pi, SquareTerminal, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { Children, Fragment, isValidElement, useEffect, useRef, useState, type ReactNode } from 'react'
import { AccessibilityInfo, Pressable, View } from 'react-native'
import Animated, { useReducedMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { oklch } from '@vibeface/client/avatar/face.ts'
import type { ProviderId } from '@vibeface/client/lib/node.ts'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { fadeIn } from '@/lib/motion'
import { cn } from '@/lib/utils'

const AnimatedView = withUniwind(Animated.View)

export type Tone = 'neutral' | 'success' | 'warning' | 'danger' | 'info'

export const toneText: Record<Tone, string> = {
  neutral: 'text-muted-foreground', success: 'text-success', warning: 'text-warning', danger: 'text-danger', info: 'text-info',
}
const toneSurface: Record<Tone, string> = {
  neutral: 'border-border bg-card dark:border-line-strong', success: 'border-success-border bg-success-soft', warning: 'border-warning-border bg-warning-soft',
  danger: 'border-danger-border bg-danger-soft', info: 'border-info-border bg-info-soft',
}
const toneChip: Record<Tone, string> = {
  neutral: 'bg-muted', success: 'bg-success-soft ring-1 ring-success-border', warning: 'bg-warning-soft ring-1 ring-warning-border',
  danger: 'bg-danger-soft ring-1 ring-danger-border', info: 'bg-info-soft ring-1 ring-info-border',
}

/** Shared by questions, join requests and confirmations, as on the web. `trailing` keeps a single action on the title row. */
export function ActionCard({ icon, tone = 'warning', title, meta, trailing, children, actions }: {
  icon: LucideIcon; tone?: Tone; title: ReactNode; meta?: string; trailing?: ReactNode; children?: ReactNode; actions?: ReactNode
}) {
  return <View className={cn('gap-3 rounded-xl border p-4 shadow-xs', toneSurface[tone])}>
    {/* Top-aligned so a long title wraps under itself; the padding centres its first line on the 28px tile. */}
    <View className="flex-row items-start gap-3">
      <View className={cn('size-7 items-center justify-center rounded-lg', toneChip[tone])}><Icon as={icon} className={toneText[tone]} /></View>
      <Text role="heading" className="min-w-0 flex-1 py-0.5 text-heading font-semibold">{title}</Text>
      {meta ? <Text numberOfLines={1} className="max-w-[40%] py-1 text-meta text-muted-foreground">{meta}</Text> : null}
      {trailing ? <View className="-my-2">{trailing}</View> : null}
    </View>
    {children ? <View className="min-w-0 gap-2">{children}</View> : null}
    {actions ? <View className="flex-row flex-wrap justify-end gap-2">{actions}</View> : null}
  </View>
}

/** A settings section as on the web: a short caption over one card of rows, divided by hairlines. */
export function SettingsGroup({ title, children }: { title?: string; children: ReactNode }) {
  return <View className="gap-2">
    {title ? <Text role="heading" className="px-1 text-meta font-medium text-muted-foreground">{title}</Text> : null}
    <View className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">
      {Children.toArray(children).map((child, index) => <Fragment key={isValidElement(child) ? child.key : index}>
        {index ? <View className="h-px bg-border" /> : null}{child}
      </Fragment>)}
    </View>
  </View>
}

/**
 * One settings row as on the web: an icon tile, the label, then whatever `children` hold at the end. With `href` the
 * whole row opens it, with `onPress` it acts. A child with `basis-full ps-11` takes its own line under the label.
 */
export function SettingsRow({ icon, label, description, tone = 'neutral', href, onPress, disabled, children }: {
  icon: LucideIcon; label: ReactNode; description?: string; tone?: 'neutral' | 'danger'; href?: Href; onPress?: () => void; disabled?: boolean; children?: ReactNode
}) {
  const layout = 'min-h-14 flex-row flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5'
  const body = <>
    <View className={cn('size-8 items-center justify-center rounded-lg', tone === 'danger' ? 'bg-danger-soft' : 'bg-muted')}>
      <Icon as={icon} className={tone === 'danger' ? 'text-danger' : 'text-muted-foreground'} />
    </View>
    <View className="min-w-0 flex-1">
      <Text className={cn(tone === 'danger' && 'text-danger', disabled && 'text-muted-foreground')}>{label}</Text>
      {description ? <Text className="text-meta text-muted-foreground">{description}</Text> : null}
    </View>
    {children}
    {href ? <Icon as={ChevronRight} className="text-muted-foreground" /> : null}
  </>
  // A row that only shows things stays a plain view, so a control inside it stays reachable to screen readers.
  if (!href && !onPress) return <View className={layout}>{body}</View>
  const row = <Pressable accessibilityRole={href ? 'link' : 'button'} disabled={disabled} onPress={onPress} className={cn(layout, 'active:bg-muted')}>{body}</Pressable>
  return href ? <Link href={href} asChild>{row}</Link> : row
}

/** A device's reach in words beside an icon, never a bare dot. */
export function Online({ online }: { online: boolean }) {
  return <View className="flex-row items-center gap-1.5">
    <Icon as={online ? Wifi : WifiOff} className={online ? 'text-success' : 'text-muted-foreground'} />
    <Text className={cn('text-meta', online ? 'text-success' : 'text-muted-foreground')}>{online ? '在线' : '离线'}</Text>
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

/** As on the web: an icon tile, one line of title and one action, fading in. A screen's own empty state centres itself with `flex-1 justify-center`. */
export function EmptyState({ icon, title, action, className }: { icon: LucideIcon; title: string; action?: ReactNode; className?: string }) {
  return <AnimatedView entering={fadeIn} className={cn('items-center gap-3 px-4 py-12', className)}>
    <View className="mb-1 size-16 items-center justify-center rounded-2xl bg-muted"><Icon as={icon} className="size-8 text-muted-foreground" /></View>
    <Text role="heading" className="text-center text-heading font-semibold">{title}</Text>
    {action ? <View className="pt-1">{action}</View> : null}
  </AnimatedView>
}

const breathe = { from: { opacity: 1 }, '50%': { opacity: 0.55 }, to: { opacity: 1 } }
const hold = { from: { opacity: 1 }, to: { opacity: 1 } }

/** Stands in for content still loading. Hidden for 300ms so a quick load never flickers, then it breathes; with Reduce Motion it holds still. */
export function Skeleton({ className }: { className?: string }) {
  const reduced = useReducedMotion()
  return <AnimatedView className={cn('rounded-lg bg-muted', className)} style={{ opacity: 0, animationName: reduced ? hold : breathe, animationDuration: '1.6s', animationDelay: '300ms', animationIterationCount: 'infinite', animationTimingFunction: 'ease-in-out' }} />
}

/** Six digits read as two groups of three on a tile, as on the web; the app ships Geist Mono in one weight only. */
export function SafetyCode({ code }: { code: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <View accessible accessibilityLabel={`安全码 ${code.split('').join(' ')}`} className="flex-row flex-wrap justify-center gap-x-4 rounded-xl bg-muted/60 px-4 py-3 ring-1 ring-border">
    {groups.map((group, index) => <Text key={index} className="font-mono text-2xl tracking-widest" style={{ fontVariant: ['tabular-nums'] }}>{group}</Text>)}
  </View>
}

/** A long id on one line with its middle elided, and a copy button that turns into a check for a moment; the id keeps its width. */
export function CopyableId({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  return <View className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
    <Text numberOfLines={1} ellipsizeMode="middle" className="min-w-0 flex-1 font-mono text-meta">{value}</Text>
    <Button variant="ghost" size="icon" accessibilityLabel={`复制${label}`} onPress={() => {
      void Clipboard.setStringAsync(value)
      setCopied(true)
      AccessibilityInfo.announceForAccessibility('已复制')
      clearTimeout(timer.current)
      timer.current = setTimeout(() => setCopied(false), 1500)
    }}><Icon as={copied ? Check : Copy} className={cn('size-4', copied && 'text-success')} /></Button>
  </View>
}

export const providerIcons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2, claude: Asterisk, pi: Pi, opencode: Braces }

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

export function Badge({ count, fill, ink }: { count: number; fill?: string; ink?: string }) {
  if (!count) return null
  return <View accessibilityLabel={`${count} 条未读`} className="min-w-5 items-center rounded-full bg-brand px-1.5" style={fill ? { backgroundColor: fill } : undefined}>
    <Text maxFontSizeMultiplier={1.4} className="text-[11px] leading-5 font-semibold text-brand-foreground" style={[{ fontVariant: ['tabular-nums'] }, ink ? { color: ink } : null]}>{count}</Text>
  </View>
}

/**
 * A bot's face hue as a solid fill with legible ink on it, and a soft edge for outlines, so its chat's accents read as that bot's.
 * `glow` lights the halo behind its face; the darker light-mode fill would fade to grey there.
 */
export function botTint(hue: number, dark: boolean) {
  const glow = oklch(0.74, 0.14, hue)
  return dark
    ? { fill: oklch(0.78, 0.09, hue), ink: oklch(0.2, 0.03, hue), edge: oklch(0.42, 0.05, hue), glow }
    : { fill: oklch(0.45, 0.1, hue), ink: '#fff', edge: oklch(0.86, 0.04, hue), glow }
}
