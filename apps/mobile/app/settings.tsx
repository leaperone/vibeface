import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { ArrowRightLeft, BellRing, Crown, KeyRound, LogOut, Monitor, Moon, Palette, Plus, RotateCcw, Sparkles, Sun, WifiOff } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { Appearance, Platform, ScrollView, useWindowDimensions, View, type ScrollViewInstance } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { onlyManagerText, soleManager } from '@vibeface/client/identity/copy.ts'
import { useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { ErrorText } from '@/components/action'
import { Online, SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { deviceIcon } from '@/features/devices/device-icon'
import { JoinRequests } from '@/features/devices/join-requests'
import { self } from '@/platform'
import { useRun } from '@/lib/use-run'

const themes = [{ value: 'system', label: '跟随系统', icon: Monitor }, { value: 'light', label: '浅色', icon: Sun }, { value: 'dark', label: '深色', icon: Moon }] as const

export default function Settings() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { theme, hasAdaptiveThemes } = useUniwind()
  const current = hasAdaptiveThemes ? 'system' : theme
  const [confirming, setConfirming] = useState<'leave' | 'reset' | null>(null)
  const { busy, error, run } = useRun()
  const scroll = useRef<ScrollViewInstance>(null)
  const reveal = useRef(false)
  const sole = soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  const otherClient = device.directory?.members.some((member) => member.kind === 'client' && member.device_id !== device.identity?.deviceId)
  const manager = device.ready?.principal.can_manage === true
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(() => {}) }, [device.ready])
  const open = (which: 'leave' | 'reset') => { void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning); reveal.current = true; setConfirming(which) }
  const reset = async () => { await device.factoryReset(); Appearance.setColorScheme('unspecified'); Uniwind.setTheme('system') }
  // The confirmation opens at the foot of the page, below the fold on a phone.
  return <ScrollView ref={scroll} onContentSizeChange={() => { if (reveal.current) { reveal.current = false; scroll.current?.scrollToEnd() } }} contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {/* The home header already shows the connection, so settings starts with the devices. */}
    <JoinRequests />
    {workspace.members.length || manager ? <SettingsGroup title="我的设备">
      {workspace.members.map((member) => <MemberRow key={member.device_id} member={member} />)}
      {manager ? <SettingsRow href="/add-device" icon={Plus} label="添加设备" /> : null}
    </SettingsGroup> : null}
    {/* One card of places to go, as a system settings group: each row names itself. */}
    <SettingsGroup>
      {manager ? <SettingsRow href="/ai" icon={Sparkles} label="Bot 与任务" /> : null}
      <SettingsRow href="/transfers" icon={ArrowRightLeft} label="发送和接收文件" />
      <SettingsRow href="/recovery" icon={KeyRound} label="换恢复密码" />
      {device.ready ? <Notifications /> : null}
    </SettingsGroup>
    <SettingsGroup title="外观">
      <SettingsRow icon={Palette} label="主题">
        {/* A theme change repaints the whole app, which stalls any slide under it; like iOS's own appearance picker, it applies at once.
            UIKit gets the same style, so native glass like the back button turns with the page instead of re-sampling it later. */}
        <View className="basis-full ps-11">
          <Segmented snap options={themes} value={current} onChange={(value) => { Appearance.setColorScheme(value === 'system' ? 'unspecified' : value); Uniwind.setTheme(value) }} />
        </View>
      </SettingsRow>
    </SettingsGroup>
    <View className="gap-3">
      <SettingsGroup>
        <SettingsRow icon={LogOut} tone="danger" label={`断开${self}`} onPress={() => open('leave')} />
        <SettingsRow icon={RotateCcw} tone="danger" label="恢复出厂设置" onPress={() => open('reset')} />
      </SettingsGroup>
      {confirming === 'leave' && !device.ready ? <View className="gap-3 px-1">
        {/* Leaving is signed by the network, so it waits for a connection; an erase does not. */}
        <View className="flex-row items-start gap-2"><Icon as={WifiOff} className="mt-1 text-muted-foreground" /><Text className="min-w-0 flex-1">连上网络才能断开。连不上的话，可以恢复出厂设置。</Text></View>
        <Button variant="ghost" className="-ms-3 self-start" onPress={() => setConfirming(null)}><Text>知道了</Text></Button>
      </View> : confirming === 'leave' && sole ? <View className="gap-3 px-1">
        <View className="flex-row items-start gap-2"><Icon as={Crown} className="mt-1 text-warning" /><Text className="min-w-0 flex-1">{onlyManagerText(self)}</Text></View>
        <View className="flex-row gap-2">
          <Button variant="ghost" className="-ms-3" onPress={() => setConfirming(null)}><Text>知道了</Text></Button>
          {/* With no other device to promote, the next manager has to join first. */}
          {otherClient ? null : <Link href="/add-device" asChild><Button><Icon as={Plus} /><Text>添加设备</Text></Button></Link>}
        </View>
      </View> : confirming === 'leave' ? <View className="gap-3 px-1">
        <Text>断开后，要用加入链接才能回来。</Text>
        <ErrorText error={error} />
        <View className="flex-row justify-end gap-2">
          <Button variant="ghost" onPress={() => setConfirming(null)}><Text>取消</Text></Button>
          <Button variant="destructive" disabled={busy !== null} onPress={() => void run('leave', workspace.actions.leave)}><Icon as={LogOut} /><Text>断开</Text></Button>
        </View>
      </View> : confirming === 'reset' ? <View className="gap-3 px-1">
        {sole ? <View className="flex-row items-start gap-2"><Icon as={Crown} className="mt-1 text-warning" /><Text className="min-w-0 flex-1">{`${self}是唯一的管理设备，清除后这个网络就没有管理设备了。`}</Text></View> : null}
        <Text>{`清除${self}上的所有 Vibeface 数据，回到欢迎页。`}</Text>
        <ErrorText error={error} />
        <View className="flex-row justify-end gap-2">
          <Button variant="ghost" onPress={() => setConfirming(null)}><Text>取消</Text></Button>
          <Button variant="destructive" disabled={busy !== null} onPress={() => void run('reset', reset)}><Icon as={RotateCcw} /><Text>清除</Text></Button>
        </View>
      </View> : null}
    </View>
  </ScrollView>
}

