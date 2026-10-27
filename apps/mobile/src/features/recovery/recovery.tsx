import * as DocumentPicker from 'expo-document-picker'
import { File } from 'expo-file-system'
import { ArchiveRestore, BellRing, CalendarClock, CircleCheck, Download, FileCheck, FileKey, FileWarning, Hourglass, KeyRound, Laptop, ShieldX, Smartphone, type LucideIcon } from 'lucide-react-native'
import { useState } from 'react'
import { Switch, View } from 'react-native'
import { useDevice, useRecoveryCode } from '@vibeface/client/identity/context.tsx'
import { recoveryCopy, whenText } from '@vibeface/client/identity/copy.ts'
import { messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { passphrase, recoveryFileStem, type RecoveryFile } from '@vibeface/client/lib/secure.ts'
import { Details, Panel } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { useRun } from '@/lib/use-run'

type PickedFile = { name: string; file: RecoveryFile }

/** Picks the recovery file from Files; the picked copy reads as the `RecoveryFile` the shared provider opens. */
function FilePick({ picked, onPick, disabled }: { picked: PickedFile | null; onPick(file: PickedFile): void; disabled?: boolean }) {
  const choose = async () => {
    const result = await DocumentPicker.getDocumentAsync({ type: ['application/json', 'public.json', '*/*'], copyToCacheDirectory: true })
    const asset = result.assets?.[0]
    if (!result.canceled && asset) onPick({ name: asset.name, file: new File(asset.uri) })
  }
  return <Button variant="outline" className="min-h-11 self-start" disabled={disabled} onPress={() => void choose()}>
    <Icon as={picked ? FileCheck : FileKey} className={picked ? 'text-success' : undefined} />
    <Text numberOfLines={1} className="shrink">{picked ? picked.name : '选择恢复文件'}</Text>
  </Button>
}

const passwordProps = { secureTextEntry: true, autoCapitalize: 'none', autoCorrect: false, className: 'h-11' } as const
const errorText = (error: string) => error ? <Text role="alert" className="text-meta text-danger">{error}</Text> : null
const hint = (icon: LucideIcon, text: string, tone = 'text-muted-foreground') => <View className="flex-row items-center gap-2"><Icon as={icon} className={`size-4 ${tone}`} /><Text className={`min-w-0 flex-1 text-meta ${tone}`}>{text}</Text></View>

/** Find devices again with the recovery file: open it, connect to one of the devices, then pick what was lost. */
export function RecoveryRestore() {
  const device = useDevice()
  const target = device.entry ?? device.entries[0] ?? null
  const { busy, error, run } = useRun()
  const [picked, setPicked] = useState<PickedFile | null>(null)
  const [password, setPassword] = useState('')
  const [name, setName] = useState('我的手机')
  const [address, setAddress] = useState(target?.address ?? '')
  const [deviceId, setDeviceId] = useState(target?.device_id ?? '')
  const submit = () => void run('restore', async () => {
    if (!picked) throw new Error('请选择恢复文件。')
    const typed = password
    // A failed try keeps the chosen file and name; only the typed password leaves the screen.
    setPassword('')
    await device.beginRecovery({ file: picked.file, password: typed, name: name.trim(), entry: parseEntry({ address: address.trim(), device_id: deviceId.trim() }) })
  })
  const locked = busy !== null || Boolean(device.review)
  return <View className="gap-4">
    <Text className="text-meta text-muted-foreground">{recoveryCopy.scope}</Text>
    <FilePick picked={picked} onPick={setPicked} disabled={locked} />
    <Input {...passwordProps} accessibilityLabel="恢复密码" placeholder="恢复密码" value={password} onChangeText={setPassword} editable={!locked} />
    <Input accessibilityLabel="这台手机叫什么？" placeholder="这台手机叫什么？" className="min-h-11" value={name} onChangeText={setName} maxLength={128} editable={!locked} />
    <Details label={target ? recoveryCopy.connectTo : recoveryCopy.manual} open={!target}>
      <Input accessibilityLabel="访问地址" placeholder="https://" className="min-h-11" keyboardType="url" autoCapitalize="none" autoCorrect={false} value={address} onChangeText={setAddress} editable={!locked} />
      <Input accessibilityLabel="设备编号" placeholder="设备编号" className="min-h-11 font-mono" autoCapitalize="none" autoCorrect={false} value={deviceId} onChangeText={setDeviceId} editable={!locked} />
    </Details>
    {errorText(error)}
    <Button className="min-h-11" disabled={locked || !picked || !password || !name.trim() || !address.trim() || !deviceId.trim()} onPress={submit}>
      <Text>{busy ? '打开中' : '下一步'}</Text>
    </Button>
  </View>
}

/** After unlocking a recovery file: keep every device except the lost ones. */
export function RecoveryReview() {
  const device = useDevice()
  const review = device.review
  const { busy, error, run } = useRun()
  const [lost, setLost] = useState<string[]>([])
  if (review?.kind !== 'recovery') return null
  // As on the web: the kept devices go as the file listed them, with no choice of who manages.
  const recover = () => void run('recover', () => device.confirmRecovery(review.members.filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id))))
  return <Panel icon={ArchiveRestore} title={recoveryCopy.whichLost}>
    <View className="gap-2">
      {[...review.members].sort((a, b) => Number(b.device_id === review.requiredNode) - Number(a.device_id === review.requiredNode)).map((member) => {
        const required = member.device_id === review.requiredNode
        const gone = lost.includes(member.device_id)
        return <View key={member.device_id} className="min-h-12 flex-row items-center gap-2 rounded-lg bg-muted/60 px-3">
          <Icon as={member.kind === 'client' ? Smartphone : Laptop} className="size-4 text-muted-foreground" />
          <View className="min-w-0 flex-1 py-1.5">
            <Text numberOfLines={1} className={gone ? 'text-muted-foreground line-through' : undefined}>{member.name}</Text>
            {gone ? <Text className="text-meta text-danger">生效后移出</Text> : null}
          </View>
          {required ? <Text className="text-meta text-muted-foreground">当前连接</Text> : <>
            <Text className="text-meta">丢了</Text>
            <Switch accessibilityLabel={`${member.name} 丢了`} value={gone} disabled={busy !== null} onValueChange={(on) => setLost(on ? [...lost, member.device_id] : lost.filter((id) => id !== member.device_id))} />
          </>}
        </View>
      })}
    </View>
    {hint(Hourglass, recoveryCopy.delay)}
    {hint(FileKey, '会保存新的恢复文件，生效后旧文件作废')}
    {errorText(error)}
    <View className="flex-row justify-between gap-2">
      <Button variant="ghost" className="-ms-3 min-h-11" onPress={device.cancelSensitive}><Text>取消</Text></Button>
      <Button className="min-h-11" disabled={busy !== null} onPress={recover}><Text>{busy ? '找回中' : '找回'}</Text></Button>
    </View>
    {busy ? <Text className="text-meta text-muted-foreground">{recoveryCopy.cancelLate}</Text> : null}
  </Panel>
}

