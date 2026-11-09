import { router, useLocalSearchParams } from 'expo-router'
import { useLinkingURL } from 'expo-linking'
import { useState } from 'react'
import { ArrowRightLeft, Crown } from 'lucide-react-native'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { eraseSoleManagerText, soleManager, switchCopy } from '@vibeface/client/identity/copy.ts'
import { joinDestination } from '@vibeface/client/identity/switch-network.ts'
import { ErrorText } from '@/components/action'
import { Panel } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { factoryReset } from '@/lib/factory-reset'
import { keepSwitchedLink, openedLink } from '@/lib/join-link'
import { useRun } from '@/lib/use-run'
import { mobilePlatform, self } from '@/platform'

const decode = (ticket: string) => mobilePlatform.secure.ticketDecode(ticket)

/** A join link from another network: the phone erases itself, and the join page that follows picks the same link up. */
export default function SwitchNetwork() {
  const device = useDevice()
  const { busy, error, run } = useRun()
  const { link } = useLocalSearchParams<{ link?: string }>()
  const url = useLinkingURL()
  // The entry device shown is the one decoded as the sheet opened; the link open now must still name it to erase.
  const [target] = useState(() => {
    if (typeof link !== 'string') return null
    const entryId = joinDestination(link, decode)
    return entryId ? { link, entryId } : null
  })
  const matches = () => target !== null && openedLink(url) === target.link && device.linkTarget(target.link) === 'other' && joinDestination(target.link, decode) === target.entryId
  const change = async () => {
    if (!target || !matches()) throw new Error('加入目标已改变。取消后重新打开链接。')
    keepSwitchedLink(target.link)
    try { await factoryReset(device) } catch (cause) { keepSwitchedLink(null); throw cause }
  }
  // Without a connection there is no directory, so only a reachable network can warn about its last manager.
  const sole = soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  return <View className="gap-4 p-6">
    <Panel icon={ArrowRightLeft} title={switchCopy.title}>
      <Text>这个加入链接指向的网络不在当前已知设备中。换网络后仍需管理设备确认加入。</Text>
      {target ? <Text selectable className="font-mono text-meta">{target.entryId}</Text> : <ErrorText error="没有有效的加入目标。取消后重新打开链接。" />}
      {sole ? <View className="flex-row items-start gap-2"><Icon as={Crown} className="mt-1 text-warning" /><Text className="min-w-0 flex-1">{eraseSoleManagerText(self)}</Text></View> : null}
      <Text className="text-muted-foreground">{switchCopy.erase(self)}</Text>
      <ErrorText error={error} />
      <View className="flex-row justify-end gap-2">
        <Button variant="ghost" onPress={() => router.back()}><Text>取消</Text></Button>
        <Button variant="destructive" disabled={busy !== null || !matches()} onPress={() => void run('switch', change)}><Icon as={ArrowRightLeft} /><Text>{switchCopy.confirm}</Text></Button>
      </View>
    </Panel>
  </View>
}
