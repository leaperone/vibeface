import * as Haptics from 'expo-haptics'
import { Link } from 'expo-router'
import { ArrowRightLeft, BellRing, Bot, ChevronUp, Crown, FileKey, FlaskConical, KeyRound, LogOut, MessageSquareShare, Monitor, Moon, Palette, Plus, RotateCcw, ShieldAlert, ShieldCheck, Sun, TriangleAlert, WifiOff, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { Appearance, Platform, ScrollView, useWindowDimensions, View, type ScrollViewInstance } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { useDevice, useRecoveryCode } from '@vibeface/client/identity/context.tsx'
import { addOnManager, eraseSoleManagerText, exitCopy, onlyManagerText, recoveryCopy, securityCopy, soleManager, waitingCopy } from '@vibeface/client/identity/copy.ts'
import { experimentsCopy, useExperiments } from '@vibeface/client/lib/experiments.ts'
import { useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { ErrorText } from '@/components/action'
import { OfflineBar } from '@/components/connection-pill'
import { Online, SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Segmented } from '@/components/ui/segmented'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { deviceIcon } from '@/features/devices/device-icon'
import { JoinRequests } from '@/features/devices/join-requests'
import { mobilePlatform, self } from '@/platform'
import { factoryReset } from '@/lib/factory-reset'
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
  const devicesAt = useRef(0)
  const sole = soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  const otherClient = device.directory?.members.some((member) => member.kind === 'client' && member.device_id !== device.identity?.deviceId)
  const manager = device.ready?.principal.can_manage === true
  const [experiments, setExperiments] = useExperiments(mobilePlatform.storage)
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(() => {}) }, [device.ready])
  const open = (which: 'leave' | 'reset') => { void Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning); reveal.current = true; setConfirming(which) }
  const reset = () => factoryReset(device)
  // The confirmation opens at the foot of the page, below the fold on a phone.
  return <ScrollView ref={scroll} onContentSizeChange={() => { if (reveal.current) { reveal.current = false; scroll.current?.scrollToEnd() } }} contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {/* The home header already shows the connection; only a computer that does not answer is called out here, above the devices. */}
    <OfflineBar />
    <JoinRequests />
    {workspace.members.length || manager ? <View onLayout={(event) => { devicesAt.current = event.nativeEvent.layout.y }}><SettingsGroup title="我的设备">
      {workspace.members.map((member) => <MemberRow key={member.device_id} member={member} />)}
      {manager ? <SettingsRow href="/add-device" icon={Plus} label="添加设备" />
        : device.ready ? <SettingsRow disabled icon={Crown} label={addOnManager(workspace.members.filter((member) => member.admin).map((member) => member.name))} /> : null}
    </SettingsGroup></View> : null}
    {/* As on the web: a computer's bots live in its detail above, and a session that waits for you reaches you here. */}
    {device.ready ? <SettingsGroup title={waitingCopy.title}>
      <Notifications />
      {manager ? <SettingsRow href="/external-chat" icon={MessageSquareShare} label={waitingCopy.forward} /> : null}
    </SettingsGroup> : null}
    {manager ? <SecurityStatus toDevices={() => scroll.current?.scrollTo({ y: Math.max(0, devicesAt.current - 16), animated: true })} /> : null}
    <SettingsGroup title="外观">
      <SettingsRow icon={Palette} label="主题">
        {/* A theme change repaints the whole app, which stalls any slide under it; like iOS's own appearance picker, it applies at once.
            UIKit gets the same style, so native glass like the back button turns with the page instead of re-sampling it later. */}
        <View className="basis-full ps-11">
          <Segmented snap options={themes} value={current} onChange={(value) => { Appearance.setColorScheme(value === 'system' ? 'unspecified' : value); Uniwind.setTheme(value) }} />
        </View>
      </SettingsRow>
    </SettingsGroup>
    <SettingsGroup title={experimentsCopy.group}>
      <SettingsRow href="/transfers" icon={ArrowRightLeft} label="文件传输" />
      <SettingsRow icon={FlaskConical} label={experimentsCopy.label} description={experiments ? experimentsCopy.on : experimentsCopy.hint}>
        <Switch accessibilityLabel={experimentsCopy.label} value={experiments} onValueChange={setExperiments} />
      </SettingsRow>
    </SettingsGroup>
    <View className="gap-3">
      <SettingsGroup>
        <SettingsRow icon={LogOut} tone="danger" label={`断开${self}`} description={exitCopy.leave} onPress={() => open('leave')} />
        <SettingsRow icon={RotateCcw} tone="danger" label="恢复出厂设置" description={exitCopy.reset} onPress={() => open('reset')} />
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
        {sole ? <View className="flex-row items-start gap-2"><Icon as={Crown} className="mt-1 text-warning" /><Text className="min-w-0 flex-1">{eraseSoleManagerText(self)}</Text></View> : null}
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

