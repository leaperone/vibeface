import * as Clipboard from 'expo-clipboard'
import { Check, CircleCheck, Copy, Link2, Plus, Share2, ShieldAlert } from 'lucide-react-native'
import { useEffect, useState } from 'react'
import { ScrollView, Share, View } from 'react-native'
import { useDevice, type Invitation } from '@vibeface/client/identity/context.tsx'
import { ErrorText } from '@/components/action'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import { JoinRequests, useOpenJoins } from '@/features/devices/join-requests'
import { QrCode } from '@/features/devices/qr-code'
import { useRun } from '@/lib/use-run'

/** "添加设备": one single-use link (QR for phones) the new device opens, then both sides compare the safety code. */
export default function AddDevice() {
  const device = useDevice()
  const { ready } = device
  const { busy, error, run } = useRun()
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  const [result, setResult] = useState<Invitation | null>(null)
  const [added, setAdded] = useState<string | null>(null)
  const requests = useOpenJoins().length > 0
  // A link is single-use: once a device has answered it, it is spent.
  const [used, setUsed] = useState(false)
  useEffect(() => { if (requests && result) setUsed(true) }, [requests, result])
  useEffect(() => { setResult(null); setAdded(null) }, [ready])
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!result) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [result, ready])
  if (!ready?.principal.can_manage) return <EmptyState icon={ShieldAlert} title="只有管理设备能添加设备" />

  const generate = () => {
    setResult(null); setUsed(false); setAdded(null)
    const connection = ready
    void run('create', async () => {
      const invitation = await device.createInvitation()
      if (connection.isCurrent()) setResult(invitation)
    })
  }
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {added && !requests ? <View className="gap-3">
      <View role="status" className="flex-row items-center gap-2"><Icon as={CircleCheck} className="size-4 text-success" /><Text numberOfLines={1} className="flex-1 text-success">已添加「{added}」</Text></View>
      <Button variant="outline" className="self-start" disabled={busy !== null} onPress={generate}><Icon as={Plus} /><Text>{busy === 'create' ? '生成中' : '再添加一台'}</Text></Button>
    </View> : !result || used && !requests ? requests ? <Button variant="outline" className="self-start" disabled={busy !== null} onPress={generate}>
      <Icon as={Link2} /><Text>{busy === 'create' ? '生成中' : '生成链接'}</Text>
    </Button> : <EmptyState icon={Link2} title="让新设备扫码加入" action={<Button disabled={busy !== null} onPress={generate}>
      <Text>{busy === 'create' ? '生成中' : '生成链接'}</Text>
    </Button>} /> : null}
    <JoinRequests issuing={result !== null} onAllowed={(name) => { setAdded(name); setResult(null) }} />
    {result && !used ? <View className="gap-4">
      <QrCode value={result.qr} label={result.publicEntry ? '用手机相机扫码打开' : '用手机 App 扫码'} />
      <View className="flex-row flex-wrap justify-center gap-2">
        <Button variant={requests ? 'outline' : 'default'} disabled={busy !== null} onPress={() => void run('copy', () => Clipboard.setStringAsync(result.link).then(() => setCopied(true)))}>
          <Icon as={copied ? Check : Copy} /><Text>{copied ? '已复制' : '复制链接'}</Text>
        </Button>
        <Button variant="outline" onPress={() => { void Share.share({ message: result.link }).catch(() => {}) }}>
          <Icon as={Share2} /><Text>分享</Text>
        </Button>
      </View>
      <Text className="text-center text-meta text-muted-foreground">只发给自己的设备，10 分钟内有效</Text>
    </View> : null}
    <ErrorText error={error} />
  </ScrollView>
}
