import type { SecureApi } from '../secure-api.ts'
import type { ConnectionEntry, LocalSetup, PublicIdentity, ReadyConnection } from './node.ts'
import type { OutboxStore } from './outbox.ts'
import type { Transport } from './transport.ts'
import type { NotificationRecord } from '../workspace/types.ts'
import type { TransferFile } from '../delivery/types.ts'

/** Synchronous string storage that survives restarts, like the browser's `localStorage`. */
export type KeyValue = { getItem(key: string): string | null; setItem(key: string, value: string): void }
export type NotificationDestination = { network_id: string; device_id: string; notification_id: string; conversation_id: string }
export type Permission = 'granted' | 'denied' | 'default' | 'unsupported'
/** Alerts shown by the system while the app is open. */
export type Notifier = {
  /** What the person turns on, such as 浏览器提醒. */
  name: string
  permission(): Promise<Permission>
  request(): Promise<Permission>
  show(alert: { title: string; body: string; tag: string; destination: NotificationDestination }, open: () => void): void
  /** Retains an opening until the current workspace consumes it. False keeps it for a later connection. */
  watchOpen?(listener: (destination: NotificationDestination) => Promise<boolean>): () => void
  replayOpen?(): void
}
/** `rejoin`: a removed device asked to join again once the launch connects. */
export type LocalLaunch = { token: string; deviceId: string; rejoin: boolean }
/** A browser page served by a Node itself: only it may set up, launch or join that computer. */
export type LocalPage = {
  origin: string
  /** The invitation or local launch in the address the page opened with, read once and cleared from it. */
  takeLaunch(): { invitation: string; localLaunch: LocalLaunch | null }
  /** Calls `listener` with each invitation opened later in this page; a link differing only in its hash does not reload it. */
  watchInvitation(listener: (invitation: string) => void): () => void
  /** This computer's setup on localhost, `null` anywhere else. */
  discoverLocalSetup(signal: AbortSignal): Promise<LocalSetup | null>
  /** The cookie-bound local session of this page, exchanged from `launch` when given. */
  /** Rejects when the computer does not answer; `null` when it answers without a session for this device. */
  localEntry(identity: PublicIdentity, launch: LocalLaunch | null): Promise<ConnectionEntry | null>
  /** Opens the local launch address, reloading when only its hash differs. */
  open(url: string): void
}
/** A Node advertising itself on this LAN: its rotating tag and where it listens (`ip:port`). */
export type LanSighting = { tag: string; addrs: string[] }
/** How the app finds Nodes on its LAN; it reaches them with the transport's own LAN dial. */
export type Lan = {
  /** Calls `listener` with every Node in sight each time that changes, while the app is in the foreground. */
  watch(listener: (seen: LanSighting[]) => void): () => void
  /** Whether `tag` is what Node `deviceId` of network `networkId` advertises around now. */
  names(tag: string, networkId: string, deviceId: string): boolean
}
/** Instances of the app on one device that share its storage; one leads and settles the outbox. */
type JoinTabs = <T>(name: string, handlers: { lead(): void | (() => void); message(value: T): void }) => { post(value: T): void; close(): void }

/** What differs between the browser and the phone; everything else in this package is shared. */
export type Platform = {
  secure: SecureApi
  transport: Transport
  storage: KeyValue
  outbox: OutboxStore
  notifier: Notifier
  /** Whether this surface has a route for session handoff alerts. */
  sessionNotifications?: boolean
  /** Calls `listener` each time the app comes back to the foreground. */
  onForeground(listener: () => void): () => void
  foreground(): boolean
  /** Opens a sealed file's download or share; completion does not confirm an external saved copy. */
  saveRecovery(bytes: Uint8Array, name: string): void | Promise<void>
  /** A sealed pending file and its index, stored together before recovery is committed. Web uses tab storage. */
  recoveryStorage?: { read(): string | null; write(value: string | null): void }
  files?: {
    pick(): Promise<TransferFile | null>
    save(bytes: Uint8Array, name: string): Promise<'saved' | 'handed_off'>
  }
  /** How messages name this device: 这个浏览器, 这台手机. */
  self: string
  /** Deletes this device's key and app-owned local data. External originals and exported copies stay with the person. */
  erase?(): Promise<void>
  page?: LocalPage
  joinTabs?: JoinTabs
  /** Serializes local input persistence across tabs independently of network delivery. */
  inputLock?<T>(name: string, save: () => Promise<T>): Promise<T>
  /** The app's: a browser cannot reach Nodes on its LAN directly. */
  lan?: Lan
}

/** A persisted OS target must be found again in the current authenticated network. */
export async function notificationForOpening(args: {
  destination: NotificationDestination
  connection: ReadyConnection
  current(): boolean
  read(): Promise<NotificationRecord[]>
}): Promise<{ kind: 'deferred' } | { kind: 'invalid' } | { kind: 'target'; notification: NotificationRecord }> {
  const { destination, connection, current, read } = args
  if (!current() || destination.network_id !== connection.principal.network_id || connection.info.network_id !== destination.network_id) return { kind: 'deferred' }
  const items = await read()
  if (!current()) return { kind: 'deferred' }
  const notification = items.find(item => item.notification_id === destination.notification_id && item.conversation_id === destination.conversation_id)
  return notification ? { kind: 'target', notification } : connection.catchingUp?.() ? { kind: 'deferred' } : { kind: 'invalid' }
}
