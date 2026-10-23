import { Stack, router, useLocalSearchParams } from 'expo-router'
import { Crown, Laptop, MonitorSmartphone, Smartphone, Trash2 } from 'lucide-react-native'
import { useState } from 'react'
import { ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { cn } from '@/lib/utils'
import { useRun } from '@/lib/use-run'

export default function DeviceDetail() {
  const { deviceId } = useLocalSearchParams<{ deviceId: string }>()
  const device = useDevice()
  const member = useWorkspace().members.find((item) => item.device_id === deviceId)
  const { busy, error, run } = useRun()
  const [removing, setRemoving] = useState(false)
  if (!member) return <EmptyState icon={MonitorSmartphone} title="找不到这台设备" />
  const removable = device.ready?.principal.can_manage === true && !member.admin && !member.current && !member.entry
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: '' }} />
    <View className="flex-row items-center gap-4">
      <View className="size-12 items-center justify-center rounded-xl bg-muted"><Icon as={member.kind === 'client' ? Smartphone : Laptop} className="size-6" /></View>
      <View className="min-w-0 flex-1 gap-1">
        <Text numberOfLines={1} className="text-title font-semibold">{member.name}</Text>
        <View className="flex-row flex-wrap items-center gap-3">
          <View className="flex-row items-center gap-1.5">
            <View className={cn('size-2 rounded-full', member.online ? 'bg-success' : 'bg-muted-foreground/40')} />
            <Text className={cn('text-meta', member.online ? 'text-success' : 'text-muted-foreground')}>{member.online ? '在线' : '离线'}</Text>
          </View>
          {member.current || member.entry ? <Text className="text-meta text-muted-foreground">{member.current ? '本机' : '当前连接'}</Text> : null}
          {member.admin ? <View className="flex-row items-center gap-1"><Icon as={Crown} className="size-3.5 text-muted-foreground" /><Text className="text-meta text-muted-foreground">管理员</Text></View> : null}
        </View>
      </View>
    </View>
    <Text selectable className="font-mono text-meta text-muted-foreground">{member.device_id}</Text>
    {removable ? <View className="gap-3 rounded-xl border border-danger-border p-4">
      {removing ? <>
        <Text>移除后这台设备无法再连接，已有的对话记录会保留。</Text>
        <View className="flex-row flex-wrap justify-end gap-2">
          <Button variant="ghost" className="h-11" onPress={() => setRemoving(false)}><Text>取消</Text></Button>
          <Button variant="destructive" className="h-11" disabled={busy !== null} onPress={() => void run('remove', async () => { await device.revoke(member.device_id); router.back() })}>
            <Icon as={Trash2} className="text-white" /><Text>{busy ? '移除中' : '移除设备'}</Text>
          </Button>
        </View>
        {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
      </> : <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={() => setRemoving(true)}>
        <Icon as={Trash2} className="text-danger" /><Text className="text-danger">移除设备</Text>
      </Button>}
    </View> : null}
  </ScrollView>
}
