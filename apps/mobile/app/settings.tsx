import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { ArrowRightLeft, Bell, BellRing, ChevronRight, Crown, FileKey, Globe, KeyRound, Laptop, LogOut, MonitorSmartphone, Palette, Plus, SlidersHorizontal, Smartphone, Sparkles, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { Appearance, Platform, Pressable, ScrollView, useWindowDimensions, View } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { ConnectionPill } from '@/components/connection-pill'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { JoinRequests } from '@/features/devices/join-requests'
import { useWide } from '@/lib/layout'
import { cn } from '@/lib/utils'
import { useRun } from '@/lib/use-run'

const themes = [{ value: 'system', label: '跟随系统' }, { value: 'light', label: '浅色' }, { value: 'dark', label: '深色' }] as const

export default function Settings() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { theme, hasAdaptiveThemes } = useUniwind()
  const current = hasAdaptiveThemes ? 'system' : theme
  const [leaving, setLeaving] = useState(false)
  const wide = useWide()
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(() => {}) }, [device.ready])
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-8 p-4 pb-10">
    <Section icon={Palette} title="外观">
      {/* A theme change repaints the whole app, which stalls any slide under it; like iOS's own appearance picker, it applies at once.
          UIKit gets the same style, so native glass like the back button turns with the page instead of re-sampling it later. */}
      <Segmented snap options={themes} value={current} onChange={(value) => { Appearance.setColorScheme(value === 'system' ? 'unspecified' : value); Uniwind.setTheme(value) }} />
    </Section>
    {/* Beside the sidebar its footer already shows the connection. */}
    {wide ? null : <Section icon={Globe} title="连接">
      <View className="-mx-2 -my-3"><ConnectionPill connection={workspace.connection} /></View>
    </Section>}
    <Section icon={MonitorSmartphone} title="我的设备">
      <JoinRequests />
      <View className="overflow-hidden rounded-xl border border-border">
        {workspace.members.map((member, index) => <MemberRow key={member.device_id} member={member} first={index === 0} />)}
      </View>
      {device.ready?.principal.can_manage ? <Link href="/add-device" asChild>
        <Button variant="outline" className="min-h-11 self-start"><Icon as={Plus} /><Text>添加设备</Text></Button>
      </Link> : null}
    </Section>
    {device.ready ? <Notifications /> : null}
    <Section icon={ArrowRightLeft} title="文件传输">
      <LinkRow href="/transfers" icon={ArrowRightLeft} label="发送和接收文件" />
    </Section>
    {device.ready?.principal.can_manage ? <Section icon={Sparkles} title="AI">
      <LinkRow href="/ai" icon={SlidersHorizontal} label="Bot 与任务" />
    </Section> : null}
    <Section icon={FileKey} title="恢复文件">
      <LinkRow href="/recovery" icon={KeyRound} label="换恢复密码" />
    </Section>
    <View className="border-t border-border pt-6">
      {leaving ? <View className="gap-3">
        <Text>断开后，要用加入链接才能回来。</Text>
        <View className="flex-row gap-2">
          <Button variant="ghost" className="min-h-11" onPress={() => setLeaving(false)}><Text>取消</Text></Button>
          <Button variant="destructive" className="min-h-11" onPress={workspace.actions.disconnect}><Icon as={LogOut} className="text-white" /><Text>断开</Text></Button>
        </View>
      </View> : <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={() => { void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning); setLeaving(true) }}>
        <Icon as={LogOut} className="text-muted-foreground" /><Text className="text-muted-foreground">断开这台手机</Text>
      </Button>}
    </View>
  </ScrollView>
}

/** Rendered only while connected: the shared workspace exists exactly then. */
function Notifications() {
  const shared = useShared()
  const { busy, error, run } = useRun()
  const button = shared.notificationButton
  return <Section icon={Bell} title="通知">
    <Button variant="outline" className="min-h-11 self-start" disabled={button.disabled || busy !== null} onPress={() => void run('notify', shared.enableNotifications)}>
      <Icon as={BellRing} /><Text>{button.label}</Text>
    </Button>
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
  </Section>
}

function LinkRow({ href, icon, label }: { href: '/ai' | '/recovery' | '/transfers'; icon: LucideIcon; label: string }) {
  return <Link href={href} asChild>
    <Pressable className="min-h-12 flex-row items-center gap-3 overflow-hidden rounded-xl border border-border px-4 py-2 active:bg-muted">
      <Icon as={icon} className="text-muted-foreground" />
      <Text className="min-w-0 flex-1 font-medium">{label}</Text>
      <Icon as={ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
  </Link>
}

function Section({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" className="min-w-0 flex-1 text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}

const kindIcon = (member: Member) => member.kind === 'client' ? Smartphone : Laptop

function MemberRow({ member, first }: { member: Member; first: boolean }) {
  // iOS sits inline views on the line box, not the glyph baseline, so they sink as the line height grows with Dynamic Type.
  // Measured: 1.5pt at the default size, about 11.5pt at the 2x cap. Android centres it on the line, about 1dp high at every size.
  const fontScale = useWindowDimensions().fontScale
  const crownLift = Platform.OS === 'ios' ? -(1.5 + 10 * (Math.min(fontScale, 2) - 1)) : 1
  return <Link href={`/device/${member.device_id}`} asChild>
    <Pressable className={cn('min-h-14 flex-row items-center gap-3 px-4 py-2 active:bg-muted', !first && 'border-t border-border')}>
      <Icon as={kindIcon(member)} className="text-muted-foreground" />
      {/* Title and subtitle, as a system cell. The name wraps in full so the inline crown is never cut off with it. */}
      <View className="min-w-0 flex-1">
        <Text className="font-medium">{member.name}{member.admin ? <>{'\u00a0'}<Icon as={Crown} accessibilityLabel="管理设备" className="size-3.5 text-muted-foreground" style={{ transform: [{ translateY: crownLift }] }} /></> : null}</Text>
        {member.current || member.entry ? <Text className="text-meta text-muted-foreground">{member.current ? '这台手机' : '当前连接'}</Text> : null}
      </View>
      <View accessibilityLabel={member.online ? '在线' : '离线'} className={cn('size-2 rounded-full', member.online ? 'bg-success' : 'bg-muted-foreground/40')} />
      <Icon as={ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
  </Link>
}
