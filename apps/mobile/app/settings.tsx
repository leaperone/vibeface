import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { ArrowRightLeft, BellRing, ChevronRight, Crown, KeyRound, LogOut, MonitorSmartphone, Palette, Plus, Sparkles, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { Appearance, Platform, Pressable, ScrollView, useWindowDimensions, View } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { deviceIcon } from '@/features/devices/device-icon'
import { JoinRequests } from '@/features/devices/join-requests'
import { cn } from '@/lib/utils'
import { useRun } from '@/lib/use-run'

const themes = [{ value: 'system', label: '跟随系统' }, { value: 'light', label: '浅色' }, { value: 'dark', label: '深色' }] as const

export default function Settings() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { theme, hasAdaptiveThemes } = useUniwind()
  const current = hasAdaptiveThemes ? 'system' : theme
  const [leaving, setLeaving] = useState(false)
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(() => {}) }, [device.ready])
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-8 p-4 pb-10">
    {/* The home list already shows the connection, so settings starts with the devices. */}
    <Section icon={MonitorSmartphone} title="我的设备">
      <JoinRequests />
      <View className="overflow-hidden rounded-xl border border-border">
        {workspace.members.map((member, index) => <MemberRow key={member.device_id} member={member} first={index === 0} />)}
        {device.ready?.principal.can_manage ? <LinkRow href="/add-device" icon={Plus} label="添加设备" /> : null}
      </View>
    </Section>
    {/* One card of places to go, as a system settings group: each row names itself, so no header per row. */}
    <View className="overflow-hidden rounded-xl border border-border">
      {device.ready?.principal.can_manage ? <LinkRow first href="/ai" icon={Sparkles} label="Bot 与任务" /> : null}
      <LinkRow first={!device.ready?.principal.can_manage} href="/transfers" icon={ArrowRightLeft} label="发送和接收文件" />
      <LinkRow href="/recovery" icon={KeyRound} label="换恢复密码" />
      {device.ready ? <Notifications /> : null}
    </View>
    <Section icon={Palette} title="外观">
      {/* A theme change repaints the whole app, which stalls any slide under it; like iOS's own appearance picker, it applies at once.
          UIKit gets the same style, so native glass like the back button turns with the page instead of re-sampling it later. */}
      <Segmented snap options={themes} value={current} onChange={(value) => { Appearance.setColorScheme(value === 'system' ? 'unspecified' : value); Uniwind.setTheme(value) }} />
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
  return <Pressable accessibilityRole="button" accessibilityState={{ disabled: button.disabled || busy !== null }} disabled={button.disabled || busy !== null} onPress={() => void run('notify', shared.enableNotifications)}
    className="min-h-12 flex-row items-center gap-3 border-t border-border px-4 py-2 active:bg-muted">
    <Icon as={BellRing} className="text-muted-foreground" />
    <View className="min-w-0 flex-1">
      <Text className={cn('font-medium', button.disabled && 'text-muted-foreground')}>{button.label}</Text>
      {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
    </View>
  </Pressable>
}

function LinkRow({ href, icon, label, first }: { href: '/ai' | '/recovery' | '/transfers' | '/add-device'; icon: LucideIcon; label: string; first?: boolean }) {
  return <Link href={href} asChild>
    <Pressable className={cn('min-h-12 flex-row items-center gap-3 px-4 py-2 active:bg-muted', !first && 'border-t border-border')}>
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

function MemberRow({ member, first }: { member: Member; first: boolean }) {
  // iOS sits inline views on the line box, not the glyph baseline, so they sink as the line height grows with Dynamic Type.
  // Measured: 1.5pt at the default size, about 11.5pt at the 2x cap. Android centres it on the line, about 1dp high at every size.
  const fontScale = useWindowDimensions().fontScale
  const crownLift = Platform.OS === 'ios' ? -(1.5 + 10 * (Math.min(fontScale, 2) - 1)) : 1
  return <Link href={`/device/${member.device_id}`} asChild>
    <Pressable className={cn('min-h-14 flex-row items-center gap-3 px-4 py-2 active:bg-muted', !first && 'border-t border-border')}>
      <Icon as={deviceIcon(member)} className="text-muted-foreground" />
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
