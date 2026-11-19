import { openURL } from 'expo-linking'
import { useRouter } from 'expo-router'
import { Check, CircleCheck, Clock, Copy, Download, Laptop, Link2, Plus, ScanEye, Share2, ShieldAlert, Smartphone, Sparkles, SquareTerminal } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { Pressable, ScrollView, Share, View } from 'react-native'
import { useDevice, type Invitation } from '@vibeface/client/identity/context.tsx'
import { addDeviceCopy, deviceCopy, joinCopy } from '@vibeface/client/identity/copy.ts'
import { ErrorText } from '@/components/action'
import { Details, EmptyState } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Segmented } from '@/components/arc/segmented'
import { Text } from '@/components/arc/text'
import { JoinRequests, useOpenJoins, type Added } from '@/features/devices/join-requests'
import { QrCode } from '@/features/devices/qr-code'
import { useRun } from '@/lib/use-run'
import { useClipboardAction } from '@/lib/use-clipboard'

const kinds = [{ value: 'phone', label: addDeviceCopy.client, icon: Smartphone }, { value: 'computer', label: addDeviceCopy.node, icon: Laptop }] as const
type Kind = (typeof kinds)[number]['value']

/** "添加设备": one single-use link the new device opens (a QR for phones, its own Vibeface page for computers), then both sides compare the safety code. */
export default function AddDevice() {
  const router = useRouter()
  const device = useDevice()
  const { ready } = device
  const { busy, error, run } = useRun()
  const [kind, setKind] = useState<Kind>('phone')
  const [result, setResult] = useState<Invitation | null>(null)
  const [added, setAdded] = useState<Added | null>(null)
  const joins = useOpenJoins()
  const requests = joins.length > 0
  const answered = result !== null && joins.some((request) => request.invite_id === result.inviteId)
  const [usedInviteId, setUsedInviteId] = useState<string | null>(null)
  const used = result !== null && (answered || usedInviteId === result.inviteId)
  useEffect(() => { if (answered && result) setUsedInviteId(result.inviteId) }, [answered, result])
  useEffect(() => { setResult(null); setAdded(null) }, [ready])
  const [, refreshExpiry] = useState(0)
  const expired = result !== null && Date.now() >= result.expiresAt
  const available = result !== null && !used && !expired
  useEffect(() => {
    if (!result || expired) return
    const timer = setTimeout(() => refreshExpiry((tick) => tick + 1), result.expiresAt - Date.now())
    return () => clearTimeout(timer)
  }, [result, expired])
  const clipboard = useClipboardAction(available ? result : null, 2000)
  const copied = clipboard.copied
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!available) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [available, result, ready])
  const generate = () => {
    if (!ready) return
    const connection = ready
    void run('create', async () => {
      setResult(null); setAdded(null)
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
  const copy = (value: string, key: 'link' | 'command') => {
    if (available && Date.now() < result.expiresAt && ready?.isCurrent()) void clipboard.copy(value, key)
  }
  const share = (message: string) => {
    if (available && Date.now() < result.expiresAt && ready?.isCurrent()) void Share.share({ message }).catch(() => {})
  }
  const copyLink = available ? <Button variant={requests ? 'secondary' : 'primary'} disabled={busy !== null || clipboard.busy} onPress={() => copy(result.link, 'link')}>
    <Icon as={copied === 'link' ? Check : Copy} /><Text>{copied === 'link' ? '已复制' : '复制链接'}</Text>
  </Button> : null
  if (!ready?.principal.can_manage) return <EmptyState icon={ShieldAlert} title="只有管理设备能添加设备" />

  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12">
    {added && !requests ? <View className="gap-3">
      <View role="status" className="flex-row items-center gap-2"><Icon as={CircleCheck} className="size-4 text-success" /><Text numberOfLines={1} className="flex-1 text-success">{added.manage ? deviceCopy.addedManager(added.name) : `已添加「${added.name}」`}</Text></View>
      <View className="flex-row flex-wrap gap-2">
        {added.kind === 'node' ? <Button onPress={() => router.replace(`/device/${added.id}`)}><Icon as={Sparkles} /><Text>{addDeviceCopy.setUpAi}</Text></Button> : null}
        {!result || (used && !expired) ? <Button variant={added.kind === 'node' ? 'secondary' : 'primary'} disabled={busy !== null} onPress={generate}><Icon as={Plus} /><Text>{busy === 'create' ? '生成中' : addDeviceCopy.addAnother}</Text></Button> : null}
      </View>
    </View> : !result || (used && !requests && !expired) ? requests ? <Button variant="secondary" className="self-start" disabled={busy !== null} onPress={generate}>
      <Icon as={Link2} /><Text>{busy === 'create' ? '生成中' : '生成链接'}</Text>
    </Button> : <EmptyState icon={Link2} title="让新设备扫码加入" action={<Button disabled={busy !== null} onPress={generate}>
      <Text>{busy === 'create' ? '生成中' : '生成链接'}</Text>
    </Button>} /> : null}
    <JoinRequests inviteId={result?.inviteId} onAllowed={(addedDevice, inviteId) => {
      if (!ready.isCurrent()) return
      setAdded(addedDevice)
      setResult((current) => current?.inviteId === inviteId ? null : current)
    }} />
    {expired ? <View className="gap-2">
      <Text role="status" accessibilityLiveRegion="polite" className="text-sm text-muted">链接已过期</Text>
      <Button variant={requests ? 'secondary' : 'primary'} className="self-start" disabled={busy !== null} onPress={generate}><Icon as={Link2} /><Text>重新生成链接</Text></Button>
    </View> : null}
    {available ? <View className="gap-4">
      <Segmented options={kinds} value={kind} onChange={(next) => { setKind(next); clipboard.reset() }} />
      {kind === 'phone' ? <>
        <Text className="text-sm text-muted">{addDeviceCopy.clientHint}</Text>
        <QrCode value={result.link} label="用手机 App 扫码" />
        <View className="flex-row flex-wrap justify-center gap-2">
          {copyLink}
          <Button variant="secondary" onPress={() => share(result.link)}>
            <Icon as={Share2} /><Text>分享</Text>
          </Button>
        </View>
      </> : <View className="gap-3">
        <View className="flex-row items-center gap-3"><Step n={1} />{copyLink}
          <Button variant="ghost" iconOnly accessibilityLabel="分享链接" onPress={() => share(result.link)}><Icon as={Share2} /></Button>
        </View>
        <View className="flex-row items-center gap-3"><Step n={2} /><Icon as={Laptop} className="size-4 text-muted" /><Text className="shrink">{addDeviceCopy.openThere}</Text></View>
        <Details label="安装节点">
          <Pressable accessibilityRole="link" onPress={() => void openURL(joinCopy.installGuide).catch(() => {})} className="min-h-11 flex-row flex-wrap items-center gap-1.5">
            <Icon as={Download} className="size-4 text-muted" />
            <Text className="text-sm text-muted">{addDeviceCopy.installThere}</Text>
            <Text className="text-sm font-medium underline">{addDeviceCopy.installHow}</Text>
          </Pressable>
          <View className="gap-2 border-t border-border pt-3">
            <View className="flex-row items-center gap-1.5"><Icon as={SquareTerminal} className="size-4 text-muted" /><Text className="shrink text-sm text-muted">{addDeviceCopy.orCommand}</Text></View>
            <View className="flex-row items-center gap-2">
              <Button variant="secondary" disabled={busy !== null || clipboard.busy} onPress={() => copy(addDeviceCopy.command(result.link), 'command')}>
                <Icon as={copied === 'command' ? Check : Copy} /><Text>{copied === 'command' ? '已复制' : addDeviceCopy.copyCommand}</Text>
              </Button>
              <Button variant="ghost" iconOnly accessibilityLabel="分享命令" onPress={() => share(addDeviceCopy.command(result.link))}><Icon as={Share2} /></Button>
            </View>
          </View>
        </Details>
      </View>}
      <View className="gap-1.5">
        <View className="flex-row items-center gap-1.5"><Icon as={ScanEye} className="size-4 text-muted" /><Text className="shrink text-sm text-muted">{addDeviceCopy.compare}</Text></View>
        <View className="flex-row items-center gap-1.5"><Icon as={Clock} className="size-4 text-muted" /><Text className="shrink text-sm text-muted">只给自己的设备，10 分钟有效</Text></View>
      </View>
    </View> : null}
    <ErrorText error={error || clipboard.error} />
  </ScrollView>
}

function Step({ n }: { n: number }) {
  return <View accessibilityElementsHidden importantForAccessibility="no-hide-descendants" className="size-6 items-center justify-center rounded-pill bg-surface-muted"><Text className="text-sm font-medium text-muted">{n}</Text></View>
}
