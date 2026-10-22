import { CameraView, useCameraPermissions } from 'expo-camera'
import * as Clipboard from 'expo-clipboard'
import { useLinkingURL } from 'expo-linking'
import { ArchiveRestore, Camera, ClipboardPaste, Hourglass, Link2, QrCode, ShieldAlert, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { SafeAreaView } from 'react-native-safe-area-context'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { SafetyCode } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { Textarea } from '@/components/ui/textarea'
import { useWorkspace } from '@/data/context'
import { RecoveryRestore, RecoveryReview } from '@/features/recovery/recovery'
import { useRun } from '@/lib/use-run'

export default function JoinScreen() {
  const workspace = useWorkspace()
  const device = useDevice()
  const session = workspace.session
  const [lane, setLane] = useState<'invite' | 'recovery'>('invite')
  const choosing = device.phase !== 'key_lost' && !device.review && session.phase !== 'joining'
  return <SafeAreaView className="flex-1 bg-background">
    <KeyboardAwareScrollView bottomOffset={24} contentContainerClassName="grow justify-center gap-8 px-6 py-10" keyboardShouldPersistTaps="handled">
      <Text role="heading" className="text-title font-semibold tracking-tight">Vibeface</Text>
      {device.phase === 'key_lost' ? <LostKey />
        : device.review?.kind === 'recovery' ? <RecoveryReview />
        : session.phase === 'joining' ? <Waiting code={session.code} name={session.deviceName} onCancel={workspace.actions.cancelJoin} />
        : lane === 'recovery' ? <Panel icon={ArchiveRestore} title="用恢复文件找回"><RecoveryRestore /></Panel>
        : <JoinForm />}
      {choosing ? <View className="border-t border-border pt-4">
        {lane === 'recovery'
          ? <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={() => setLane('invite')}><Icon as={Link2} /><Text>用加入链接</Text></Button>
          : <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={() => setLane('recovery')}><Icon as={ArchiveRestore} /><Text>用恢复文件找回</Text></Button>}
      </View> : null}
    </KeyboardAwareScrollView>
  </SafeAreaView>
}

/** The phone's device key is gone; starting over makes a new one, then it joins again by link or recovery file. */
function LostKey() {
  const device = useDevice()
  const { busy, error, run } = useRun()
  return <Panel icon={ShieldAlert} title="需要重新加入">
    <Text className="text-muted-foreground">这台手机保存的登录信息不见了。</Text>
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
    <Button className="h-11 self-start" disabled={busy !== null} onPress={() => void run('restart', device.replaceLostKey)}><Text>重新开始</Text></Button>
  </Panel>
}

function JoinForm() {
  const workspace = useWorkspace()
  const url = useLinkingURL()
  const [invitation, setInvitation] = useState('')
  // The raw URL keeps the ticket after `#`, which router params drop; every new `vibeface://join?link=` refills the field.
  useEffect(() => {
    const link = url ? new URL(url).searchParams.get('link') : null
    if (link) setInvitation(link)
  }, [url])
  const [name, setName] = useState('我的手机')
  const [scanning, setScanning] = useState(false)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setError('')
    setBusy(true)
    try { await workspace.actions.join(invitation.trim(), name.trim()) }
    catch (caught) { setError(caught instanceof Error ? caught.message : '没能加入，请再试一次。') }
    finally { setBusy(false) }
  }
  if (scanning) return <Scanner onCancel={() => setScanning(false)} onScanned={(link) => { setInvitation(link); setScanning(false) }} />
  return <Panel icon={QrCode} title="扫码加入">
    <Button className="h-11 self-start" onPress={() => setScanning(true)}><Icon as={Camera} /><Text>扫码</Text></Button>
    <View className="gap-2">
      <Textarea accessibilityLabel="加入链接" placeholder="或粘贴加入链接" autoCapitalize="none" autoCorrect={false} numberOfLines={4}
        className="min-h-20 font-mono text-meta" value={invitation} onChangeText={setInvitation} />
      <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={async () => setInvitation((await Clipboard.getStringAsync()).trim())}>
        <Icon as={ClipboardPaste} /><Text>粘贴</Text>
      </Button>
    </View>
    <Input accessibilityLabel="设备名称" placeholder="设备名称" className="h-11" value={name} onChangeText={setName} maxLength={64} />
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
    <Button className="h-11" disabled={busy || !invitation.trim() || !name.trim()} onPress={submit}><Text>{busy ? '加入中' : '加入'}</Text></Button>
  </Panel>
}

function Scanner({ onScanned, onCancel }: { onScanned: (link: string) => void; onCancel: () => void }) {
  const [permission, request] = useCameraPermissions()
  if (!permission) return null
  if (!permission.granted) return <Panel icon={Camera} title="扫码加入">
    <Text className="text-muted-foreground">需要相机权限才能扫码，也可以粘贴链接。</Text>
    <View className="flex-row gap-2">
      <Button variant="ghost" className="h-11" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
      <Button className="h-11" onPress={request}><Text>允许使用相机</Text></Button>
    </View>
  </Panel>
  return <Panel icon={Camera} title="扫码加入">
    <View className="aspect-square overflow-hidden rounded-xl bg-muted">
      <CameraView style={{ flex: 1 }} barcodeScannerSettings={{ barcodeTypes: ['qr'] }} onBarcodeScanned={({ data }) => onScanned(data)} />
    </View>
    <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
  </Panel>
}

function Waiting({ code, name, onCancel }: { code: string | null; name: string; onCancel: () => void }) {
  return <Panel icon={Hourglass} title="等待批准">
    <Text className="text-muted-foreground">{code ? `在管理设备上核对「${name}」的安全码` : '等管理设备开始核对'}</Text>
    {code ? <SafetyCode code={code} /> : <View className="h-8 w-40 rounded-md bg-muted" />}
    <Button variant="ghost" className="-ms-3 h-11 self-start" onPress={onCancel}><Text>取消</Text></Button>
  </Panel>
}

function Panel({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-4">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} className="text-muted-foreground" />
      <Text role="heading" className="text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}
