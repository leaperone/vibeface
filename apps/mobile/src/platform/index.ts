import { AppState, Platform as Native } from 'react-native'
import { File } from 'expo-file-system'
import * as DocumentPicker from 'expo-document-picker'
import * as Notifications from 'expo-notifications'
import * as SecureStore from 'expo-secure-store'
import * as Sharing from 'expo-sharing'
import { createMMKV } from 'react-native-mmkv'
import { lan as nativeLan, secure, type NativeIdentity } from '@vibeface/secure-native'
import type { OutboxItem } from '@vibeface/client/lib/outbox.ts'
import type { Lan, Notifier, Permission, Platform } from '@vibeface/client/lib/platform.ts'
import { createTransport, type DeviceKeys } from '@vibeface/client/lib/transport.ts'
import { sha256 } from '@vibeface/client/delivery/model.ts'
import { lostKeyText } from '@vibeface/client/identity/copy.ts'
import LanBrowser from '../../modules/lan-browser'
import { privateFiles } from './files'

const store = createMMKV({ id: 'vibeface' })
const secretKey = 'vibeface.device-secret'
const createdKey = 'vibeface.device-created'
const outboxPrefix = 'outbox:'
/** The same app runs on iPad, which calls itself a tablet, as the shared device forms do. */
export const tablet = Native.OS === 'ios' && Native.isPad
export const self = tablet ? '这台平板' : '这台手机'
export const defaultName = tablet ? '我的 iPad' : '我的手机'
const lostKeyMessage = lostKeyText(self)

let identity: NativeIdentity | undefined
/** The device secret lives in the Keychain / Keystore, readable only by this app on this device. */
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
    if (!identity) throw new Error(`${self}还没准备好，重新打开 Vibeface 再试。`)
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
  if (typeof tag === 'string') { const open = opens.get(tag); opens.delete(tag); open?.() }
})
// Android 13+ reports a never-asked permission as denied; canAskAgain tells it apart from a real refusal.
const permission = ({ status, canAskAgain }: { status: string; canAskAgain: boolean }): Permission =>
  status === 'undetermined' || (status === 'denied' && canAskAgain) ? 'default' : status as Permission

const notifier: Notifier = {
  name: '系统通知',
  permission: async () => permission(await Notifications.getPermissionsAsync()),
  request: async () => permission(await Notifications.requestPermissionsAsync()),
  show({ title, body, tag }, open) {
    opens.delete(tag)
    opens.set(tag, open)
    while (opens.size > 1000) { const first = opens.keys().next().value; if (first !== undefined) opens.delete(first) }
    void privateFiles.run(async (current) => { if (current()) await Notifications.scheduleNotificationAsync({ identifier: tag, content: { title, body, data: { tag } }, trigger: { channelId } }) }).catch(() => { if (opens.get(tag) === open) opens.delete(tag) })
  },
}

/** Nodes on this LAN over mDNS, browsed only while the app is in the foreground. */
const lan: Lan = {
  watch(listener) {
    let browsing: { remove(): void } | null = null
    const follow = (state: string | null | undefined) => {
      if (state === 'active' && !browsing) { nativeLan.prepare(); browsing = LanBrowser.addListener('onChange', ({ services }) => listener(services)) }
      if (state === 'background' && browsing) { browsing.remove(); browsing = null; listener([]) }
    }
    follow(AppState.currentState)
    const app = AppState.addEventListener('change', follow)
    return () => { app.remove(); browsing?.remove() }
  },
  names: nativeLan.names,
}

const transport = createTransport(secure, phoneKeys, nativeLan.dial)

export const mobilePlatform: Platform = {
  secure,
  transport,
  storage: { getItem: (key) => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) },
  outbox: {
    all: async () => store.getAllKeys().filter((key) => key.startsWith(outboxPrefix)).map((key) => JSON.parse(store.getString(key)!) as OutboxItem),
    put: async (item) => store.set(outboxPrefix + item.key, JSON.stringify(item)),
    delete: async (key) => { store.remove(outboxPrefix + key) },
  },
  notifier,
  sessionNotifications: false,
  onForeground(listener) {
    const subscription = AppState.addEventListener('change', (state) => { if (state === 'active') listener() })
    return () => subscription.remove()
  },
  foreground: () => AppState.currentState === 'active',
  recoveryStorage: privateFiles.recoveryStorage,
  async saveRecovery(bytes, name) {
    await privateFiles.run(async (current) => {
      privateFiles.recovery.create({ idempotent: true, intermediates: true })
      const file = new File(privateFiles.recovery, name)
      await file.write(bytes)
      if (!current()) return
      if (!await Sharing.isAvailableAsync()) throw new Error('这个设备无法打开分享。应用内已保留恢复副本，请稍后重新另存。')
      if (current()) await Sharing.shareAsync(file.uri, { mimeType: 'application/json', dialogTitle: '另存恢复文件副本' })
    })
  },
  files: {
    async pick() {
      return privateFiles.run(async (current) => {
        const picked = await DocumentPicker.getDocumentAsync({ copyToCacheDirectory: true, multiple: false })
        const asset = picked.assets?.[0]
        if (picked.canceled || !asset) return null
        try {
          if (!current()) return null
          const file = privateFiles.retain(asset.uri)
          return { name: asset.name, size: file.size, arrayBuffer: () => privateFiles.run(async () => file.arrayBuffer()) }
        } finally { privateFiles.releasePicker(asset.uri) }
      })
    },
    async save(bytes, name) {
      return privateFiles.run<'saved'>(async (current) => {
        const directory = privateFiles.transfers
        directory.create({ idempotent: true, intermediates: true })
        const file = new File(directory, `${crypto.randomUUID()}-${name}`)
        file.create()
        await file.write(bytes)
        const saved = await file.bytes()
        if (saved.length !== bytes.length || await sha256(saved) !== await sha256(bytes)) throw new Error('保存后的文件对不上，重新取回再试。')
        if (current() && await Sharing.isAvailableAsync() && current()) await Sharing.shareAsync(file.uri, { mimeType: 'application/octet-stream', dialogTitle: '导出文件副本' })
        return 'saved'
      })
    },
  },
  self,
  lan,
  async erase() {
    await privateFiles.erase(async () => {
      await transport.forgetClient()
      await Notifications.dismissAllNotificationsAsync()
      opens.clear()
      store.clearAll()
    })
  },
}
