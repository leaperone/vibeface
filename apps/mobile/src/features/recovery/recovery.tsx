import * as DocumentPicker from 'expo-document-picker'
import { File } from 'expo-file-system'
import { ArchiveRestore, FileCheck, FileKey, Hourglass, Laptop, Smartphone, type LucideIcon } from 'lucide-react-native'
import { useState } from 'react'
import { Switch, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { recoveryCopy } from '@vibeface/client/identity/copy.ts'
import { parseEntry, type Member } from '@vibeface/client/lib/node.ts'
import { passphrase, type RecoveryFile } from '@vibeface/client/lib/secure.ts'
import { Details } from '@/components/kit'
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
const hint = (icon: LucideIcon, text: string) => <View className="flex-row items-center gap-2"><Icon as={icon} className="size-4 text-muted-foreground" /><Text className="min-w-0 flex-1 text-meta text-muted-foreground">{text}</Text></View>

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
  const [admins, setAdmins] = useState<string[] | null>(null)
  if (review?.kind !== 'recovery') return null
  const managers = admins ?? review.members.filter((member) => member.manage).map((member) => member.device_id)
  const toggle = (list: string[], id: string, on: boolean) => on ? [...list, id] : list.filter((item) => item !== id)
  const recover = () => void run('recover', () => device.confirmRecovery(review.members
    .filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id))
    .map((member): Member => ({ ...member, manage: member.kind === 'client' && managers.includes(member.device_id) }))))
  return <View className="gap-4">
    <View className="flex-row items-center gap-2">
      <Icon as={ArchiveRestore} className="text-muted-foreground" />
      <Text role="heading" className="text-heading font-semibold">{recoveryCopy.whichLost}</Text>
    </View>
    <View className="gap-2">
      {review.members.map((member) => {
        const required = member.device_id === review.requiredNode
        const gone = lost.includes(member.device_id)
        return <View key={member.device_id} className="gap-1 rounded-lg bg-muted/60 px-3 py-2">
          <View className="min-h-11 flex-row items-center gap-2">
            <Icon as={member.kind === 'client' ? Smartphone : Laptop} className="size-4 text-muted-foreground" />
            <Text numberOfLines={1} className="min-w-0 flex-1">{member.name}</Text>
            {required ? <Text className="text-meta text-muted-foreground">当前连接</Text> : <>
              <Text className="text-meta">丢了</Text>
              <Switch accessibilityLabel={`${member.name} 丢了`} value={gone} disabled={busy !== null} onValueChange={(on) => setLost(toggle(lost, member.device_id, on))} />
            </>}
          </View>
          {member.kind === 'client' && !gone ? <View className="min-h-11 flex-row items-center justify-end gap-2">
            <Text className="text-meta text-muted-foreground">管理设备</Text>
            <Switch accessibilityLabel={`${member.name} 管理设备`} value={managers.includes(member.device_id)} disabled={busy !== null} onValueChange={(on) => setAdmins(toggle(managers, member.device_id, on))} />
          </View> : null}
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
  </View>
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
  </View>
}
