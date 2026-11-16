import { WizardStep } from '@/components/arc/wizard'
import { ToastNotice } from '@/components/arc/feedback'
export { Skeleton } from '@/components/arc/feedback'
import * as Clipboard from 'expo-clipboard'
import { Link, type Href } from 'expo-router'
import { Asterisk, Braces, Check, ChevronRight, CircleAlert, CircleCheck, Copy, Info, MousePointer2, Pi, SquareTerminal, TriangleAlert, Wifi, WifiOff, type LucideIcon } from 'lucide-react-native'
import { Children, Fragment, isValidElement, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from 'react'
import { AccessibilityInfo, Pressable, View } from 'react-native'
import Animated, { LayoutAnimationConfig, useAnimatedStyle, useSharedValue, withTiming } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { AlertPanel, ApprovalPanel, Empty, SettingsSection } from '@/components/arc/surface'
import type { ProviderId } from '@vibeface/client/lib/node.ts'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { fadeIn, fadeOut, glide, move } from '@/lib/motion'
import { cn } from '@/lib/utils'

const AnimatedView = withUniwind(Animated.View)

export type Tone = 'neutral' | 'success' | 'warning' | 'danger' | 'info'

export const toneText: Record<Tone, string> = {
  neutral: 'text-muted', success: 'text-success', warning: 'text-warning', danger: 'text-danger', info: 'text-info',
}

/** Shared by questions, join requests and confirmations, as on the web. `trailing` keeps a single action on the title row. */
export function ActionCard({ icon, tone = 'warning', kind = 'alert', title, meta, trailing, children, actions }: {
  icon: LucideIcon; tone?: Tone; kind?: 'alert' | 'approval'; title: ReactNode; meta?: string; trailing?: ReactNode; children?: ReactNode; actions?: ReactNode
}) {
  const Panel = kind === 'approval' ? ApprovalPanel : AlertPanel
  return <Panel>
    <View className="flex-row items-start gap-3">
      <Icon as={icon} className={cn('mt-px size-[18px]', toneText[tone])} />
      <Text role="heading" className={cn('min-w-0 flex-1 font-medium', kind === 'approval' ? 'text-base' : 'text-sm')}>{title}</Text>
      {meta ? <Text numberOfLines={1} className="max-w-[40%] py-1 text-sm text-muted">{meta}</Text> : null}
      {trailing ? <View className="-my-2">{trailing}</View> : null}
    </View>
    {children ? <View className="min-w-0 gap-2">{children}</View> : null}
    {actions ? <View className="flex-row flex-wrap gap-2">{actions}</View> : null}
  </Panel>
}

/** A settings section as on the web: a section heading over rows divided by hairlines. */
export function SettingsGroup({ title, children }: { title?: string; children: ReactNode }) {
  return <SettingsSection title={title}>
    {Children.toArray(children).map((child, index) => <Fragment key={isValidElement(child) ? child.key : index}>
      {index ? <View className="border-t border-subtle" /> : null}{child}
    </Fragment>)}
  </SettingsSection>
}

/**
 * One settings row: a plain icon, the label, then whatever `children` hold at the end. With `href` the
 * whole row opens it, with `onPress` it acts. A child with `basis-full ps-11` takes its own line under the label.
 */
export function SettingsRow({ icon, label, description, tone = 'neutral', href, onPress, disabled, children }: {
  icon: LucideIcon; label: ReactNode; description?: string; tone?: 'neutral' | 'danger'; href?: Href; onPress?: () => void; disabled?: boolean; children?: ReactNode
}) {
  const layout = 'min-h-11 flex-row flex-wrap items-start gap-x-6 gap-y-2 py-[14px]'
  const body = <>
    <Icon as={icon} className={cn('size-5', tone === 'danger' ? 'text-danger' : 'text-secondary')} />
    <View className="min-w-0 flex-1">
      <Text className={cn(tone === 'danger' && 'text-danger', disabled && 'text-muted')}>{label}</Text>
      {description ? <Text className="text-sm text-muted">{description}</Text> : null}
    </View>
    {children}
    {href ? <Icon as={ChevronRight} className="text-muted" /> : null}
  </>
  // A row that only shows things stays a plain view, so a control inside it stays reachable to screen readers.
  if (!href && !onPress) return <View className={layout}>{body}</View>
  const row = <Pressable accessibilityRole={href ? 'link' : 'button'} disabled={disabled} onPress={onPress} className={cn(layout, 'active:bg-surface-muted')}>{body}</Pressable>
  return href ? <Link href={href} asChild>{row}</Link> : row
}

/** A device's reach in words beside an icon, never a bare dot. */
export function Online({ online }: { online: boolean }) {
  return <View className="flex-row items-center gap-1.5">
    <Icon as={online ? Wifi : WifiOff} className={online ? 'text-success' : 'text-muted'} />
    <Text className={cn('text-sm', online ? 'text-success' : 'text-muted')}>{online ? '在线' : '离线'}</Text>
  </View>
}

/** A titled step on a full screen, as the web's <Panel>. */
export function Panel({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <WizardStep icon={icon} title={title}>{children}</WizardStep>
}

/** As on the web: an icon tile, one line of title and one action, fading in. A screen's own empty state centres itself with `flex-1 justify-center`. */
export function EmptyState({ icon, title, action, className }: { icon: LucideIcon; title: string; action?: ReactNode; className?: string }) {
  return <AnimatedView entering={fadeIn} className={className}><Empty title={title} glyph={<Icon as={icon} className="size-6 text-muted" />} action={action} /></AnimatedView>
}

/** Six digits read as two groups of three on a tile, as on the web; the app ships Geist Mono in one weight only. */
export function SafetyCode({ code }: { code: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <View accessible accessibilityLabel={`安全码 ${code.split('').join(' ')}`} className="flex-row flex-wrap justify-center gap-x-4 rounded-panel bg-surface-muted/60 px-4 py-3 ">
    {groups.map((group, index) => <Text key={index} className="font-mono text-2xl tracking-widest" style={{ fontVariant: ['tabular-nums'] }}>{group}</Text>)}
  </View>
}

/** A long id on one line with its middle elided, and a copy button that turns into a check for a moment; the id keeps its width. */
export function CopyableId({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  return <View className="min-h-11 flex-row items-center gap-2 rounded-control bg-surface-muted/60 ps-3">
    <Text numberOfLines={1} ellipsizeMode="middle" className="min-w-0 flex-1 font-mono text-sm">{value}</Text>
    <Button variant="ghost" iconOnly accessibilityLabel={`复制${label}`} onPress={() => {
      void Clipboard.setStringAsync(value)
      setCopied(true)
      AccessibilityInfo.announceForAccessibility('已复制')
      clearTimeout(timer.current)
      timer.current = setTimeout(() => setCopied(false), 1500)
    }}><Icon as={copied ? Check : Copy} className={cn('size-4', copied && 'text-success')} /></Button>
  </View>
}

export const providerIcons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2, claude: Asterisk, pi: Pi, opencode: Braces }

/** Technical detail folded away by default, as the web's <Details>. Only an open by press fades in; one open from the start just shows. */
export function Details({ children, label = '详细信息', open: initial = false, keepMounted = false }: { children: ReactNode; label?: string; open?: boolean; keepMounted?: boolean }) {
  const [open, setOpen] = useState(initial)
  const turn = useSharedValue(initial ? 90 : 0)
  const chevron = useAnimatedStyle(() => ({ transform: [{ rotate: `${turn.value}deg` }] }))
  return <LayoutAnimationConfig skipEntering><View>
    <Button variant="ghost" accessibilityRole="button" accessibilityState={{ expanded: open }} className="min-h-11 flex-row items-center gap-1 self-start" onPress={() => { turn.value = withTiming(open ? 0 : 90, glide); setOpen(!open) }}>
      <Animated.View style={chevron}><Icon as={ChevronRight} className="size-4 text-muted" /></Animated.View><Text className="text-sm text-muted">{label}</Text>
    </Button>
    {open || keepMounted ? <AnimatedView entering={fadeIn} className={cn('gap-2', !open && 'hidden')}>{children}</AnimatedView> : null}
  </View></LayoutAnimationConfig>
}

export function Badge({ count }: { count: number }) {
  if (!count) return null
  return <View accessibilityLabel={`${count} 条未读`} className="min-w-5 items-center rounded-pill bg-accent px-1.5">
    <Text maxFontSizeMultiplier={1.4} className="text-sm leading-5 font-medium text-accent-foreground" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text>
  </View>
}

type ToastItem = { id: number; text: string; tone: Tone }
let toasts: ToastItem[] = []
let toastId = 0
const toastListeners = new Set<() => void>()
const emitToasts = () => { for (const listener of toastListeners) listener() }
const subscribeToasts = (listener: () => void) => { toastListeners.add(listener); return () => { toastListeners.delete(listener) } }
const toastIcon: Record<Tone, LucideIcon> = { neutral: Info, success: CircleCheck, warning: TriangleAlert, danger: CircleAlert, info: Info }

/** A passing word about what just happened, as the web's toast: two at most, gone after 3 s, read out by VoiceOver. Anything to act on goes to 待处理 instead. */
export function toast(text: string, tone: Tone = 'neutral') {
  const item = { id: ++toastId, text, tone }
  toasts = [...toasts, item].slice(-2)
  emitToasts()
  // Queued, so the focus change that usually follows the action does not cut it off.
  AccessibilityInfo.announceForAccessibilityWithOptions(text, { queue: true })
  setTimeout(() => { toasts = toasts.filter((entry) => entry.id !== item.id); emitToasts() }, 3000)
}

/** Mounted once at the root, `bottom` clear of the tab bar. */
export function Toaster({ bottom }: { bottom: number }) {
  const items = useSyncExternalStore(subscribeToasts, () => toasts)
  return <View pointerEvents="none" className="absolute inset-x-0 items-center gap-2 px-4" style={{ bottom }}>
    {items.map((item) => <AnimatedView key={item.id} entering={fadeIn} exiting={fadeOut} layout={move} accessibilityElementsHidden importantForAccessibility="no-hide-descendants"
      className="w-full max-w-sm">
      <ToastNotice>
      <Icon as={toastIcon[item.tone]} className={cn('size-4', toneText[item.tone])} /><Text className="min-w-0 flex-1 text-sm text-foreground">{item.text}</Text>
      </ToastNotice>
    </AnimatedView>)}
  </View>
}
