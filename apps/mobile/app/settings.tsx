import { Link } from 'expo-router'
import { Bell, ChevronRight, Crown, FileKey, Globe, KeyRound, Laptop, LogOut, MonitorSmartphone, Palette, Plus, Smartphone, Sparkles, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace as useShared } from '@vibeface/client/workspace/context.tsx'
import { ConnectionPill } from '@/components/connection-pill'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { JoinRequests, useOpenJoins } from '@/features/devices/join-requests'
import { cn } from '@/lib/utils'
import { useRun } from '@/lib/use-run'

const themes = [['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']] as const

export default function Settings() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { theme, hasAdaptiveThemes } = useUniwind()
  const current = hasAdaptiveThemes ? 'system' : theme
  const [leaving, setLeaving] = useState(false)
  const requests = useOpenJoins().length > 0
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(() => {}) }, [device.ready])
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="gap-8 p-4 pb-10">
    <Section icon={Palette} title="外观">
      <View accessibilityRole="radiogroup" className="flex-row flex-wrap gap-1 self-start rounded-lg bg-muted p-1">
        {themes.map(([value, label]) => <Button key={value} accessibilityRole="radio" accessibilityState={{ checked: current === value }}
          variant={current === value ? 'outline' : 'ghost'} className="min-h-11" onPress={() => Uniwind.setTheme(value)}><Text>{label}</Text></Button>)}
      </View>
    </Section>
    <Section icon={Globe} title="连接">
      <ConnectionPill connection={workspace.connection} />
    </Section>
    <Section icon={MonitorSmartphone} title="我的设备" action={device.ready?.principal.can_manage ? <Link href="/add-device" asChild>
      <Button variant={requests ? 'outline' : 'default'} className="min-h-11"><Icon as={Plus} /><Text>添加设备</Text></Button>
    </Link> : null}>
      <JoinRequests />
      <View className="overflow-hidden rounded-xl border border-border">
        {workspace.members.map((member, index) => <MemberRow key={member.device_id} member={member} first={index === 0} />)}
      </View>
    </Section>
    {device.ready ? <Notifications /> : null}
    {device.ready?.principal.can_manage ? <Section icon={Sparkles} title="AI">
      <Link href="/ai" asChild>
        <Button variant="outline" className="min-h-11 self-start"><Text>AI 设置</Text><Icon as={ChevronRight} /></Button>
      </Link>
    </Section> : null}
    <Section icon={FileKey} title="恢复文件">
      <Link href="/recovery" asChild>
        <Button variant="outline" className="min-h-11 self-start"><Icon as={KeyRound} /><Text>换恢复密码</Text></Button>
      </Link>
    </Section>
    <View className="border-t border-border pt-6">
      {leaving ? <View className="gap-3">
        <Text>断开后，要用加入链接才能回来。</Text>
        <View className="flex-row gap-2">
          <Button variant="ghost" className="min-h-11" onPress={() => setLeaving(false)}><Text>取消</Text></Button>
          <Button variant="destructive" className="min-h-11" onPress={workspace.actions.disconnect}><Icon as={LogOut} className="text-white" /><Text>断开</Text></Button>
        </View>
      </View> : <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={() => setLeaving(true)}>
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
      <Text>{button.label}</Text>
    </Button>
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
  </Section>
}

function Section({ icon, title, action, children }: { icon: LucideIcon; title: string; action?: ReactNode; children: ReactNode }) {
  return <View className="gap-3">
    <View className="min-h-11 flex-row flex-wrap items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" className="min-w-32 flex-1 text-heading font-semibold">{title}</Text>
      {action}
    </View>
    {children}
  </View>
}

const kindIcon = (member: Member) => member.kind === 'client' ? Smartphone : Laptop

function MemberRow({ member, first }: { member: Member; first: boolean }) {
  return <Link href={`/device/${member.device_id}`} asChild>
    <Pressable className={cn('min-h-14 flex-row items-center gap-3 px-4 py-2 active:bg-muted', !first && 'border-t border-border')}>
      <Icon as={kindIcon(member)} className="text-muted-foreground" />
      <View className="min-w-0 flex-1 flex-row items-center gap-1.5">
        <Text numberOfLines={1} className="shrink font-medium">{member.name}</Text>
        {member.current || member.entry ? <Text className="text-meta text-muted-foreground">{member.current ? '本机' : '当前连接'}</Text> : null}
        {member.admin ? <Icon as={Crown} accessibilityLabel="管理员" className="size-3.5 text-muted-foreground" /> : null}
      </View>
      <View accessibilityLabel={member.online ? '在线' : '离线'} className={cn('size-2 rounded-full', member.online ? 'bg-success' : 'bg-muted-foreground/40')} />
      <Icon as={ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
  </Link>
}
