import * as Clipboard from 'expo-clipboard'
import { Check, CircleCheck, Clock, Copy, Link2, Monitor, Plus, ScanEye, Share2, ShieldAlert, Smartphone, SquareTerminal } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { ScrollView, Share, View } from 'react-native'
import { useDevice, type Invitation } from '@vibeface/client/identity/context.tsx'
import { addDeviceCopy } from '@vibeface/client/identity/copy.ts'
import { ErrorText } from '@/components/action'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { JoinRequests, useOpenJoins } from '@/features/devices/join-requests'
import { QrCode } from '@/features/devices/qr-code'
import { useRun } from '@/lib/use-run'

const kinds = [{ value: 'phone', label: addDeviceCopy.phone, icon: Smartphone }, { value: 'computer', label: addDeviceCopy.computer, icon: Monitor }] as const
type Kind = (typeof kinds)[number]['value']

/** "添加设备": one single-use link the new device opens (a QR for phones, a command for computers), then both sides compare the safety code. */
export default function AddDevice() {
  const device = useDevice()
  const { ready } = device
  const { busy, error, run } = useRun()
  const [copied, setCopied] = useState(false)
  const [kind, setKind] = useState<Kind>('phone')
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
  const generate = () => {
    if (!ready) return
    setResult(null); setUsed(false); setAdded(null)
    const connection = ready
    void run('create', async () => {
      const invitation = await device.createInvitation()
      if (connection.isCurrent()) setResult(invitation)
    })
  }
  // As on the web, opening the page makes the link and its code at once, unless a device is already asking to join; once per connection.
  const opened = useRef<object | null>(null)
  useEffect(() => {
    if (!ready?.principal.can_manage || requests || opened.current === ready) return
    opened.current = ready
    generate()
  }, [ready])
  if (!ready?.principal.can_manage) return <EmptyState icon={ShieldAlert} title="只有管理设备能添加设备" />

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
      <Segmented options={kinds} value={kind} onChange={(next) => { setKind(next); setCopied(false) }} />
      {kind === 'phone' ? <>
        <QrCode value={result.qr} label={result.publicEntry ? '用手机相机扫码打开' : '用手机 App 扫码'} />
        <View className="flex-row flex-wrap justify-center gap-2">
          <Button variant={requests ? 'outline' : 'default'} disabled={busy !== null} onPress={() => void run('copy', () => Clipboard.setStringAsync(result.link).then(() => setCopied(true)))}>
            <Icon as={copied ? Check : Copy} /><Text>{copied ? '已复制' : '复制链接'}</Text>
          </Button>
          <Button variant="outline" onPress={() => { void Share.share({ message: result.link }).catch(() => {}) }}>
            <Icon as={Share2} /><Text>分享</Text>
          </Button>
        </View>
      </> : <View className="gap-3">
        <View className="flex-row items-center gap-3"><Step n={1} />
          <Button variant={requests ? 'outline' : 'default'} disabled={busy !== null} onPress={() => void run('copy', () => Clipboard.setStringAsync(addDeviceCopy.command(result.link)).then(() => setCopied(true)))}>
            <Icon as={copied ? Check : Copy} /><Text>{copied ? '已复制' : addDeviceCopy.copyCommand}</Text>
          </Button>
          <Button variant="ghost" size="icon" accessibilityLabel="分享命令" onPress={() => { void Share.share({ message: addDeviceCopy.command(result.link) }).catch(() => {}) }}><Icon as={Share2} /></Button>
        </View>
        <View className="flex-row items-center gap-3"><Step n={2} /><Icon as={SquareTerminal} className="size-4 text-muted-foreground" /><Text className="shrink">{addDeviceCopy.runThere}</Text></View>
      </View>}
      <View className="gap-1.5">
        <View className="flex-row items-center gap-1.5"><Icon as={ScanEye} className="size-4 text-muted-foreground" /><Text className="shrink text-meta text-muted-foreground">{addDeviceCopy.compare}</Text></View>
        <View className="flex-row items-center gap-1.5"><Icon as={Clock} className="size-4 text-muted-foreground" /><Text className="shrink text-meta text-muted-foreground">只发给自己的设备，10 分钟内有效</Text></View>
      </View>
    </View> : null}
    <ErrorText error={error} />
  </ScrollView>
}

function Step({ n }: { n: number }) {
  return <View accessibilityElementsHidden importantForAccessibility="no-hide-descendants" className="size-6 items-center justify-center rounded-full bg-muted"><Text className="text-meta font-medium text-muted-foreground">{n}</Text></View>
}
