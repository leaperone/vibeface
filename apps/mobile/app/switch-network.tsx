import { router } from 'expo-router'
import { ArrowRightLeft, Crown } from 'lucide-react-native'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { eraseSoleManagerText, soleManager, switchCopy } from '@vibeface/client/identity/copy.ts'
import { ErrorText } from '@/components/action'
import { Panel } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { factoryReset } from '@/lib/factory-reset'
import { useRun } from '@/lib/use-run'
import { self } from '@/platform'

/** A join link from another network: the phone erases itself, and the join page that follows picks the same link up. */
export default function SwitchNetwork() {
  const device = useDevice()
  const { busy, error, run } = useRun()
  // Without a connection there is no directory, so only a reachable network can warn about its last manager.
  const sole = soleManager(device.directory?.members ?? [], device.identity?.deviceId)
  return <View className="gap-4 p-6">
    <Panel icon={ArrowRightLeft} title={switchCopy.title}>
      {sole ? <View className="flex-row items-start gap-2"><Icon as={Crown} className="mt-1 text-warning" /><Text className="min-w-0 flex-1">{eraseSoleManagerText(self)}</Text></View> : null}
      <Text className="text-muted-foreground">{switchCopy.erase(self)}</Text>
      <ErrorText error={error} />
      <View className="flex-row justify-end gap-2">
        <Button variant="ghost" onPress={() => router.back()}><Text>取消</Text></Button>
        <Button variant="destructive" disabled={busy !== null} onPress={() => void run('switch', () => factoryReset(device))}><Icon as={ArrowRightLeft} /><Text>{switchCopy.confirm}</Text></Button>
      </View>
    </Panel>
  </View>
}
