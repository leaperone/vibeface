import type { SecureApi } from '../secure-api.ts'
import type { ConnectionEntry, LocalSetup, PublicIdentity } from './node.ts'
import type { OutboxStore } from './outbox.ts'
import type { Transport } from './transport.ts'

/** Synchronous string storage that survives restarts, like the browser's `localStorage`. */
export type KeyValue = { getItem(key: string): string | null; setItem(key: string, value: string): void }
export type Permission = 'granted' | 'denied' | 'default' | 'unsupported'
/** Alerts shown by the system while the app is open. */
export type Notifier = {
  /** What the person turns on, such as 浏览器提醒. */
  name: string
  permission(): Promise<Permission>
  request(): Promise<Permission>
  show(alert: { title: string; body: string; tag: string }, open: () => void): void
}
/** `rejoin`: a removed device asked to join again once the launch connects. */
export type LocalLaunch = { token: string; deviceId: string; rejoin: boolean }
/** A browser page served by a Node itself: only it may set up, launch or join that computer. */
export type LocalPage = {
  origin: string
  /** The invitation or local launch in the address the page opened with, read once and cleared from it. */
  takeLaunch(): { invitation: string; localLaunch: LocalLaunch | null }
  /** This computer's setup on localhost, `null` anywhere else. */
  discoverLocalSetup(signal: AbortSignal): Promise<LocalSetup | null>
  /** The cookie-bound local session of this page, exchanged from `launch` when given. */
  localEntry(identity: PublicIdentity, launch: LocalLaunch | null): Promise<ConnectionEntry | null>
  /** Opens the local launch address, reloading when only its hash differs. */
  open(url: string): void
}
/** Instances of the app on one device that share its storage; one leads and settles the outbox. */
export type JoinTabs = <T>(name: string, handlers: { lead(): void | (() => void); message(value: T): void }) => { post(value: T): void; close(): void }

/** What differs between the browser and the phone; everything else in this package is shared. */
export type Platform = {
  secure: SecureApi
  transport: Transport
  storage: KeyValue
  outbox: OutboxStore
  notifier: Notifier
  /** Calls `listener` each time the app comes back to the foreground. */
  onForeground(listener: () => void): () => void
  foreground(): boolean
  /** Hands a sealed recovery file to the person to keep, under `name`. */
  saveRecovery(bytes: Uint8Array, name: string): void | Promise<void>
  /** How messages name this device: 这个浏览器, 这台手机. */
  self: string
  page?: LocalPage
  joinTabs?: JoinTabs
}
