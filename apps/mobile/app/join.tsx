import * as Clipboard from 'expo-clipboard'
import { openURL, useLinkingURL } from 'expo-linking'
import { ArchiveRestore, Camera, ClipboardPaste, ExternalLink, Hourglass, Laptop, Link2, ShieldAlert, ShieldCheck, X } from 'lucide-react-native'
import { humanError, spentInviteText } from '@vibeface/client/lib/errors.ts'
import { useEffect, useState } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import Animated from 'react-native-reanimated'
import { SafeAreaView } from 'react-native-safe-area-context'
import Svg, { Path, Rect } from 'react-native-svg'
import { useCSSVariable } from 'uniwind'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { joinCopy } from '@vibeface/client/identity/copy.ts'
import { recoveryTarget } from '@vibeface/client/identity/recovery-link.ts'
import { ErrorText } from '@/components/action'
import { Panel, SafetyCode, Skeleton } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { Textarea } from '@/components/ui/textarea'
import { useWorkspace } from '@/data/context'
import { Scanner } from '@/features/devices/scanner'
import { RecoveredStep, RecoveryRestore, RecoveryReview, RecoveryWaiting } from '@/features/recovery/recovery'
import { openedLink } from '@/lib/join-link'
import { move } from '@/lib/motion'
import { useRun } from '@/lib/use-run'
import { defaultName, self } from '@/platform'

export default function JoinScreen() {
  const workspace = useWorkspace()
  const device = useDevice()
  const session = workspace.session
  const url = useLinkingURL()
  const [lane, setLane] = useState<'invite' | 'recovery'>('invite')
  // A recovery link the camera opened goes straight to finding devices again, with its computer filled in.
  useEffect(() => { if (url && recoveryTarget(url)) setLane('recovery') }, [url])
  const recovering = Boolean(device.recovery || device.ready && device.recovered)
  // The request is out, so the Node has spent the link.
  useEffect(() => { if (session.phase === 'joining') spent = submitted }, [session.phase])
  const choosing = device.phase !== 'key_lost' && !device.review && !recovering && session.phase !== 'joining'
  // The safe-area view is not Uniwind-wrapped, so its size goes in `style`.
  return <SafeAreaView style={{ flex: 1 }}>
    <KeyboardAwareScrollView bottomOffset={24} contentContainerClassName="w-full max-w-md grow gap-6 self-center px-4 py-12" keyboardShouldPersistTaps="handled">
      {/* The name holds the open space; the actions sit low, where the thumb is. */}
      <View className="min-h-24 flex-1 justify-center">
        <Animated.View layout={move}><Wordmark /></Animated.View>
      </View>
      {device.phase === 'key_lost' ? <LostKey />
        : device.ready && device.recovered ? <RecoveredStep />
        : device.recovery ? <RecoveryWaiting />
        : device.review?.kind === 'recovery' ? <RecoveryReview />
        : session.phase === 'joining' ? <Waiting code={session.code} admin={session.admin} reached={Boolean(device.info)} onCancel={workspace.actions.cancelJoin} />
        : lane === 'recovery' ? <Panel icon={ArchiveRestore} title="用恢复文件找回"><RecoveryRestore link={url} /></Panel>
        : <JoinForm />}
      {choosing ? <View className="gap-1 border-t border-border pt-4">
        {lane === 'recovery'
          ? <Button variant="ghost" className="-ms-3 self-start" onPress={() => setLane('invite')}><Icon as={Link2} /><Text>{joinCopy.byLink}</Text></Button>
          : <>
            <NoComputer />
            <Button variant="ghost" className="-ms-3 self-start" onPress={() => setLane('recovery')}><Icon as={ArchiveRestore} /><Text>用恢复文件找回</Text></Button>
          </>}
      </View> : null}
    </KeyboardAwareScrollView>
  </SafeAreaView>
}

/** The app icon's "V" on its tile beside the name, as on the web's join page. */
function Wordmark() {
  const [background, foreground] = useCSSVariable(['--background', '--foreground']) as [string, string]
  return <View className="flex-row items-center gap-2">
    <Svg width={32} height={32} viewBox="0 0 64 64">
      <Rect width={64} height={64} rx={16} fill={foreground} />
      <Path d="M19 21l13 24 13-24" fill="none" stroke={background} strokeWidth={6.5} strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
    <Text role="heading" className="text-title font-semibold tracking-tight">Vibeface</Text>
  </View>
}

/** A first phone with no computer yet: Vibeface runs on a computer first, and its 添加设备 brings the phone in. */
function NoComputer() {
  return <Pressable accessibilityRole="link" onPress={() => void openURL(joinCopy.installGuide).catch(() => {})}
    className="-mx-3 min-h-14 flex-row items-center gap-2 rounded-lg px-4 py-2 active:bg-accent">
    <Icon as={Laptop} />
    <View className="min-w-0 flex-1">
      <Text className="font-medium">{joinCopy.noComputer}</Text>
      <Text className="text-meta text-muted-foreground">{joinCopy.installFirst}</Text>
    </View>
    <Icon as={ExternalLink} className="size-4 text-muted-foreground" />
  </Pressable>
}

/** The phone's device key is gone; starting over makes a new one, then it joins again by link or recovery file. */
function LostKey() {
  const device = useDevice()
  const { busy, error, run } = useRun()
  return <Panel icon={ShieldAlert} title="需要重新加入">
    <Text className="text-muted-foreground">{self}保存的登录信息不见了。</Text>
    <ErrorText error={error} />
    <Button className="self-start" disabled={busy !== null} onPress={() => void run('restart', device.replaceLostKey)}><Text>重新开始</Text></Button>
  </Panel>
}

