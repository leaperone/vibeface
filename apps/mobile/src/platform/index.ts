import { AppState, Platform as Native } from 'react-native'
import { File } from 'expo-file-system'
import * as DocumentPicker from 'expo-document-picker'
import * as Notifications from 'expo-notifications'
import * as SecureStore from 'expo-secure-store'
import * as Sharing from 'expo-sharing'
import { createMMKV } from 'react-native-mmkv'
import { lan as nativeLan, secure, type NativeIdentity } from '@vibeface/secure-native'
import { mobileOutbox } from './outbox'
import type { Lan, Notifier, Permission, Platform } from '@vibeface/client/lib/platform.ts'
import { createTransport, type DeviceKeys } from '@vibeface/client/lib/transport.ts'
import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import { lostKeyText } from '@vibeface/client/identity/copy.ts'
import { notificationPreferences } from '@vibeface/client/workspace/system-notifications.ts'
import LanBrowser from '../../modules/lan-browser'
import { privateFiles } from './files'
import { notificationOpenings } from './notification-opens'
import { nativeDeliveryReservations } from './delivery-reservations'

const store = createMMKV({ id: 'vibeface' })
const notificationStorage = { getItem: (key: string) => store.getString(key) ?? null, setItem: (key: string, value: string) => store.set(key, value) }
const secretKey = 'vibeface.device-secret'
const createdKey = 'vibeface.device-created'
const outbox = mobileOutbox(store, secure, privateFiles)
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
  handleNotification: async (notification) => {
    const destination: unknown = notification.request.content.data?.destination
    const networkId = destination && typeof destination === 'object' && 'network_id' in destination && typeof destination.network_id === 'string' ? destination.network_id : ''
    const settings = notificationPreferences(notificationStorage, networkId).read()
    const muted = destination && typeof destination === 'object' && 'conversation_id' in destination && typeof destination.conversation_id === 'string' && settings.mutedConversations.includes(destination.conversation_id)
    return { shouldShowBanner: settings.enabled && !muted, shouldShowList: settings.enabled && !muted, shouldPlaySound: settings.enabled && settings.sound && !muted, shouldSetBadge: settings.enabled && settings.badge }
  },
})
// Its own Android channel, so system settings show Vibeface's name for these alerts; iOS ignores channels.
const channelId = 'vibeface'
const silentChannelId = 'vibeface-silent'
void Notifications.setNotificationChannelAsync(channelId, { name: '回答与进展', importance: Notifications.AndroidImportance.HIGH, sound: 'default' }).catch(() => {})
void Notifications.setNotificationChannelAsync(silentChannelId, { name: '回答与进展（静音）', importance: Notifications.AndroidImportance.HIGH, sound: null, enableVibrate: false }).catch(() => {})
const openings = notificationOpenings({ getItem: key => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) })
const receiveOpening = (response: Notifications.NotificationResponse) => {
  const { request } = response.notification
  openings.receive(`${request.identifier}:${response.notification.date}:${response.actionIdentifier}`, request.content.data?.destination)
}
// Android 13+ reports a never-asked permission as denied; canAskAgain tells it apart from a real refusal.
const permission = ({ status, canAskAgain }: { status: string; canAskAgain: boolean }): Permission =>
  status === 'undetermined' || (status === 'denied' && canAskAgain) ? 'default' : status as Permission

const notifier: Notifier = {
  name: '系统通知',
  permission: async () => permission(await Notifications.getPermissionsAsync()),
  request: async () => permission(await Notifications.requestPermissionsAsync()),
  watchOpen(listener) {
    const stop = openings.watch(listener)
    const response = Notifications.addNotificationResponseReceivedListener(receiveOpening)
    const foreground = AppState.addEventListener('change', state => { if (state === 'active') void openings.flush() })
    let active = true
    void Notifications.getLastNotificationResponseAsync().then(response => { if (active && response) receiveOpening(response) }, () => {})
    return () => { active = false; response.remove(); foreground.remove(); stop() }
  },
  replayOpen() { void openings.flush() },
  async show({ title, body, tag, destination }) {
    const settings = notificationPreferences(notificationStorage, destination.network_id).read()
    await privateFiles.run(async (current) => {
      if (!current()) throw new Error('应用内数据正在清除。')
      await Notifications.scheduleNotificationAsync({ identifier: tag, content: { title, body, data: { destination }, ...(settings.sound ? { sound: 'default' } : {}) }, trigger: { channelId: settings.sound ? channelId : silentChannelId } })
    })
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
  conversationCache: true,
  explicitMessageRead: true,
  secure,
  transport,
  storage: { getItem: (key) => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) },
  reserveDeliveryIntent: nativeDeliveryReservations,
  outbox,
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
        let candidate: File | undefined
        try {
          try {
            if (!current()) return null
            const file = await privateFiles.retain(asset.uri)
            candidate = file
            if (!current()) { privateFiles.release(file.uri); candidate = undefined; return null }
            return { ...privateFiles.ownedFile(file), name: asset.name }
          } finally { privateFiles.releasePicker(asset.uri) }
        } catch (cause) {
          try { if (candidate) privateFiles.release(candidate.uri) }
          catch (cleanup) { throw new AggregateError([cause, cleanup], '未能选择文件或清理选择副本。') }
          throw cause
        }
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
        if (saved.length !== bytes.length || await sha256Hex(saved) !== await sha256Hex(bytes)) throw new Error('保存后的文件对不上，重新取回再试。')
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
      await Notifications.clearLastNotificationResponseAsync()
      openings.clear()
      store.clearAll()
    })
    outbox.reset()
  },
}
