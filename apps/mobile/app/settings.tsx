import { Crown, Globe, Laptop, LogOut, MonitorSmartphone, Palette, Smartphone, type LucideIcon } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { ScrollView, View } from 'react-native'
import { Uniwind, useUniwind } from 'uniwind'
import { ConnectionPill } from '@/components/connection-pill'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import type { Member } from '@/data/types'
import { cn } from '@/lib/utils'

const themes = [['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']] as const

export default function Settings() {
  const workspace = useWorkspace()
  const { theme, hasAdaptiveThemes } = useUniwind()
  const current = hasAdaptiveThemes ? 'system' : theme
  const [leaving, setLeaving] = useState(false)
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="gap-8 p-4 pb-10">
    <Section icon={Palette} title="外观">
      <View accessibilityRole="radiogroup" className="flex-row gap-1 self-start rounded-lg bg-muted p-1">
        {themes.map(([value, label]) => <Button key={value} accessibilityRole="radio" accessibilityState={{ checked: current === value }}
          variant={current === value ? 'outline' : 'ghost'} className="h-11" onPress={() => Uniwind.setTheme(value)}><Text>{label}</Text></Button>)}
      </View>
    </Section>
    <Section icon={Globe} title="连接">
      <ConnectionPill connection={workspace.connection} />
    </Section>
    <Section icon={MonitorSmartphone} title="我的设备">
      <View className="overflow-hidden rounded-xl border border-border">
        {workspace.members.map((member, index) => <MemberRow key={member.device_id} member={member} first={index === 0} />)}
      </View>
    </Section>
    <View className="border-t border-border pt-6">
      {leaving ? <View className="gap-3">
        <Text>断开后，要用加入链接才能回来。</Text>
        <View className="flex-row gap-2">
          <Button variant="ghost" className="h-11" onPress={() => setLeaving(false)}><Text>取消</Text></Button>
          <Button variant="destructive" className="h-11" onPress={workspace.actions.disconnect}><Icon as={LogOut} className="text-white" /><Text>断开</Text></Button>
        </View>
      </View> : <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={() => setLeaving(true)}>
        <Icon as={LogOut} className="text-muted-foreground" /><Text className="text-muted-foreground">断开这台手机</Text>
      </Button>}
    </View>
  </ScrollView>
}

function Section({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" className="text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}

const kindIcon = (member: Member) => member.kind === 'client' ? Smartphone : Laptop

function MemberRow({ member, first }: { member: Member; first: boolean }) {
  return <View className={cn('min-h-14 flex-row items-center gap-3 px-4 py-2', !first && 'border-t border-border')}>
    <Icon as={kindIcon(member)} className="text-muted-foreground" />
    <View className="min-w-0 flex-1 flex-row items-center gap-1.5">
      <Text numberOfLines={1} className="shrink font-medium">{member.name}</Text>
      {member.current ? <Text className="text-meta text-muted-foreground">本机</Text> : null}
      {member.admin ? <Icon as={Crown} accessibilityLabel="管理员" className="size-3.5 text-muted-foreground" /> : null}
    </View>
    <View accessibilityLabel={member.online ? '在线' : '离线'} className={cn('size-2 rounded-full', member.online ? 'bg-success' : 'bg-muted-foreground/40')} />
  </View>
}