/** Shared by waiting and recovered: the new file, saved through the share sheet, then the promise it is kept apart from its password. */
function SaveRecoveryFile({ code, saved, onSaved, save }: { code: string; saved: boolean; onSaved(saved: boolean): void; save?: { done: boolean; run(): void } }) {
  const done = !save || save.done
  return <>
    {hint(KeyRound, recoveryCopy.needsPassword)}
    {hint(FileWarning, recoveryCopy.onlyCopy)}
    {done ? <View className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <Icon as={CircleCheck} className="size-4 text-success" />
      {/* The code is what tells files apart; on a narrow screen the shared prefix gives way first. */}
      <Text accessibilityLabel={`已保存 ${recoveryFileStem(code)}`} numberOfLines={1} ellipsizeMode="head" className="min-w-0 flex-1 py-2 font-mono text-meta">
        <Text className="font-mono text-meta text-muted-foreground">{recoveryFileStem('')}</Text><Text className="font-mono text-meta font-medium tracking-wide">{code}</Text>
      </Text>
      {save ? <Button variant="ghost" className="min-h-11" onPress={save.run}><Text>重新保存</Text></Button> : null}
    </View> : <Button className="min-h-11 self-start" onPress={save.run}><Icon as={Download} /><Text>保存恢复文件</Text></Button>}
    <View className="min-h-11 flex-row items-center justify-between gap-3">
      <Text className={`min-w-0 flex-1 ${done ? '' : 'text-muted-foreground'}`}>{recoveryCopy.keptApart}</Text>
      <Switch accessibilityLabel={recoveryCopy.keptApart} value={saved} disabled={!done} onValueChange={onSaved} />
    </View>
  </>
}

/** Hands the kept file to the share sheet again; a failure stays next to the button. */
function useSaveAgain() {
  const { unsavedRecovery: file, downloadUnsavedRecovery } = useDevice()
  const [error, setError] = useState('')
  const save = file ? { done: file.attempted, run: () => { setError(''); downloadUnsavedRecovery().catch((cause) => setError(messageOf(cause))) } } : undefined
  return { save, error }
}

