import * as Clipboard from 'expo-clipboard'
import { Copy, Link2, Share2, ShieldAlert } from 'lucide-react-native'
import { useEffect, useState } from 'react'
import { ScrollView, Share, View } from 'react-native'
import { useDevice, type Invitation } from '@vibeface/client/identity/context.tsx'
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
  const { busy, error, status, run } = useRun()
  const [result, setResult] = useState<Invitation | null>(null)
  const requests = useOpenJoins().length > 0
  // A link is single-use: once a device has answered it, it is spent.
  const [used, setUsed] = useState(false)
  useEffect(() => { if (requests && result) setUsed(true) }, [requests, result])
  useEffect(() => { setResult(null) }, [ready])
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!result) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [result, ready])
  if (!ready?.principal.can_manage) return <EmptyState icon={ShieldAlert} title="只有管理员能添加设备" />

  const generate = () => {
    setResult(null); setUsed(false)
    const connection = ready
    void run('create', async () => {
      const invitation = await device.createInvitation()
      if (connection.isCurrent()) setResult(invitation)
    })
  }
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="gap-5 p-4 pb-10">
    {!result || used && !requests ? <Button variant={requests ? 'outline' : 'default'} className="h-11 self-start" disabled={busy !== null} onPress={generate}>
      <Icon as={Link2} /><Text>{busy === 'create' ? '生成中' : '生成链接'}</Text>
    </Button> : null}
    <JoinRequests issuing={result !== null} />
    {result && !used ? <View className="gap-4">
      <QrCode value={result.publicEntry ? result.link : result.ticket} label={result.publicEntry ? '用手机相机扫码打开' : '在新设备的 Vibeface 里扫码'} />
      {result.publicEntry ? null : <Text className="text-center text-meta text-muted-foreground">在新设备的 Vibeface 里点“扫码”</Text>}
      <View className="flex-row flex-wrap gap-2">
        <Button variant={requests ? 'outline' : 'default'} className="h-11" disabled={busy !== null} onPress={() => void run('copy', () => Clipboard.setStringAsync(result.link).then(() => {}), '已复制')}>
          <Icon as={Copy} /><Text>复制链接</Text>
        </Button>
        <Button variant="outline" className="h-11" onPress={() => { void Share.share({ message: result.link }).catch(() => {}) }}>
          <Icon as={Share2} /><Text>分享</Text>
        </Button>
      </View>
      <Text className="text-meta text-muted-foreground">只发给你自己的设备，10 分钟内有效。</Text>
    </View> : null}
    {status ? <Text role="status" className="text-meta text-success">{status}</Text> : null}
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
  </ScrollView>
}
