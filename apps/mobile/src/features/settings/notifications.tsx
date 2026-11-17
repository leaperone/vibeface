import * as Notifications from 'expo-notifications'
import { BellRing, Bot, CircleDot, Settings, Volume2 } from 'lucide-react-native'
import { useEffect, useMemo, useState } from 'react'
import { Linking, View } from 'react-native'
import { notificationPreferences, type NotificationPreferences } from '@vibeface/client/workspace/system-notifications.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useLive } from '@/data/live'
import { useWorkspace } from '@/data/context'
import { SettingsRow } from '@/components/kit'
import { Switch } from '@/components/arc/switch'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { ErrorText } from '@/components/action'
import { mobilePlatform } from '@/platform'
import { remoteNotifications, type RemoteNotificationState } from '@/platform/remote-notifications'
import { useRun } from '@/lib/use-run'

export function NotificationSettings() {
  const device = useDevice(), workspace = useWorkspace(), shared = useLive()
  const networkId = workspace.networkId ?? ''
  const preferences = useMemo(() => notificationPreferences(mobilePlatform.storage, networkId), [networkId])
  const [, refresh] = useState(0)
  const settings = preferences.read()
  const { busy, error, run } = useRun()
  const ready = device.ready
  const remote: RemoteNotificationState = ready ? remoteNotifications.state(ready) : { kind: 'off' }
  useEffect(() => remoteNotifications.watch(() => refresh((value) => value + 1)), [ready])
  const save = async (next: NotificationPreferences) => {
    if (!ready?.isCurrent() || !shared) throw new Error('连上网络后再同步提醒设置。')
    if (next.enabled && !settings.enabled) {
      await shared.enableNotifications()
      if (await mobilePlatform.notifier.permission() !== 'granted') throw new Error('系统通知未获允许，请在系统设置中开启。')
    }
    preferences.write(next)
    refresh((value) => value + 1)
    if (!next.badge || !next.enabled) await Notifications.setBadgeCountAsync(0)
    const state = await remoteNotifications.sync(ready, next)
    if (state.kind === 'error') throw new Error(state.message)
  }
  const disabled = busy !== null || !ready || !networkId
  return <>
    <SettingsRow icon={BellRing} label="接收此网络的提醒" description={shared?.notificationButton.state === 'denied' ? '系统权限未开启，请前往系统设置。' : 'Bot 回复和需要你决定的事项。'}>
      <Switch accessibilityLabel="接收此网络的提醒" value={settings.enabled} disabled={disabled} onValueChange={(enabled) => void run('notifications', () => save({ ...settings, enabled }))} />
    </SettingsRow>
    <SettingsRow icon={Volume2} label="提醒声音"><Switch accessibilityLabel="提醒声音" value={settings.sound} disabled={disabled} onValueChange={(sound) => void run('notification-sound', () => save({ ...settings, sound }))} /></SettingsRow>
    <SettingsRow icon={CircleDot} label="应用未读角标"><Switch accessibilityLabel="应用未读角标" value={settings.badge} disabled={disabled} onValueChange={(badge) => void run('notification-badge', () => save({ ...settings, badge }))} /></SettingsRow>
    {settings.enabled ? <View className="gap-2 px-4 py-3"><Text className="text-sm text-muted">{remote.kind === 'registered' ? '后台提醒已连接，系统通知设置仍会影响展示。' : remote.kind === 'unconfigured' ? '此安装包尚未启用后台提醒。打开应用后仍可接收消息。' : remote.kind === 'error' ? remote.message : '正在连接后台提醒。'}</Text>
      {remote.kind !== 'registered' && remote.kind !== 'unconfigured' ? <Button variant="secondary" size="sm" disabled={disabled} onPress={() => void run('push-retry', () => save(settings))}><Text>重新连接提醒</Text></Button> : null}
    </View> : null}
    {workspace.bots.map((bot) => <SettingsRow icon={Bot} key={bot.bot_id} label={bot.name} description="接收这个 Bot 的提醒">
      <Switch accessibilityLabel={`接收 ${bot.name} 的提醒`} value={!settings.mutedConversations.includes(bot.conversation_id)} disabled={disabled} onValueChange={(enabled) => void run(`mute:${bot.bot_id}`, () => save({ ...settings, mutedConversations: enabled ? settings.mutedConversations.filter((id) => id !== bot.conversation_id) : [...settings.mutedConversations, bot.conversation_id] }))} />
    </SettingsRow>)}
    <SettingsRow icon={Settings} label="系统通知设置" onPress={() => void run('system-settings', () => Linking.openSettings())}><Icon as={Settings} className="text-muted" /></SettingsRow>
    {error ? <View className="px-4 pb-3"><ErrorText error={error} /></View> : null}
  </>
}