/** The file that still works, as on the web: the code its name carries. */
function CurrentRecoveryFile() {
  const { directory } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  if (!code) return null
  return <SettingsRow icon={FileKey} label={recoveryCopy.file} description={`${recoveryCopy.codeLabel} ${code}`} />
}

/** 安全 in one group, as on the web: management devices, a waiting recovery (also after 是我 hid it), the recovery file and password. */
function SecurityStatus({ toDevices }: { toDevices(): void }) {
  const { directory, recoverySince, nodeClockOffset } = useDevice()
  const managers = directory?.members.filter((member) => member.manage).length ?? 0
  const pending = directory?.pending
  const tabular = { fontVariant: ['tabular-nums' as const] }
  return <SettingsGroup title={securityCopy.title}>
    {/* The devices are listed at the top of this page, where a phone is made a management device from its own row. */}
    {directory ? <><SettingsRow icon={Crown} label={securityCopy.managers} onPress={toDevices}>
      <View className="flex-row items-center gap-1">
        {managers === 1 ? <Icon as={TriangleAlert} className="size-4 text-warning" /> : null}
        <Text className={managers === 1 ? 'text-meta text-warning' : 'text-meta text-muted-foreground'} style={tabular}>{securityCopy.managerCount(managers)}</Text>
        <Icon as={ChevronUp} className="text-muted-foreground" />
      </View>
    </SettingsRow>
    {pending ? <SettingsRow href="/inbox" icon={ShieldAlert} tone="danger" label={securityCopy.recovery}>
      <Text className="text-meta text-danger" style={tabular}>{securityCopy.recoveryIn(pending.effective_at_ms - nodeClockOffset)}</Text>
    </SettingsRow> : <SettingsRow icon={ShieldCheck} label={securityCopy.recovery}><Text className="text-meta text-muted-foreground">{securityCopy.noRecovery}</Text></SettingsRow>}</> : null}
    <CurrentRecoveryFile />
    <SettingsRow href="/recovery" icon={KeyRound} label={securityCopy.password}>
      {recoverySince ? <Text className="text-meta text-muted-foreground" style={tabular}>{securityCopy.changedOn(recoverySince)}</Text> : null}
    </SettingsRow>
  </SettingsGroup>
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
  const mark = (icon: LucideIcon, label: string) => <>{'\u00a0'}<Icon as={icon} accessibilityLabel={label} className="size-3.5 text-muted-foreground" style={{ transform: [{ translateY: crownLift }] }} /></>
  // The name wraps in full so the inline marks are never cut off with it. As in the web's device list, the bot mark leads to the computer whose detail holds Bot 与任务.
  return <SettingsRow href={`/device/${member.device_id}`} icon={deviceIcon(member)}
    label={<>{member.name}{member.admin ? mark(Crown, '管理设备') : null}{member.kind === 'node' && member.bot ? mark(Bot, '能运行 Bot') : null}</>}
    description={member.current ? self : member.entry ? '当前连接' : undefined}>
    {member.online !== undefined ? <Online online={member.online} /> : null}
  </SettingsRow>
}
