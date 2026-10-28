import { CameraView, useCameraPermissions } from 'expo-camera'
import * as Clipboard from 'expo-clipboard'
import { useLinkingURL } from 'expo-linking'
import { ArchiveRestore, Camera, ClipboardPaste, Hourglass, Link2, ShieldAlert, X } from 'lucide-react-native'
import { humanError } from '@vibeface/client/lib/errors.ts'
import { useEffect, useState } from 'react'
import { View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import Animated, { LinearTransition, ReduceMotion } from 'react-native-reanimated'
import { SafeAreaView } from 'react-native-safe-area-context'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { joinCopy } from '@vibeface/client/identity/copy.ts'
import { Panel, SafetyCode } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { Textarea } from '@/components/ui/textarea'
import { useWorkspace } from '@/data/context'
import { RecoveredStep, RecoveryRestore, RecoveryReview, RecoveryWaiting } from '@/features/recovery/recovery'
import { useRun } from '@/lib/use-run'
import { defaultName, self } from '@/platform'

export default function JoinScreen() {
  const workspace = useWorkspace()
  const device = useDevice()
  const session = workspace.session
  const [lane, setLane] = useState<'invite' | 'recovery'>('invite')
  const recovering = Boolean(device.recovery || device.ready && device.recovered)
  const choosing = device.phase !== 'key_lost' && !device.review && !recovering && session.phase !== 'joining'
  // The safe-area view is not Uniwind-wrapped, so its size goes in `style`.
  return <SafeAreaView style={{ flex: 1 }}>
    <KeyboardAwareScrollView bottomOffset={24} contentContainerClassName="w-full max-w-md grow gap-8 self-center px-6 py-10" keyboardShouldPersistTaps="handled">
      {/* The name holds the open space; the actions sit low, where the thumb is. */}
      <View className="min-h-24 flex-1 justify-center">
        <Animated.View layout={LinearTransition.reduceMotion(ReduceMotion.System)}>
          <Text role="heading" className="text-[40px] leading-[44px] font-bold tracking-tighter">Vibeface</Text>
        </Animated.View>
      </View>
      {device.phase === 'key_lost' ? <LostKey />
        : device.ready && device.recovered ? <RecoveredStep />
        : device.recovery ? <RecoveryWaiting />
        : device.review?.kind === 'recovery' ? <RecoveryReview />
        : session.phase === 'joining' ? <Waiting code={session.code} onCancel={workspace.actions.cancelJoin} />
        : lane === 'recovery' ? <Panel icon={ArchiveRestore} title="用恢复文件找回"><RecoveryRestore /></Panel>
        : <JoinForm />}
      {choosing ? <View className="border-t border-border pt-4">
        {lane === 'recovery'
          ? <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={() => setLane('invite')}><Icon as={Link2} /><Text>{joinCopy.byLink}</Text></Button>
          : <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={() => setLane('recovery')}><Icon as={ArchiveRestore} /><Text>用恢复文件找回</Text></Button>}
      </View> : null}
    </KeyboardAwareScrollView>
  </SafeAreaView>
}

/** The phone's device key is gone; starting over makes a new one, then it joins again by link or recovery file. */
function LostKey() {
  const device = useDevice()
  const { busy, error, run } = useRun()
  return <Panel icon={ShieldAlert} title="需要重新加入">
    <Text className="text-muted-foreground">{self}保存的登录信息不见了。</Text>
    {error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null}
    <Button className="min-h-11 self-start" disabled={busy !== null} onPress={() => void run('restart', device.replaceLostKey)}><Text>重新开始</Text></Button>
  </Panel>
}

// A link is single-use: once a join started from it, coming back here after 取消 or 断开 must not offer it again.
let filled: string | null = null

function JoinForm() {
  const workspace = useWorkspace()
  const device = useDevice()
  const url = useLinkingURL()
  const [invitation, setInvitation] = useState('')
  // The raw URL keeps the ticket after `#`, which router params drop; every new `vibeface://join?link=` refills the field.
  useEffect(() => {
    const link = url ? new URL(url).searchParams.get('link') : null
    if (link && link !== filled) { filled = link; setInvitation(link) }
  }, [url])
  const [name, setName] = useState(defaultName)
  const [scanning, setScanning] = useState(false)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setError('')
    setBusy(true)
    try { await workspace.actions.join(invitation.trim(), name.trim()) }
    catch (caught) { setError(humanError(caught instanceof Error ? caught.message : '', undefined, '这次加入')) }
    finally { setBusy(false) }
  }
  if (scanning) return <Scanner onCancel={() => setScanning(false)} onScanned={(link) => { setInvitation(link); setScanning(false) }} />
  const ready = invitation.trim() !== ''
  // A join that fails after it started remounts this form, so its own error is gone; the shared one says why, as on the web.
  const failure = error || device.error
  // Scanning leads; once there is a link it shrinks to one line, and the name and 加入 take over.
  if (ready) return <View className="gap-4">
    <View className="min-h-12 flex-row items-center gap-3 rounded-xl bg-muted/60 ps-4">
      <Icon as={Link2} className="text-success" />
      <Text className="min-w-0 flex-1">{joinCopy.linkFilled}</Text>
      <Button variant="ghost" size="icon" className="size-12" accessibilityLabel="清除链接" onPress={() => setInvitation('')}><Icon as={X} /></Button>
    </View>
    <View className="gap-2">
      <Text className="text-meta font-medium text-muted-foreground">{joinCopy.deviceName}</Text>
      <Input accessibilityLabel={joinCopy.deviceName} placeholder={defaultName} className="min-h-11" value={name} onChangeText={setName} maxLength={64} />
    </View>
    {failure ? <Text role="alert" className="text-meta text-danger">{failure}</Text> : null}
    <Button className="min-h-12" disabled={busy || !name.trim()} onPress={submit}><Text>{busy ? '加入中' : '加入'}</Text></Button>
  </View>
  return <View className="gap-4">
    <Text className="text-meta text-muted-foreground">{joinCopy.linkSource()}</Text>
    {failure ? <Text role="alert" className="text-meta text-danger">{failure}</Text> : null}
    <Button className="min-h-12" onPress={() => setScanning(true)}><Icon as={Camera} /><Text>扫码加入</Text></Button>
    <View className="gap-2">
      <Textarea accessibilityLabel="加入链接" placeholder="或粘贴加入链接" autoCapitalize="none" autoCorrect={false} numberOfLines={4}
        className="min-h-20 font-mono text-meta" value={invitation} onChangeText={setInvitation} />
      <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={async () => setInvitation((await Clipboard.getStringAsync()).trim())}>
        <Icon as={ClipboardPaste} /><Text>粘贴</Text>
      </Button>
    </View>
  </View>
}