/** A new file this phone made and still holds, offered again until it is confirmed kept apart from its password. */
function UnsavedRecoveryFile() {
  const { unsavedRecovery, forgetUnsavedRecovery } = useDevice()
  const { save, error } = useSaveAgain()
  if (!unsavedRecovery || !save) return null
  return <View className="gap-3 border-t border-border pt-4">
    <View className="flex-row items-center gap-2"><Icon as={FileKey} className="size-4 text-muted-foreground" /><Text role="heading" className="font-medium">{recoveryCopy.saveNew}</Text></View>
    <SaveRecoveryFile code={unsavedRecovery.code} saved={false} onSaved={(saved) => { if (saved) forgetUnsavedRecovery() }} save={save} />
    {errorText(error)}
  </View>
}

/** This phone's recovery while it waits on the device it went through, or once one of the devices it would replace stopped it. */
export function RecoveryWaiting() {
  const { recovery, dismissRecovery, forgetUnsavedRecovery } = useDevice()
  if (recovery?.status === 'stopped') return <Panel icon={ShieldX} title={recoveryCopy.stopped}>
    <Text className="text-muted-foreground">{recoveryCopy.stoppedNote}</Text>
    <Button className="min-h-11 self-end" onPress={() => { forgetUnsavedRecovery(); dismissRecovery() }}><Text>知道了</Text></Button>
  </Panel>
  const at = recovery?.status === 'waiting' ? recovery.effectiveAt : null
  return <Panel icon={Hourglass} title={recoveryCopy.waiting}>
    {at === null ? null : <>
      <Text role="status" className="text-heading font-semibold" style={{ fontVariant: ['tabular-nums'] }}>{recoveryCopy.left(at)}</Text>
      {hint(CalendarClock, `${whenText(at)} ${recoveryCopy.autoEnter}`)}
    </>}
    {hint(BellRing, recoveryCopy.managersSee)}
    {/* Saving the new file is a separate thing to do now, not part of the wait. */}
    <UnsavedRecoveryFile />
  </Panel>
}

/** After a recovery took effect, before anything else: its new file gets confirmed as kept apart from its password. */
export function RecoveredStep() {
  const { directory, finishRecovered, forgetUnsavedRecovery } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  const { save, error } = useSaveAgain()
  const [saved, setSaved] = useState(false)
  return <Panel icon={FileKey} title={recoveryCopy.saveNew}>
    {hint(CircleCheck, recoveryCopy.recovered, 'text-success')}
    <SaveRecoveryFile code={code ?? ''} saved={saved} onSaved={(value) => { setSaved(value); if (value) forgetUnsavedRecovery() }} save={save} />
    {errorText(error)}
    <Button className="min-h-11 self-end" disabled={!saved} onPress={() => { forgetUnsavedRecovery(); finishRecovered() }}><Text>完成</Text></Button>
  </Panel>
}

/** A new recovery file under a new password; every older file stops working. */
export function ReplaceRecovery() {
  const device = useDevice()
  const { busy, error, status, run } = useRun()
  const [picked, setPicked] = useState<PickedFile | null>(null)
  const [oldPassword, setOldPassword] = useState('')
  const [password, setPassword] = useState('')
  const submit = () => void run('replace', async () => {
    if (!picked) throw new Error('请选择恢复文件。')
    const typed = { oldPassword, password }
    setOldPassword(''); setPassword('')
    await device.replaceRecoveryFile({ file: picked.file, oldPassword: typed.oldPassword, password: passphrase(typed.password) })
  }, recoveryCopy.replaced)
  const locked = busy !== null || Boolean(device.review)
  return <View className="gap-4">
    <Text className="text-meta text-muted-foreground">{recoveryCopy.replaceHint}</Text>
    <FilePick picked={picked} onPick={setPicked} disabled={locked} />
    <Input {...passwordProps} accessibilityLabel="旧密码" placeholder="旧密码" value={oldPassword} onChangeText={setOldPassword} editable={!locked} />
    <Input {...passwordProps} accessibilityLabel="新密码" placeholder="新密码（至少 8 个字符）" value={password} onChangeText={setPassword} editable={!locked} />
    {status ? <Text role="status" className="text-meta text-success">{status}</Text> : null}
    {errorText(error)}
    <Button className="min-h-11" disabled={locked || !picked || !oldPassword || !password} onPress={submit}>
      <Text>{busy ? '处理中' : '换密码并保存新文件'}</Text>
    </Button>
    {device.recovered ? null : <UnsavedRecoveryFile />}
  </View>
}
