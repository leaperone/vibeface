import init, * as wasm from '../../generated/vibeface_secure.js'
import type { Notifier, Platform } from '@vibeface/client/lib/platform.ts'
import { createTransport } from '@vibeface/client/lib/transport.ts'
import type { DeviceKeys } from '@vibeface/client/lib/transport.ts'
import type { SecureApi } from '@vibeface/client/secure-api.ts'
import { lostKeyText } from '@vibeface/client/identity/copy.ts'
import { indexedOutbox } from './indexed-outbox'
import { localPage } from './local-page'
import { joinTabs } from './tabs'

const secure: SecureApi = wasm
const lostKeyMessage = lostKeyText('这个浏览器')
const hex = (bytes: ArrayBuffer) => [...new Uint8Array(bytes)].map(value => value.toString(16).padStart(2, '0')).join('')
type StoredKey = { privateKey: CryptoKey; deviceId: string; created_at: number }

function database() {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open('vibeface-device', 1)
    request.onupgradeneeded = () => request.result.createObjectStore('keys')
    request.onsuccess = () => resolve(request.result)
    request.onerror = () => reject(new Error('无法打开这个浏览器的存储。允许这个网站保存数据后重试。'))
  })
}

/** Runs one transaction on the key store; `operation` returns the request whose result is wanted. */
async function keys<T>(mode: IDBTransactionMode, operation: (store: IDBObjectStore) => IDBRequest<T>): Promise<T | false | undefined> {
  const db = await database()
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction('keys', mode)
      const request = operation(transaction.objectStore('keys'))
      let result: T | false | undefined
      request.onsuccess = () => { result = request.result }
      request.onerror = (event) => {
        if (request.error?.name === 'ConstraintError') { result = false; event.preventDefault(); event.stopPropagation() }
      }
      transaction.oncomplete = () => resolve(result)
      transaction.onerror = () => reject(new Error('无法在这个浏览器里保存登录信息。允许这个网站保存数据后重试。'))
      transaction.onabort = transaction.onerror
    })
  } finally { db.close() }
}

let device: StoredKey | undefined
/** The device key is a non-extractable Ed25519 CryptoKey; page scripts can only ask it to sign. */
const browserKeys: DeviceKeys = {
  async load({ replaceLostKey }) {
    if (!globalThis.isSecureContext || !globalThis.crypto?.subtle || !globalThis.indexedDB) throw new Error('这个地址不能安全保存登录信息。在这台电脑上直接打开，或换成 HTTPS 地址。')
    await init()
    let record = await keys<StoredKey | undefined>('readonly', store => store.get('device'))
    if (!record) {
      let created = false
      try { created = localStorage.getItem('vibeface.device-created') === 'true' } catch {}
      if (created && !replaceLostKey) throw Object.assign(new Error(lostKeyMessage), { code: 'key_lost' })
      let pair: CryptoKeyPair
      try { pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']) as CryptoKeyPair }
      catch { throw new Error('这个浏览器版本太旧。更新浏览器后重试。') }
      const candidate: StoredKey = { privateKey: pair.privateKey, deviceId: hex(await crypto.subtle.exportKey('raw', pair.publicKey)), created_at: Date.now() }
      await keys('readwrite', store => store.add(candidate, 'device'))
      record = await keys<StoredKey | undefined>('readonly', store => store.get('device'))
      try { localStorage.setItem('vibeface.device-created', 'true') } catch {}
    }
    if (!record || !(record.privateKey instanceof CryptoKey) || record.privateKey.extractable) throw new Error('这个浏览器保存的登录信息坏了。需要重新加入。')
    device = record
    return { deviceId: record.deviceId }
  },
  async remove() { await keys('readwrite', store => store.delete('device')); device = undefined },
  async sign(message) {
    if (!device) throw new Error('这个浏览器还没准备好。刷新页面再试。')
    return hex(await crypto.subtle.sign({ name: 'Ed25519' }, device.privateKey, new Uint8Array(message)))
  },
}

const notifier: Notifier = {
  name: '浏览器提醒',
  async permission() { return 'Notification' in globalThis ? Notification.permission : 'unsupported' },
  async request() { return 'Notification' in globalThis ? Notification.requestPermission() : 'unsupported' },
  show({ title, body, tag }, open) {
    const popup = new Notification(title, { body, tag })
    popup.onclick = () => { window.focus(); open(); popup.close() }
  },
}

export const webPlatform: Platform = {
  secure,
  transport: createTransport(secure, browserKeys),
  storage: { getItem: key => localStorage.getItem(key), setItem: (key, value) => localStorage.setItem(key, value) },
  outbox: indexedOutbox(),
  notifier,
  onForeground(listener) {
    const visible = () => { if (!document.hidden) listener() }
    document.addEventListener('visibilitychange', visible)
    return () => document.removeEventListener('visibilitychange', visible)
  },
  foreground: () => !document.hidden,
  saveRecovery(bytes, name) {
    const url = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: 'application/json' }))
    const link = document.createElement('a')
    link.href = url
    link.download = name
    link.click()
    setTimeout(() => URL.revokeObjectURL(url), 1000)
  },
  files: {
    pick: () => new Promise((resolve, reject) => {
      const input = document.createElement('input')
      input.type = 'file'
      input.hidden = true
      const finish = () => { const file = input.files?.[0] ?? null; input.remove(); resolve(file) }
      input.addEventListener('change', finish, { once: true })
      input.addEventListener('cancel', finish, { once: true })
      document.body.append(input)
      try { input.click() } catch (cause) { input.remove(); reject(cause) }
    }),
    async save(bytes, name) {
      const url = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: 'application/octet-stream' }))
      const link = document.createElement('a')
      link.href = url; link.download = name
      document.body.append(link); link.click(); link.remove()
      setTimeout(() => URL.revokeObjectURL(url), 1000)
      return 'handed_off'
    },
  },
  self: '这个浏览器',
  page: localPage,
  get joinTabs() { return typeof BroadcastChannel !== 'undefined' && globalThis.navigator?.locks ? joinTabs : undefined },
}