function Scanner({ onScanned, onCancel }: { onScanned: (link: string) => void; onCancel: () => void }) {
  const [permission, request] = useCameraPermissions()
  if (!permission) return null
  if (!permission.granted) return <Panel icon={Camera} title="扫码加入">
    <Text className="text-muted-foreground">需要相机权限才能扫码，也可以粘贴链接</Text>
    <View className="flex-row gap-2">
      <Button variant="ghost" className="min-h-11" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
      <Button className="min-h-11" onPress={request}><Text>允许使用相机</Text></Button>
    </View>
  </Panel>
  return <Panel icon={Camera} title="扫码加入">
    <View className="aspect-square overflow-hidden rounded-xl bg-muted">
      <CameraView style={{ flex: 1 }} barcodeScannerSettings={{ barcodeTypes: ['qr'] }} onBarcodeScanned={({ data }) => onScanned(data)} />
    </View>
    <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
  </Panel>
}

function Waiting({ code, onCancel }: { code: string | null; onCancel: () => void }) {
  return <Panel icon={Hourglass} title={joinCopy.waiting}>
    <Text role="status" className="text-muted-foreground">{code ? joinCopy.compareCode : joinCopy.showCode}</Text>
    {code ? <SafetyCode code={code} /> : <View className="h-8 w-40 rounded-md bg-muted" />}
    <Button variant="ghost" className="-ms-3 min-h-11 self-start" onPress={onCancel}><Text>取消</Text></Button>
  </Panel>
}
