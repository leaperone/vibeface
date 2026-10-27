import { AppState } from 'react-native'
import { File, Paths } from 'expo-file-system'
import * as Notifications from 'expo-notifications'
import * as SecureStore from 'expo-secure-store'
import * as Sharing from 'expo-sharing'
import { createMMKV } from 'react-native-mmkv'
import { secure, type NativeIdentity } from '@vibeface/secure-native'
import type { OutboxItem } from '@vibeface/client/lib/outbox.ts'
import type { Notifier, Permission, Platform } from '@vibeface/client/lib/platform.ts'
import { createTransport, type DeviceKeys } from '@vibeface/client/lib/transport.ts'

const store = createMMKV({ id: 'vibeface' })
const secretKey = 'vibeface.device-secret'
const createdKey = 'vibeface.device-created'
const outboxPrefix = 'outbox:'
const lostKeyMessage = '这台手机的设备密钥不见了。请用管理设备生成的加入链接重新加入，或用恢复文件找回。原任务不会因此重新执行。'

let identity: NativeIdentity | undefined
/** The device secret lives in the Keychain / Keystore, readable only by this app on this phone. */
const phoneKeys: DeviceKeys = {
  async load({ replaceLostKey }) {
    const saved = await SecureStore.getItemAsync(secretKey)
    if (saved) identity = secure.Identity.fromSecretHex(saved)
    else {
      if (store.getString(createdKey) === 'true' && !replaceLostKey) throw Object.assign(new Error(lostKeyMessage), { code: 'key_lost' })
      const created = secure.Identity.generate()
      await SecureStore.setItemAsync(secretKey, created.secretHex(), { keychainAccessible: SecureStore.AFTER_FIRST_UNLOCK_THIS_DEVICE_ONLY })
      store.set(createdKey, 'true')
      identity = created
    }
    return { deviceId: identity.deviceId() }
  },
  async remove() {
    await SecureStore.deleteItemAsync(secretKey)
    identity?.free()
    identity = undefined
  },
  async sign(message) {
    if (!identity) throw new Error('设备密钥还没准备好。')
    return identity.signMessage(message)
  },
}

Notifications.setNotificationHandler({
  handleNotification: async () => ({ shouldShowBanner: true, shouldShowList: true, shouldPlaySound: false, shouldSetBadge: false }),
})
// Its own Android channel, so system settings show Vibeface's name for these alerts; iOS ignores channels.
const channelId = 'vibeface'
void Notifications.setNotificationChannelAsync(channelId, { name: '回答与进展', importance: Notifications.AndroidImportance.HIGH }).catch(() => {})
const opens = new Map<string, () => void>()
Notifications.addNotificationResponseReceivedListener((response) => {
  const tag = response.notification.request.content.data?.tag
  if (typeof tag === 'string') opens.get(tag)?.()
})
// Android 13+ reports a never-asked permission as denied; canAskAgain tells it apart from a real refusal.
const permission = ({ status, canAskAgain }: { status: string; canAskAgain: boolean }): Permission =>
  status === 'undetermined' || (status === 'denied' && canAskAgain) ? 'default' : status as Permission

const notifier: Notifier = {
  name: '系统通知',
  permission: async () => permission(await Notifications.getPermissionsAsync()),
  request: async () => permission(await Notifications.requestPermissionsAsync()),
  show({ title, body, tag }, open) {
    opens.set(tag, open)
    void Notifications.scheduleNotificationAsync({ identifier: tag, content: { title, body, data: { tag } }, trigger: { channelId } })
  },
}

export const mobilePlatform: Platform = {
  secure,
  transport: createTransport(secure, phoneKeys),
  storage: { getItem: (key) => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) },
  outbox: {
    all: async () => store.getAllKeys().filter((key) => key.startsWith(outboxPrefix)).map((key) => JSON.parse(store.getString(key)!) as OutboxItem),
    put: async (item) => store.set(outboxPrefix + item.key, JSON.stringify(item)),
    delete: async (key) => { store.remove(outboxPrefix + key) },
  },
  notifier,
  onForeground(listener) {
    const subscription = AppState.addEventListener('change', (state) => { if (state === 'active') listener() })
    return () => subscription.remove()
  },
  foreground: () => AppState.currentState === 'active',
  async saveRecovery(bytes, name) {
    // Kept in the app's documents: the share sheet reports success even when dismissed, and the old file stops working.
    const file = new File(Paths.document, name)
    if (file.exists) file.delete()
    file.write(bytes)
    await Sharing.shareAsync(file.uri, { mimeType: 'application/json', dialogTitle: '保存恢复文件' })
  },
  self: '这台手机',
}