/** Rendered only while connected: the shared workspace exists exactly then. */
function Notifications() {
  const shared = useShared()
  const { busy, error, run } = useRun()
  const button = shared.notificationButton
  return <SettingsRow icon={BellRing} label={button.label} disabled={button.disabled || busy !== null} onPress={() => void run('notify', shared.enableNotifications)}>
    {error ? <View className="basis-full ps-11"><ErrorText error={error} /></View> : null}
  </SettingsRow>
}

function MemberRow({ member }: { member: Member }) {
  // iOS sits inline views on the line box, not the glyph baseline, so they sink as the line height grows with Dynamic Type.
  // Measured: 1.5pt at the default size, about 11.5pt at the 2x cap. Android centres it on the line, about 1dp high at every size.
  const fontScale = useWindowDimensions().fontScale
  const crownLift = Platform.OS === 'ios' ? -(1.5 + 10 * (Math.min(fontScale, 2) - 1)) : 1
  // The name wraps in full so the inline crown is never cut off with it.
  return <SettingsRow href={`/device/${member.device_id}`} icon={deviceIcon(member)}
    label={<>{member.name}{member.admin ? <>{'\u00a0'}<Icon as={Crown} accessibilityLabel="管理设备" className="size-3.5 text-muted-foreground" style={{ transform: [{ translateY: crownLift }] }} /></> : null}</>}
    description={member.current ? self : member.entry ? '当前连接' : undefined}>
    {member.online !== undefined ? <Online online={member.online} /> : null}
  </SettingsRow>
}
