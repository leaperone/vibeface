import { Stack, router, useLocalSearchParams } from 'expo-router'
import { ArrowRightLeft, Bot, Check, Crown, MonitorSmartphone, Pencil, Sparkles, Trash2, X } from 'lucide-react-native'
import { useState } from 'react'
import { Pressable, ScrollView, View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { deviceCopy } from '@vibeface/client/identity/copy.ts'
import { ErrorText } from '@/components/action'
import { CopyableId, Details, EmptyState, Online, SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useWorkspace } from '@/data/context'
import { deviceIcon } from '@/features/devices/device-icon'
import { self } from '@/platform'
import { useRun } from '@/lib/use-run'

const tag = 'flex-row items-center gap-1 rounded-md bg-muted px-1.5'

export default function DeviceDetail() {
  const { deviceId } = useLocalSearchParams<{ deviceId: string }>()
  const device = useDevice()
  const { members } = useWorkspace()
  const member = members.find((item) => item.device_id === deviceId)
  const { busy, error, run } = useRun()
  const switching = useRun()
  const [removing, setRemoving] = useState(false)
  if (!member) return <EmptyState icon={MonitorSmartphone} title="找不到这个设备" className="flex-1 justify-center" />
  const manageable = device.ready?.principal.can_manage === true && member.kind === 'client' && !member.current
  const removable = device.ready?.principal.can_manage === true && !member.admin && !member.current && !member.entry
  const saved = device.entries.find((item) => item.device_id === member.device_id)
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Stack.Screen options={{ title: '' }} />
    <View className="flex-row items-center gap-4">
      <View className="size-12 items-center justify-center rounded-2xl bg-muted ring-1 ring-border"><Icon as={deviceIcon(member)} className="size-6" /></View>
      <View className="min-w-0 flex-1 gap-1">
        <DeviceTitle deviceId={member.device_id} name={member.name} editable={device.ready?.principal.can_manage === true} />
        <View className="flex-row flex-wrap items-center gap-2">
          {member.online !== undefined ? <Online online={member.online} /> : null}
          {member.current || member.entry ? <View className={tag}><Text className="text-meta text-muted-foreground">{member.current ? self : '当前连接'}</Text></View> : null}
          {member.admin ? <View className={tag}><Icon as={Crown} className="size-3.5 text-muted-foreground" /><Text className="text-meta text-muted-foreground">管理</Text></View> : null}
        </View>
        {member.online === false ? <Text className="text-meta text-muted-foreground">{deviceCopy.offline}</Text> : null}
      </View>
    </View>
    {/* The AI page sets up the computer this phone connects through; any other computer shows how its bot stands, as on the web. */}
    {member.kind === 'node' ? <SettingsGroup>
      {member.entry && device.ready?.principal.can_manage ? <SettingsRow href="/ai" icon={Sparkles} label="Bot 与任务" />
        : <SettingsRow icon={Bot} label={deviceCopy.botStatus[deviceCopy.botState(member.bot, member.online)]} />}
      {!member.entry && device.ready?.principal.can_manage && saved ? <SettingsRow icon={ArrowRightLeft} label="切换到它" disabled={switching.busy !== null} onPress={() => void switching.run('switch', () => device.connect(saved))}>
        {switching.error ? <View className="basis-full ps-11"><ErrorText error={switching.error} /></View> : null}
      </SettingsRow> : null}
    </SettingsGroup> : null}
    {manageable || removable ? <SettingsGroup>
      {manageable ? <SettingsRow icon={Crown} label="管理设备" description={deviceCopy.manageHint}>
        <Switch accessibilityLabel="管理设备" value={member.admin} disabled={busy !== null || member.admin && !members.some((item) => item.admin && item.device_id !== member.device_id)}
          onValueChange={(manage) => void run('manage', () => device.setManage(member.device_id, manage))} />
        {error && busy === null && !removing ? <View className="basis-full ps-11"><ErrorText error={error} /></View> : null}
      </SettingsRow> : null}
      {removable ? <SettingsRow icon={Trash2} tone="danger" label="移除设备" onPress={removing ? undefined : () => setRemoving(true)}>
        {removing ? <View className="basis-full gap-3 ps-11">
          <Text>{deviceCopy.removeNotice(member.bot !== null)}</Text>
          <ErrorText error={error} />
          <View className="flex-row flex-wrap justify-end gap-2">
            <Button variant="ghost" onPress={() => setRemoving(false)}><Text>取消</Text></Button>
            <Button variant="destructive" disabled={busy !== null} onPress={() => void run('remove', async () => { await device.revoke(member.device_id); router.back() })}>
              <Icon as={Trash2} /><Text>{busy ? '移除中' : '移除'}</Text>
            </Button>
          </View>
        </View> : null}
      </SettingsRow> : null}
    </SettingsGroup> : null}
    <Details>
      <Text className="text-meta text-muted-foreground">设备编号</Text>
      <CopyableId value={member.device_id} label="设备编号" />
    </Details>
  </ScrollView>
}

/** The name heads the device page; a management device taps it to rename the device in place, as on the web. */
function DeviceTitle({ deviceId, name, editable }: { deviceId: string; name: string; editable: boolean }) {
  const device = useDevice()
  const { busy, error, run } = useRun()
  const [draft, setDraft] = useState<string | null>(null)
  if (draft === null) return editable
    ? <Pressable accessibilityRole="button" accessibilityLabel={`改名 ${name}`} onPress={() => setDraft(name)} className="min-h-11 flex-row items-center gap-2">
      <Text role="heading" className="shrink text-title font-semibold">{name}</Text>
      <Icon as={Pencil} className="size-4 text-muted-foreground" />
    </Pressable>
    : <Text role="heading" className="text-title font-semibold">{name}</Text>
  const save = () => {
    const next = draft.trim()
    if (next === name) return setDraft(null)
    void run('rename', async () => { await device.renameDevice(deviceId, next); setDraft(null) })
  }
  return <View className="gap-1">
    <View className="flex-row items-center gap-1">
      <Input accessibilityLabel="设备名称" autoFocus maxLength={128} className="min-w-0 flex-1" value={draft} editable={busy === null}
        onChangeText={setDraft} onSubmitEditing={save} returnKeyType="done" />
      <Button variant="ghost" size="icon" accessibilityLabel="保存" disabled={busy !== null || !draft.trim()} onPress={save}><Icon as={Check} /></Button>
      <Button variant="ghost" size="icon" accessibilityLabel="取消" disabled={busy !== null} onPress={() => setDraft(null)}><Icon as={X} /></Button>
    </View>
    <ErrorText error={error} />
  </View>
}