// A link is single-use: once a join request went out with it, coming back here after 取消 or 断开 must not offer it again.
let submitted = '', spent = ''

function JoinForm() {
  const workspace = useWorkspace()
  const device = useDevice()
  const url = useLinkingURL()
  const [invitation, setInvitation] = useState('')
  // An error belongs to the link it was about: a new one, typed, pasted, scanned or opened, starts clean.
  const [edited, setEdited] = useState(false)
  const edit = (text: string) => { setInvitation(text); setError(''); setEdited(true) }
  // Every new `vibeface://join?link=` refills the field, also the one a network switch erased the phone for.
  useEffect(() => {
    const link = openedLink(url)
    if (link && link !== spent) edit(link)
  }, [url])
  const [name, setName] = useState(defaultName)
  const [scanning, setScanning] = useState(false)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setError('')
    setBusy(true)
    submitted = invitation.trim()
    try { await workspace.actions.join(invitation.trim(), name.trim()) }
    catch (caught) { setError(humanError(caught instanceof Error ? caught.message : '', undefined, '这次加入')) }
    finally { setBusy(false) }
  }
  // Checked as it changes, as on the web, down to a ticket that decodes: only a real join link counts as filled, so typing
  // or any other text keeps the field.
  const ready = device.linkTarget(invitation) !== null
  // A used or expired link says so as it opens, before the name and 加入; no answer leaves the form as it is.
  useEffect(() => {
    if (!ready) return
    let live = true
    void device.checkInvite(invitation.trim()).then((state) => {
      if (live && (state === 'used' || state === 'expired')) { setInvitation(''); setError(spentInviteText) }
    })
    return () => { live = false }
  }, [invitation, ready])
  if (scanning) return <Scanner title={joinCopy.scan} onCancel={() => setScanning(false)} onScanned={(link) => { edit(link); setScanning(false) }} />
  const invalid = !ready && invitation.trim() !== ''
  // A join that fails after it started remounts this form, so its own error is gone; the shared one says why, as on the web.
  const failure = error || (edited ? '' : device.error)
  // Scanning leads; once there is a link it shrinks to one line, and the name and 加入 take over.
  if (ready) return <View className="gap-4">
    <View className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <Icon as={Link2} className="text-muted-foreground" />
      <Text className="min-w-0 flex-1">{joinCopy.linkFilled}</Text>
      <Button variant="ghost" size="icon" accessibilityLabel="清除链接" onPress={() => edit('')}><Icon as={X} /></Button>
    </View>
    <View className="gap-2">
      <Text className="text-meta font-medium text-muted-foreground">{joinCopy.deviceName}</Text>
      <Input accessibilityLabel={joinCopy.deviceName} value={name} onChangeText={setName} maxLength={64} />
    </View>
    <ErrorText error={failure} />
    <Button disabled={busy || !name.trim()} onPress={submit}><Text>{busy ? joinCopy.joining : joinCopy.join}</Text></Button>
  </View>
  return <View className="gap-4">
    <Text className="text-meta text-muted-foreground">{joinCopy.linkSource()}</Text>
    <ErrorText error={failure} />
    <Button onPress={() => setScanning(true)}><Icon as={Camera} /><Text>{joinCopy.scan}</Text></Button>
    <View className="gap-2">
      <Textarea accessibilityLabel="加入链接" placeholder={joinCopy.paste} autoCapitalize="none" autoCorrect={false} numberOfLines={4}
        className="min-h-20 font-mono text-meta" value={invitation} onChangeText={edit} />
      {invalid ? <ErrorText error={joinCopy.notLink} /> : null}
      <Paste onText={(text) => edit(text.trim())} />
    </View>
  </View>
}

/** iOS's own paste control reads the clipboard without asking each time; elsewhere a button reads it. */
function Paste({ onText }: { onText: (text: string) => void }) {
  const [fill, ink] = useCSSVariable(['--muted', '--foreground']) as [string, string]
  if (Clipboard.isPasteButtonAvailable) return <Clipboard.ClipboardPasteButton acceptedContentTypes={['plain-text', 'url']} backgroundColor={fill} foregroundColor={ink}
    cornerStyle="medium" style={{ alignSelf: 'flex-start', height: 44, width: 104 }} onPress={(data) => { if (data.type === 'text') onText(data.text) }} />
  return <Button variant="ghost" className="-ms-3 self-start" onPress={async () => onText(await Clipboard.getStringAsync())}>
    <Icon as={ClipboardPaste} /><Text>粘贴</Text>
  </Button>
}

function Waiting({ code, admin, reached, onCancel }: { code: string | null; admin?: string; reached: boolean; onCancel: () => void }) {
  return <Panel icon={Hourglass} title={joinCopy.waiting}>
    {reached ? <View className="flex-row items-center gap-2"><Icon as={ShieldCheck} className="size-4 text-success" /><Text className="text-meta text-muted-foreground">{joinCopy.reached}</Text></View> : null}
    <Text role="status" className="text-muted-foreground">{code ? joinCopy.compareCode(admin) : joinCopy.showCode}</Text>
    {code ? <SafetyCode code={code} /> : <Skeleton className="h-8 w-40" />}
    <Button variant="ghost" className="-ms-3 self-start" onPress={onCancel}><Text>取消</Text></Button>
  </Panel>
}
