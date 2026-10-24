import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { joinTicket, validTarget } from '../lib/transport.ts'
import type { DeviceChannel, JoinLink } from '../lib/transport.ts'
import { array, boolean, deviceName, entryKey, messageOf, nodeNow, parseEntry, parseInfo, parseJoinRequest, parseView, record, same, text } from '../lib/node.ts'
import type { ConnectionEntry, EntryInfo, JoinRequest, LocalSetup, Member, MembershipView, PendingRecovery, PublicIdentity, ReadyConnection } from '../lib/node.ts'
import { shouldAutoConnect } from './connection-lane.ts'
import { parseChanged } from '../lib/progress.ts'
import type { ChangedEvent } from '../lib/progress.ts'
import { recoveryCode, recoveryFileName, secureKit } from '../lib/secure.ts'
import type { AuthenticationTrust, RecoveryFile } from '../lib/secure.ts'
import { nextLink } from '../lib/link.ts'
import type { Link, LinkEvent } from '../lib/link.ts'
import { candidates, isPublicUrl, markReached, mergeEntrances, openEntrances, verifiedEntrances } from '../lib/entrances.ts'
import type { Entrance, OpenEntrance } from '../lib/entrances.ts'
import type { KeyValue, LocalLaunch, Platform } from '../lib/platform.ts'
import { connectLogSink, reportLog } from '../lib/web-log.ts'

type Phase = 'initializing' | 'blocked' | 'key_lost' | 'disconnected' | 'connecting' | 'reconnecting' | 'unpaired' | 'removed' | 'restarting' | 'ready'
type Review = { kind: 'bootstrap'; networkId: string } | { kind: 'recovery'; networkId: string; members: Member[]; newManager: Member; requiredNode: string }
type Sensitive = { cancel(): void; download?: () => Promise<void>; bootstrap?: (saved: boolean) => Promise<void>; recover?: (members: Member[]) => Promise<void> }
type Session = { channel: DeviceChannel; entry: ConnectionEntry; info: EntryInfo; generation: number; networkId?: string }
type Reconnect = { entry: ConnectionEntry; networkId: string; attempt: number }
type AuthenticatedEntry = Pick<Reconnect, 'entry' | 'networkId'>
/** This browser's own recovery as its Node last told: waiting until `effectiveAt` on this browser's clock, or stopped. */
type RecoveryWait = { status: 'waiting'; effectiveAt: number | null } | { status: 'stopped' }
/** What this browser keeps of a recovery it sent, to ask the Node how it stands and to trust the directory once it took effect. */
type WaitRecord = { entry: ConnectionEntry; networkId: string; genesis: unknown; checkpoint: unknown }
/**
 * This device's own join request; `nonce` stays here until the manager has committed its nonce. `pinned` holds the
 * network, manager and manager nonce seen before the reveal; the short code uses only these.
 */
type Joining = { entry: ConnectionEntry; join_id: string; invite_id: string; nonce: string; name: string; status: JoinRequest['status']; pinned?: JoinPin; code?: string; reason?: string | null }
type JoinPin = { network_id: string; admin_id: string; admin_nonce: string }
/** This computer's own join, as the Node reports it while the manager approves. */
type NodeJoining = { status: 'waiting' | 'confirming' | 'joined'; code?: string }
/** What a manager fixed when it sent its nonce: the request it saw, so a later answer of the entry cannot change it. */
type ApprovalPin = { nonce: string; device_id: string; invite_id: string; commit: string }
/** A fresh invitation; `publicEntry`: a phone's camera can open `link`, otherwise the Vibeface app scans `ticket`. */
export type Invitation = { link: string; ticket: string; publicEntry: boolean; expiresAt: number }
/** What tabs of this origin tell each other: the leader's new entrance, or that the outbox changed. */
type TabMessage = { type: 'entrance'; networkId: string; entry: ConnectionEntry } | { type: 'outbox'; networkId: string }
export type Tabs = { leader(): boolean; post(message: TabMessage): void; listen(listener: (message: TabMessage) => void): () => void }
type DeviceContextValue = {
  connectionGeneration(): number
  link: Link; entrances: OpenEntrance[]; tabs: Tabs
  identity: PublicIdentity | null; ready: ReadyConnection | null; entries: ConnectionEntry[]; phase: Phase; error: string
  localAutoAccess: boolean | null; setLocalAutoAccess(enabled: boolean): Promise<void>
  entry: ConnectionEntry | null; info: EntryInfo | null; directory: MembershipView | null
  /** When the Node first saw the current recovery key, if it knows. */
  recoverySince: number | null
  /** A recovery waiting to replace this directory, unless this manager said it was them. */
  pendingRecovery: PendingRecovery | null; dismissPendingRecovery(): void
  /** Stops the recovery `checkpoint` names; resolves once it is stopped, also when an earlier try already stopped it. */
  vetoRecovery(checkpoint: string): Promise<void>
  /** The Node's clock minus this browser's, as of the last directory: times in the directory are on the Node's clock. */
  nodeClockOffset: number
  /** This browser's own recovery while it waits, or once a device stopped it. */
  recovery: RecoveryWait | null; dismissRecovery(): void
  /** This browser's recovery took effect; its new file still waits to be confirmed as saved. */
  recovered: boolean; finishRecovered(): void
  /** The name this browser last saw a device listed under, kept past its removal. */
  knownName(deviceId: string): string | null
  joins: JoinRequest[]; joining: Joining | null; review: Review | null; initialInvitation: string; localSetup: LocalSetup | null
  /** Set on the local page of a removed Node, which alone may start it over. */
  removedLocally: boolean; rejoined: boolean; nodeJoining: NodeJoining | null
  rejoin(): Promise<void>; joinThisComputer(link: string, name: string): Promise<'joined' | 'cancelled'>; cancelNodeJoin(): Promise<void>
  connect(entry: ConnectionEntry): Promise<void>; entryName(entry: ConnectionEntry): string; disconnect(): void; retryNow(): void; saveEntry(entry: ConnectionEntry): void; removeEntry(entry: ConnectionEntry): void
  replaceLostKey(): Promise<void>; join(link: string, name: string): Promise<void>; checkJoin(): Promise<void>; cancelJoin(): void
  createLocalNetwork(name: string, password: string, nodeName?: string): Promise<void>; openLocalAccess(): Promise<void>
  refreshDirectory(): Promise<void>; createInvitation(): Promise<Invitation>
  startApproval(joinId: string): Promise<void>; approveJoin(joinId: string, manage: boolean): Promise<void>; rejectJoin(joinId: string): Promise<void>; revoke(deviceId: string): Promise<void>
  beginRecovery(input: { file: RecoveryFile; password: string; name: string; entry: ConnectionEntry }): Promise<void>
  replaceRecoveryFile(input: { file: RecoveryFile; oldPassword: string; password: string }): Promise<void>
  downloadBackup(): Promise<void>; confirmBootstrap(saved: boolean): Promise<void>; confirmRecovery(members: Member[]): Promise<void>; cancelSensitive(): void
}
const DeviceContext = createContext<DeviceContextValue | null>(null)
class AuthenticationError extends Error {}
const sinceOf = (response: Record<string, unknown>) => typeof response.recovery_since_ms === 'number' ? response.recovery_since_ms : null
const INVITE_TTL_MS = 10 * 60 * 1000
/** Not an error: the entry buttons below it are the way in. */
export const notJoined = (self: string) => `${self}还没加入`
export const removedText = (self: string) => `${self}已被移除，请在管理设备上重新添加`
const changed = () => new Error('设备连接已切换。原操作结果需要确认，不会自动重发。')
const superseded = () => new Error('这个恢复文件已作废。请用最近一次下载的恢复文件。')
const hex = (bytes: Uint8Array) => [...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('')
const randomHex = () => hex(crypto.getRandomValues(new Uint8Array(32)))
/** Saved connections, pins and pending joins in this device's storage. */
function deviceStore(storage: KeyValue, self: string) {
  function load(key: string): unknown { try { const value = storage.getItem(`vibeface.${key}`); return value ? JSON.parse(value) : null } catch { return null } }
  function store(key: string, value: unknown) { try { storage.setItem(`vibeface.${key}`, JSON.stringify(value)) } catch { throw new Error(`${self}无法保存设备记录。请允许保存本地数据。`) } }
  function savedEntries(): ConnectionEntry[] {
    const value = load('connections')
    if (!Array.isArray(value)) return []
    const entries = new Map<string, ConnectionEntry>()
    for (const item of value) { try { const entry = parseEntry(item); entries.set(entryKey(entry), entry) } catch {} }
    return [...entries.values()]
  }
  function loadAuthenticated(): AuthenticatedEntry | null {
    try {
      const value = load('last-authenticated')
      if (!value) return null
      const v = record(value)
      return { entry: parseEntry(v.entry), networkId: validTarget(v.networkId) }
    } catch { return null }
  }
  /** The last authenticated target first, then saved ones from most to least recently authenticated. */
  function resumeTargets(): Reconnect[] {
    if (load('stay-disconnected') === true) return []
    const last = loadAuthenticated()
    const targets: Reconnect[] = last && !last.entry.local ? [{ ...last, attempt: 0 }] : []
    for (const entry of savedEntries().reverse()) {
      const networkId = load(`network-for.${entry.device_id}`)
      if (entry.local || typeof networkId !== 'string' || targets.some(item => entryKey(item.entry) === entryKey(entry))) continue
      try { targets.push({ entry, networkId: validTarget(networkId), attempt: 0 }) } catch {}
    }
    return targets
  }
  function loadJoining(): Joining | null {
    try {
      const v = record(load('pending-join'))
      const pin = v.pinned ? record(v.pinned) : null
      const pinned = pin ? { network_id: validTarget(pin.network_id), admin_id: validTarget(pin.admin_id), admin_nonce: validTarget(pin.admin_nonce) } : undefined
      return { entry: parseEntry(v.entry), join_id: String(v.join_id), invite_id: String(v.invite_id), nonce: validTarget(v.nonce), name: deviceName(String(v.name)), status: 'pending', pinned }
    } catch { return null }
  }
  function approvalPins(): Record<string, ApprovalPin> { const value = load('approval-pins'); return value && typeof value === 'object' ? value as Record<string, ApprovalPin> : {} }
  const loadEntrances = (networkId: string): Entrance[] => { const value = load(`entrances.${networkId}`); return Array.isArray(value) ? value as Entrance[] : [] }
  function loadWait(): WaitRecord | null {
    try { const v = record(load('recovery-wait')); return { entry: parseEntry(v.entry), networkId: validTarget(v.networkId), genesis: record(v.genesis), checkpoint: record(v.checkpoint) } } catch { return null }
  }
  return { load, store, savedEntries, loadAuthenticated, resumeTargets, loadJoining, approvalPins, loadEntrances, loadWait }
}
function publicIdentity({ deviceId }: { deviceId: string }): PublicIdentity { return { deviceId } }
function isResponseError(error: unknown) { return error instanceof Error && 'responded' in error && error.responded === true }
type Vector = Record<string, number>
/** Whether `seen` holds an event the entry's `vector` does not, so the page shows 同步中. */
function behind(seen: Vector, vector: unknown): boolean {
  if (!vector || typeof vector !== 'object') return false
  const have = vector as Vector
  return Object.entries(seen).some(([author, seq]) => !(typeof have[author] === 'number' && have[author] >= seq))
}
function absorb(seen: Vector, vector: unknown) {
  if (!vector || typeof vector !== 'object') return
  for (const [author, seq] of Object.entries(vector as Vector)) if (typeof seq === 'number' && !(seen[author] >= seq)) seen[author] = seq
}

export function DeviceProvider({ platform, children }: { platform: Platform; children: ReactNode }) {
  const { self, page } = platform
  const { load, store, savedEntries, loadAuthenticated, resumeTargets, loadJoining, approvalPins, loadEntrances, loadWait } = deviceStore(platform.storage, self)
  const { Identity, createGenesis, governancePayload, inviteSigningPayload, joinCommit, openPackage, sealPackage, shortCode, ticketEncode, verifiedView, verifyAddress, verifyAuthentication, verifyRemoval, verifyState } = secureKit(platform.secure)
  const { connectDevice, initClient, parseJoinLink, replaceClient, signClient } = platform.transport
  const discoverLocalSetup = async (signal: AbortSignal) => page ? page.discoverLocalSetup(signal) : null
  const localEntry = async (me: PublicIdentity, launch: LocalLaunch | null) => page ? page.localEntry(me, launch) : null
  /**
   * The manager's short code (design 3.4): only from its own network, its own nonce and the request it saw before
   * sending that nonce, and only once the revealed nonce opens the commitment it saw. A lying entry gets no code.
   */
  function managerCode(request: JoinRequest, networkId: string, me: string): JoinRequest {
    const pin = approvalPins()[request.join_id]
    if (!pin || !request.joiner_nonce || request.admin_id !== me || request.admin_nonce !== pin.nonce || request.device_id !== pin.device_id || request.invite_id !== pin.invite_id || request.commit !== pin.commit) return request
    if (joinCommit(pin.device_id, request.joiner_nonce) !== pin.commit) return request
    return { ...request, code: shortCode(networkId, pin.invite_id, pin.device_id, me, pin.nonce, request.joiner_nonce) }
  }
  const [identity, setIdentity] = useState<PublicIdentity | null>(null)
  const [ready, setReady] = useState<ReadyConnection | null>(null)
  const [entries, setEntries] = useState<ConnectionEntry[]>([])
  const [phase, setPhase] = useState<Phase>('initializing')
  const [error, setError] = useState('')
  const [entry, setEntry] = useState<ConnectionEntry | null>(null)
  const [info, setInfo] = useState<EntryInfo | null>(null)
  const [directory, setDirectory] = useState<MembershipView | null>(null)
  const [recoverySince, setRecoverySince] = useState<number | null>(null)
  const [nodeClockOffset, setNodeClockOffset] = useState(0)
  const [recovery, setRecovery] = useState<RecoveryWait | null>(null)
  // Kept so a reload before the new file is confirmed still lands on it; the wait record goes, so a reload resumes as usual.
  const [recovered, setRecovered] = useState(load('recovered') === true)
  const [, setDismissals] = useState(0)
  const [joins, setJoins] = useState<JoinRequest[]>([])
  const [joining, setJoining] = useState<Joining | null>(null)
  const [review, setReview] = useState<Review | null>(null)
  const [initialInvitation, setInitialInvitation] = useState('')
  const [localSetup, setLocalSetup] = useState<LocalSetup | null>(null)
  const [removedFrom, setRemovedFrom] = useState<ConnectionEntry | null>(null)
  const [rejoined, setRejoined] = useState(false)
  const [nodeJoining, setNodeJoining] = useState<NodeJoining | null>(null)
  const [localAutoAccess, setLocalAutoAccessState] = useState<boolean | null>(null)
  const generation = useRef(0)
  const session = useRef<Session | null>(null)
  const connecting = useRef<AbortController | null>(null)
  const sensitive = useRef<Sensitive | null>(null)
  const listeners = useRef(new Set<(event: ChangedEvent) => void>())
  const reconnect = useRef<Reconnect | null>(null)
  /** Older saved targets to try at page load when the newer one is gone, most recent first. */
  const resumeQueue = useRef<Reconnect[]>([])
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const identityRef = useRef<PublicIdentity | null>(null)
  const joiningRef = useRef<Joining | null>(null)
  const [link, setLinkState] = useState<Link>({ kind: 'booting' })
  const linkRef = useRef<Link>({ kind: 'booting' })
  const [entrances, setEntrances] = useState<OpenEntrance[]>([])
  const seen = useRef<Vector>({})
  const switching = useRef(false)
  const leaderRef = useRef(true)
  const tabChannel = useRef<{ post(message: TabMessage): void; close(): void } | null>(null)
  const tabListeners = useRef(new Set<(message: TabMessage) => void>())

  function step(...events: LinkEvent[]) {
    const next = events.reduce(nextLink, linkRef.current)
    linkRef.current = next; setLinkState(next)
  }
  const tabs: Tabs = {
    leader: () => leaderRef.current,
    post(message) { tabChannel.current?.post(message) },
    listen(listener) { tabListeners.current.add(listener); return () => { tabListeners.current.delete(listener) } },
  }
  const releaseSensitive = useCallback(() => { sensitive.current?.cancel(); sensitive.current = null; setReview(null) }, [])
  const setJoin = useCallback((next: Joining | null) => {
    joiningRef.current = next; setJoining(next)
    store('pending-join', next && { entry: next.entry, join_id: next.join_id, invite_id: next.invite_id, nonce: next.nonce, name: next.name, pinned: next.pinned })
  }, [])
  const resetConnection = useCallback(() => {
    reconnect.current = null
    resumeQueue.current = []
    if (reconnectTimer.current !== null) clearTimeout(reconnectTimer.current)
    reconnectTimer.current = null
    generation.current += 1
    releaseSensitive()
    connecting.current?.abort(); connecting.current = null
    const previous = session.current; session.current = null
    previous?.channel.close()
    listeners.current.clear(); setReady(null); setDirectory(null); setJoins([]); setInfo(null); setEntry(null); setLocalAutoAccessState(null)
    joiningRef.current = null; setJoining(null); setPhase('disconnected'); setError('')
    step({ type: 'left' })
  }, [releaseSensitive])
  const disconnect = useCallback(() => {
    resetConnection()
    try { store('last-authenticated', null); store('pending-join', null); store('stay-disconnected', true) } catch (cause) { setError(messageOf(cause)) }
  }, [resetConnection])
  const cancelSensitive = disconnect
  const saveEntry = useCallback((candidate: ConnectionEntry) => {
    const parsed = parseEntry(candidate)
    const next = [...savedEntries().filter(item => entryKey(item) !== entryKey(parsed)), parsed]
    store('connections', next); setEntries(next)
  }, [])
  const removeEntry = useCallback((candidate: ConnectionEntry) => {
    const next = savedEntries().filter(item => entryKey(item) !== entryKey(candidate))
    store('connections', next)
    const previous = loadAuthenticated()
    if (previous && entryKey(previous.entry) === entryKey(candidate)) store('last-authenticated', null)
    setEntries(next)
  }, [])
  function current() { const value = session.current; if (!value) throw new Error('先连接自己的设备。'); return value }
  function own() { const value = identityRef.current; if (!value) throw new Error(`${self}还没准备好，请${page ? '刷新页面' : '重新打开'}。`); return value }
  async function callSession(active: Session, method: string, params: Record<string, unknown> = {}) {
    if (session.current !== active || active.generation !== generation.current) throw changed()
    let result: unknown
    try { result = await active.channel.request(method, params) } catch (cause) {
      if (method !== 'log') reportLog({ level: 'warn', event: 'web.rpc_failed', message: messageOf(cause), fields: { method: method === 'rpc' ? `rpc.${String(params.method)}` : method, origin: page?.origin ?? self } })
      throw cause
    }
    if (session.current !== active || active.generation !== generation.current) throw changed()
    return result
  }
  function scoped(active = current()) { return (method: string, params: Record<string, unknown> = {}) => callSession(active, method, params) }
  function managing() {
    if (!ready?.isCurrent() || !ready.principal.can_manage || !directory) throw new Error('只有管理员能做这个操作。')
    return ready
  }
  async function authenticate(authValue: unknown, active = current(), expected: AuthenticationTrust | null = null) {
    const request = scoped(active)
    let verified: ReturnType<typeof verifyAuthentication>
    try {
      const trust: AuthenticationTrust | null = active.entry.local === true
        ? { kind: 'local', networkId: expected?.networkId ?? validTarget(record(record(authValue).principal).network_id), nodeId: active.entry.device_id }
        : expected
      verified = verifyAuthentication(authValue, own(), trust)
      const pinnedNetwork = load(`network-for.${active.entry.device_id}`)
      if (pinnedNetwork && pinnedNetwork !== verified.principal.network_id) throw new Error('这台设备属于另一组设备。请先确认连接的是自己的设备。')
    } catch (cause) { throw new AuthenticationError(messageOf(cause)) }
    if (session.current !== active) throw changed()
    const { principal, view } = verified
    const subscribed = record(await request('subscribe', { vector: { ...seen.current } }))
    if (session.current !== active || active.generation !== generation.current) throw changed()
    const lagging = behind(seen.current, subscribed.vector)
    absorb(seen.current, subscribed.vector)
    if (principal.source === 'membership' && active.entry.local) active.entry = { ...active.entry, local: false }
    setLocalAutoAccessState(typeof verified.auth.local_auto_access === 'boolean' ? verified.auth.local_auto_access : null)
    store(`network-for.${active.entry.device_id}`, principal.network_id)
    store(`name-for.${active.entry.device_id}`, active.info.name)
    resumeQueue.current = []
    saveEntry(active.entry)
    store('last-authenticated', { entry: active.entry, networkId: principal.network_id } satisfies AuthenticatedEntry)
    store('pending-join', null); store('stay-disconnected', null)
    active.networkId = principal.network_id
    reconnect.current = { entry: active.entry, networkId: principal.network_id, attempt: 0 }
    const connection: ReadyConnection = {
      generation: active.generation, entry: active.entry, principal, info: active.info,
      request: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request(method, params) },
      rpc: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request('rpc', { method, ...params }) },
      subscribe(listener) { if (session.current !== active || active.generation !== generation.current) return () => {}; listeners.current.add(listener); return () => { listeners.current.delete(listener) } },
      isCurrent: () => session.current === active && generation.current === active.generation,
      catchingUp: () => linkRef.current.kind === 'online' && linkRef.current.catchingUp,
    }
    noteNames(principal.network_id, view.members)
    setDirectory(view); setRecoverySince(sinceOf(verified.auth)); setNodeClockOffset((nodeNow(verified.auth) ?? Date.now()) - Date.now()); setReady(connection); setPhase('ready'); setError(''); joiningRef.current = null; setJoining(null)
    setLocalSetup(local => local && entryKey(local.entry) === entryKey(active.entry) ? { kind: 'initialized', entry: local.entry, removed: false } : local)
    step({ type: 'ready', joined: true }, { type: 'connect' }, { type: 'connected', behind: lagging })
    if (principal.source === 'membership') void refreshEntrances(active, view)
    return view
  }
  /** Signed entrance records, checked against the verified directory, merged into this origin's list. */
  async function refreshEntrances(active: Session, view: MembershipView) {
    try {
      const fresh = verifiedEntrances(record(await callSession(active, 'entrances')).entrances, view, verifyAddress)
      const merged = markReached(mergeEntrances(loadEntrances(view.network_id), fresh, view), active.entry)
      store(`entrances.${view.network_id}`, merged); setEntrances(openEntrances(merged))
    } catch { /* the saved list stays usable; the next connection refreshes it */ }
  }
  /** While the entry catches up with what this page already showed, ask again until it has. */
  async function checkCaughtUp(active: Session) {
    const subscribed = record(await callSession(active, 'subscribe', { vector: { ...seen.current } }))
    if (!behind(seen.current, subscribed.vector)) { absorb(seen.current, subscribed.vector); step({ type: 'caught-up' }) }
  }
  /** Tries the entry that dropped, then the other entrances by last success; backs off when all fail. */
  function scheduleReconnect(immediate = false) {
    const target = reconnect.current
    if (!target || reconnectTimer.current !== null || switching.current) return
    const delay = immediate ? 0 : linkRef.current.kind === 'offline' ? linkRef.current.retryInMs : 1000
    reconnectTimer.current = setTimeout(async () => {
      reconnectTimer.current = null
      if (reconnect.current !== target) return
      switching.current = true
      step({ type: 'connect' })
      let done: boolean | 'next' = false
      try { done = await reconnectThrough(target) } finally { switching.current = false }
      if (done === 'next') { scheduleReconnect(true); return }
      if (done || reconnect.current !== target) return
      step({ type: 'lost', reason: '所有访问地址都连不上。' })
      setPhase('reconnecting'); scheduleReconnect()
    }, delay)
  }
  /** Skips the wait before the next reconnect attempt. */
  function retryNow() {
    if (reconnectTimer.current === null) return
    clearTimeout(reconnectTimer.current); reconnectTimer.current = null
    scheduleReconnect(true)
  }
  async function reconnectThrough(target: Reconnect): Promise<boolean | 'next'> {
    if (target.entry.local) {
      const local = identityRef.current && await localEntry(identityRef.current, null)
      if (reconnect.current !== target) return true
      if (!local || entryKey(local) !== entryKey(target.entry)) {
        resetConnection(); setPhase('unpaired'); setError('本机自动访问已关闭或会话已失效。请从原本机设置页重新打开本机访问，或使用邀请加入。')
        return true
      }
    }
    const list = target.entry.local ? [target.entry] : candidates(loadEntrances(target.networkId), target.entry)
    for (const candidate of list) {
      let active: Session | null = null
      try {
        active = await open(candidate, true, list.length > 1 ? 5000 : 10000)
        if (candidate === target.entry && active.info.network_id && active.info.network_id !== target.networkId) throw new AuthenticationError('这台设备已属于另一组设备。')
        const auth = record(await callSession(active, 'authenticate', { network_id: target.networkId }))
        if (candidate === target.entry && removed(auth, active, target.networkId)) return true
        if (auth.principal === null && (auth.revoked === true || candidate === target.entry)) throw new AuthenticationError(auth.revoked === true ? removedText(self) : `${self}已不在我的设备里。`)
        if (auth.principal === null) throw new Error(`这个访问地址还没同步到${self}。`)
        await authenticate(auth, active, { kind: 'network', networkId: target.networkId })
        if (candidate !== target.entry) tabs.post({ type: 'entrance', networkId: target.networkId, entry: candidate })
        return true
      } catch (cause) {
        if (reconnect.current !== target) return true
        // Only a definite answer about this browser's identity ends the resume; a Node error, timeout or drop is retried.
        if (candidate === target.entry && cause instanceof AuthenticationError) {
          const next = resumeQueue.current.shift()
          if (next) {
            // A saved target this browser can no longer use, such as an old device at the same address, is dropped.
            if (active && session.current === active) { generation.current += 1; session.current = null; active.channel.close() }
            removeEntry(target.entry)
            reconnect.current = { ...next, attempt: 0 }
            return 'next'
          }
          resetConnection(); setPhase('unpaired')
          let detail = messageOf(cause)
          try { store('last-authenticated', null) } catch (storageError) { detail += ` ${messageOf(storageError)}` }
          setError(detail)
          return true
        }
        // An entrance that cannot serve this browser yet is skipped; the next one gets its turn.
        if (active && session.current === active) { generation.current += 1; session.current = null; active.channel.close() }
      }
    }
    return false
  }
  async function open(entryValue: ConnectionEntry, resuming = false, timeoutMs = 10000) {
    const parsed = parseEntry(entryValue)
    if (!resuming) { resetConnection(); setPhase('connecting'); setEntry(parsed) }
    const version = generation.current
    const controller = new AbortController(); connecting.current = controller
    try {
      const channel = await connectDevice(parsed, {
        signal: controller.signal, timeoutMs,
        onEvent(value) {
          if (version !== generation.current || session.current?.generation !== version || !session.current.networkId) return
          if (!value || typeof value !== 'object' || Array.isArray(value)) return
          const event = record(value)
          if (event.type !== 'changed' || event.network_id !== undefined && event.network_id !== session.current.networkId) return
          const payload = parseChanged(event.payload)
          for (const listener of listeners.current) listener(payload)
        },
        onClose(reason) {
          if (version !== generation.current) return
          generation.current += 1; session.current = null; releaseSensitive(); listeners.current.clear()
          if (reconnect.current) {
            if (!switching.current) step({ type: 'lost', reason: messageOf(reason) })
            setPhase('reconnecting'); setError('连接已断开，正在重连。没确认的操作会保留，恢复后先确认结果再提交。')
            scheduleReconnect()
          } else {
            setReady(null); setDirectory(null); setJoins([]); setPhase('disconnected'); setError(messageOf(reason))
          }
        },
      })
      if (version !== generation.current) { channel.close(); throw changed() }
      const infoValue = await channel.request('info')
      if (version !== generation.current) { channel.close(); throw changed() }
      let details: EntryInfo
      try {
        details = parseInfo(infoValue)
        if (details.device_id !== parsed.device_id || details.device_id !== channel.deviceId) throw new Error('连接的设备和预期的不一致，已断开。')
      } catch (cause) { channel.close(); throw new AuthenticationError(messageOf(cause)) }
      const active = { channel, entry: parsed, info: details, generation: version }
      session.current = active; setInfo(details); if (!resuming) setPhase('unpaired'); return active
    } catch (cause) {
      if (version === generation.current) {
        if (resuming) {
          generation.current += 1
          const previous = session.current; session.current = null; controller.abort(); previous?.channel.close()
          setPhase('reconnecting'); scheduleReconnect()
        } else { resetConnection(); setError(messageOf(cause)) }
      }
      throw cause
    }
  }
  /** Resolves true when the device turned out to be removed. */
  async function connect(candidate: ConnectionEntry) {
    const active = await open(candidate)
    const pinned = load(`network-for.${active.entry.device_id}`)
    const auth = record(await scoped(active)('authenticate', { network_id: pinned ?? '' }))
    if (removed(auth, active, typeof pinned === 'string' ? pinned : '')) return true
    if (auth.principal === null) {
      setError(auth.revoked === true ? removedText(self) : auth.initialized === false ? '这台电脑还没设置。请在它的本机页面完成设置。' : notJoined(self))
      return false
    }
    await authenticate(auth, active)
    return false
  }
  /** A device removed from the network shows only that, once its signed removal checks out. */
  function removed(auth: unknown, active: Session, networkId: string) {
    try { if (!verifyRemoval(auth, active.entry.device_id, networkId)) return false } catch (cause) { throw new AuthenticationError(messageOf(cause)) }
    resetConnection(); setPhase('removed'); setRemovedFrom(active.entry)
    return true
  }
  /** Sets this removed computer's data aside and waits for it to come back fresh, then offers the join. */
  async function rejoin(entry = removedFrom) {
    // Without a local session yet, open one first; the launched page carries on with the rejoin.
    if (!entry?.local && localSetup?.kind === 'initialized' && localSetup.removed) return openLocalAccess(true)
    if (!entry?.local) throw new Error('请在这台电脑上打开它的本机页面，再重新加入。')
    try { await callSession(await open(entry, true), 'rejoin') }
    catch (cause) { resetConnection(); setPhase('removed'); throw cause }
    resetConnection(); setPhase('restarting')
    const version = generation.current
    removeEntry(entry)
    const deadline = Date.now() + 60_000
    let setup: LocalSetup | null = null
    while (setup?.kind !== 'uninitialized' && Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 1000))
      if (version !== generation.current) throw changed()
      try { setup = await discoverLocalSetup(new AbortController().signal) } catch { setup = null }
    }
    if (version !== generation.current) throw changed()
    if (setup?.kind !== 'uninitialized') { setPhase('removed'); throw new Error('这台电脑还没重新开始。请刷新页面看看，或用下面的命令。') }
    setLocalSetup(setup); setRejoined(true); setPhase('disconnected')
  }
  /** Joins this computer, not this browser, with an invite; the Node runs the join and shows the short code. */
  async function joinThisComputer(linkText: string, nameValue: string): Promise<'joined' | 'cancelled'> {
    if (localSetup?.kind !== 'uninitialized') throw new Error('这台电脑已经设置过。')
    const name = deviceName(nameValue)
    const found = joinTicket(linkText)
    const origin = found?.origin ?? page?.origin
    if (!found || !origin) throw new Error('这不是有效的加入链接。')
    const link = `${origin}/join#${found.ticket}`
    const { capability, entry } = localSetup
    const active = await open(entry)
    setNodeJoining({ status: 'waiting' })
    try {
      for (;;) {
        const reply = record(await callSession(active, 'node_join', { capability, link, name }))
        if (reply.status === 'cancelled') return 'cancelled'
        if (reply.status === 'failed') throw new Error('没能加入。链接可能已过期或被拒绝，请在已有设备上重新添加。')
        if (reply.status === 'joined') break
        if (reply.status !== 'waiting' && reply.status !== 'confirming') throw new Error('这台电脑返回了未知的加入状态。')
        setNodeJoining({ status: reply.status, code: typeof reply.code === 'string' ? reply.code : undefined })
        await new Promise(resolve => setTimeout(resolve, 2000))
      }
      setNodeJoining({ status: 'joined' })
      const setup = await discoverLocalSetup(new AbortController().signal)
      if (setup?.kind !== 'initialized' || setup.entry.device_id !== entry.device_id) throw new Error('已加入，但本机页面没变。请刷新页面。')
      launchLocal(setup)
      return 'joined'
    } finally { setNodeJoining(null) }
  }
  /** Stops this computer's join on the Node; the join's own polling then reports how it ended. */
  async function cancelNodeJoin() {
    const active = session.current
    if (!active || localSetup?.kind !== 'uninitialized') return
    await callSession(active, 'node_join_cancel', { capability: localSetup.capability })
  }
  async function refreshDirectory() {
    if (!ready?.isCurrent()) return
    const active = current(), request = scoped(active)
    const response = record(await request('directory'))
    const view = verifiedView(response.state, ready.principal.network_id, nodeNow(response))
    if (!same(response.view, view)) throw new Error('设备列表校验失败，已停止使用。')
    const me = own().deviceId
    const pending = ready.principal.can_manage ? array(record(await request('join_status')).requests).map(parseJoinRequest).map(item => managerCode(item, ready.principal.network_id, me)) : []
    if (session.current !== active || active.generation !== generation.current) throw changed()
    noteNames(ready.principal.network_id, view.members)
    setDirectory(view); setRecoverySince(sinceOf(response)); setNodeClockOffset((nodeNow(response) ?? Date.now()) - Date.now()); setJoins(pending)
  }
  /** A revocation carries no name, so the removed list names a device by what this browser saw while it was a member. */
  function noteNames(networkId: string, members: Member[]) {
    const saved = load(`device-names.${networkId}`), current = members.filter(member => member.name)
    const kept = Object.entries(saved && typeof saved === 'object' ? saved : {}).filter(([id]) => !current.some(member => member.device_id === id))
    try { store(`device-names.${networkId}`, Object.fromEntries([...kept, ...current.map(member => [member.device_id, member.name])].slice(-100))) } catch {}
  }
  function knownName(deviceId: string) {
    const saved = ready && load(`device-names.${ready.principal.network_id}`)
    const name = saved && typeof saved === 'object' ? (saved as Record<string, unknown>)[deviceId] : null
    return typeof name === 'string' ? name : null
  }
  async function setLocalAutoAccess(enabled: boolean) {
    const active = current(), connection = ready
    if (!connection?.isCurrent() || !connection.principal.can_manage && !(connection.principal.source === 'local_session' && !enabled)) throw new Error('此连接不能变更本机自动访问设置。')
    try {
      const response = record(await callSession(active, 'local_access', { enabled }))
      setLocalAutoAccessState(boolean(response.enabled))
    } catch (cause) {
      if (connection.principal.source === 'local_session' && !enabled && !isResponseError(cause)) {
        disconnect(); setError('关闭本机免确认打开的结果还没确认。请重新连接检查。')
      }
      throw cause
    }
    if (connection.principal.source === 'local_session' && !enabled) disconnect()
  }
  async function createInvitation(): Promise<Invitation> {
    const connection = managing(), request = scoped()
    const secret = crypto.getRandomValues(new Uint8Array(32))
    const inviteHash = hex(new Uint8Array(await crypto.subtle.digest('SHA-256', secret)))
    const expiresAt = Date.now() + INVITE_TTL_MS
    const record_ = { network_id: connection.principal.network_id, invite_hash: inviteHash, issuer: own().deviceId, expires_at_ms: expiresAt, signature: '' }
    record_.signature = await signClient('identity-invite', inviteSigningPayload(JSON.stringify(record_)))
    const result = record(await request('invite', { record: record_ }))
    const entryId = validTarget(result.entry_id)
    if (entryId !== connection.entry.device_id) throw new Error('加入链接指向的设备和当前连接的不一致。')
    const route = connection.info.connection_entry ?? connection.entry
    // Any member Gateway forwards the join to this Node, so a public entrance beats the address in use.
    // The entry Node's own record names the public entry it is reached through, if it has one.
    const known = openEntrances(loadEntrances(connection.principal.network_id))
    const reachable = known.filter(item => item.public).sort((a, b) => b.last_ok_ms - a.last_ok_ms)[0] ?? known.find(item => item.device_id === entryId)
    // The Node's own public entry, when it has one, is what a phone's camera can open.
    const configured = typeof result.gateway_url === 'string' && isPublicUrl(result.gateway_url) ? result.gateway_url : undefined
    const origin = configured ?? reachable?.gateway_url ?? (route.device_id === entryId ? route.address : connection.entry.address)
    const entry = result.entry && typeof result.entry === 'object' ? record(result.entry) : {}
    const relay = typeof entry.relay_url === 'string' ? entry.relay_url : undefined
    const direct = array(entry.direct_addrs).filter((value): value is string => typeof value === 'string')
    const ticket = ticketEncode(entryId, hex(secret), relay, direct)
    secret.fill(0)
    return { link: `${origin}/join#${ticket}`, ticket, publicEntry: isPublicUrl(origin), expiresAt }
  }
  async function startApproval(joinId: string) {
    managing()
    const pins = approvalPins()
    const seen = joins.find(item => item.join_id === joinId)
    if (!pins[joinId]) {
      if (seen?.status !== 'pending') throw new Error('这个加入请求已失效。请重新生成加入链接。')
      // Kept across reloads and retries, so a repeated start sends the same nonce.
      pins[joinId] = { nonce: randomHex(), device_id: seen.device_id, invite_id: seen.invite_id, commit: seen.commit }
      store('approval-pins', Object.fromEntries(Object.entries(pins).slice(-16)))
    }
    const request = parseJoinRequest(await scoped()('approve', { join_id: joinId, admin_nonce: pins[joinId].nonce }))
    if (request.status !== 'confirming') throw new Error('这个加入请求已失效。请重新生成加入链接。')
    await refreshDirectory()
  }
  async function approveJoin(joinId: string, manage: boolean) {
    managing()
    const request = joins.find(item => item.join_id === joinId)
    if (!request?.code || request.status !== 'confirming') throw new Error('新设备还没显示安全码。请等它显示后再允许。')
    const response = record(await scoped()('directory'))
    const verified = verifyState(response.state, request.network_id, nodeNow(response))
    let event: Record<string, unknown>
    try { event = JSON.parse(verified.governanceTemplate(own().deviceId, JSON.stringify({ type: 'grant', member: { device_id: request.device_id, kind: request.kind, name: request.name, manage: manage && request.kind === 'client' } }))) }
    finally { verified.free() }
    event.signature = await signClient('identity-governance', governancePayload(JSON.stringify(event)))
    const result = parseJoinRequest(await scoped()('approve', { join_id: joinId, event }))
    if (result.status !== 'joined') throw new Error('加入还没完成。请刷新后再看。')
    await refreshDirectory()
  }
  async function rejectJoin(joinId: string) {
    managing()
    await scoped()('approve', { join_id: joinId, reject: true }); await refreshDirectory()
  }
  async function revoke(deviceId: string) {
    const connection = managing(), request = scoped()
    const member = directory?.members.find(item => item.device_id === deviceId)
    if (!member) throw new Error('找不到这台设备，请刷新后再试。')
    if (deviceId === own().deviceId || deviceId === connection.entry.device_id) throw new Error(`不能移除${self}或当前连接的电脑。`)
    // A Node's log counts up to the head this entry holds, along that branch if it forked.
    let cutoff = 0, cutoffHash: string | null = null
    if (member.kind === 'node') {
      const node = array(record(await connection.rpc('nodes')).nodes).map(item => record(item)).find(item => item.device_id === deviceId)
      if (typeof node?.max_seq === 'number' && node.max_seq > 0) { cutoff = node.max_seq; cutoffHash = text(node.head_hash) }
    }
    const response = record(await request('directory'))
    const verified = verifyState(response.state, connection.principal.network_id, nodeNow(response))
    let event: Record<string, unknown>
    try { event = JSON.parse(verified.governanceTemplate(own().deviceId, JSON.stringify({ type: 'revoke', device_id: deviceId, cutoff_seq: cutoff, cutoff_hash: cutoffHash }))) }
    finally { verified.free() }
    event.signature = await signClient('identity-governance', governancePayload(JSON.stringify(event)))
    await request('revoke', { event }); await refreshDirectory()
    for (const entry of savedEntries().filter(item => item.device_id === deviceId)) removeEntry(entry)
  }
  /**
   * Stops the recovery `checkpoint` names. The directory is read first, and again when the answer is lost, so a veto
   * that landed is not signed again and a recovery that got stopped is told apart from one that took effect.
   */
  async function vetoRecovery(checkpoint: string) {
    const connection = managing(), request = scoped()
    const read = async () => {
      const response = record(await request('directory'))
      const now = nodeNow(response) ?? Date.now()
      const verified = verifyState(response.state, connection.principal.network_id, now)
      try {
        const view = parseView(JSON.parse(verified.viewJson()))
        const event = view.pending?.checkpoint === checkpoint ? JSON.parse(verified.governanceTemplate(own().deviceId, JSON.stringify({ type: 'veto', checkpoint, at_ms: now }))) as Record<string, unknown> : null
        return { event, stopped: view.stopped.includes(checkpoint) }
      } finally { verified.free() }
    }
    const done = async (stopped: boolean) => {
      // In effect, the recovery may have removed this device, so the refresh may fail; the outcome is what counts.
      const refreshed = refreshDirectory()
      if (!stopped) { refreshed.catch(() => {}); throw new Error('这次找回已经生效，没能拦下。') }
      await refreshed
    }
    const { event, stopped } = await read()
    if (!event) return done(stopped)
    event.signature = await signClient('identity-governance', governancePayload(JSON.stringify(event)))
    try { await request('veto', { event }) }
    catch (cause) {
      // Whether it landed is unknown; the directory tells, and only a recovery still waiting keeps the error.
      const after = await read().catch(() => null)
      if (!after || after.event) throw cause
      return done(after.stopped)
    }
    await refreshDirectory()
  }
  /** Joins through a ticket link: commit, wait for the manager's nonce, reveal, show the short code. */
  async function join(linkText: string, nameValue: string) {
    const name = deviceName(nameValue)
    const link: JoinLink = await parseJoinLink(linkText)
    // Joining opens a new connection, so the screen drops this failure as stale; keep it here.
    try {
      // This page's own entry first: a scanned code's address is only a hint, the device key is the check.
      const addresses = [...new Set([...page ? [page.origin] : [], ...link.origin ? [link.origin] : []])]
      let active: Session | null = null
      for (const [index, address] of addresses.entries()) {
        try { active = await open({ address, device_id: link.entryId }) }
        catch (cause) { if (index === addresses.length - 1 || cause instanceof AuthenticationError) { link.secret = ''; throw cause } }
        if (active) break
      }
      if (!active) throw new Error('连不上这台设备。')
      // A browser removed from this network joins again as a new device; its old key stays revoked.
      if (record(await scoped(active)('authenticate')).revoked === true) {
        const fresh = publicIdentity(await replaceClient())
        identityRef.current = fresh; setIdentity(fresh)
        active = await open(active.entry)
      }
      const me = own().deviceId
      const nonce = randomHex()
      let response: ReturnType<typeof parseJoinRequest>
      try { response = parseJoinRequest(await scoped(active)('join', { secret: link.secret, public_key: me, kind: 'client', name, commit: joinCommit(me, nonce) })) }
      finally { link.secret = '' }
      if (response.invite_id !== link.inviteId || response.device_id !== me) throw new Error('加入结果和这条链接对不上，已停止。请重新生成加入链接。')
      if (response.status === 'rejected' || response.status === 'expired') throw new Error('这条加入链接已失效。请在已有设备上重新添加。')
      setJoin({ entry: active.entry, join_id: response.join_id, invite_id: response.invite_id, nonce, name, status: response.status })
      await checkJoin()
    } catch (cause) { link.secret = ''; setError(messageOf(cause)); throw cause }
  }
  async function checkJoin() {
    const pending = joiningRef.current
    if (!pending) return
    const active = current(), request = scoped(active)
    let status = parseJoinRequest(await request('join_status', { join_id: pending.join_id }))
    if (joiningRef.current !== pending) return
    let mine = pending
    if (status.status === 'confirming' && !status.joiner_nonce && status.admin_id && status.admin_nonce) {
      // Fix the manager's nonce before revealing ours, so the entry cannot pick it after seeing our nonce.
      if (!mine.pinned) { mine = { ...mine, pinned: { network_id: status.network_id, admin_id: status.admin_id, admin_nonce: status.admin_nonce } }; setJoin(mine) }
      status = parseJoinRequest(await request('join_status', { join_id: pending.join_id, reveal: pending.nonce }))
    }
    if (joiningRef.current !== mine) return
    if (status.device_id !== own().deviceId || status.invite_id !== pending.invite_id) throw new Error(`加入状态与${self}不一致。`)
    const pin = mine.pinned
    if (pin && status.admin_id && (status.network_id !== pin.network_id || status.admin_id !== pin.admin_id || status.admin_nonce !== pin.admin_nonce)) {
      const failure = new Error('连接的设备改动了加入请求，已停止。请重新生成加入链接。')
      setJoin(null); setError(failure.message)
      throw failure
    }
    if (status.status === 'joined') {
      if (pin?.network_id !== status.network_id) throw new Error('加入结果和显示安全码时的不一样，已停止。请重新生成加入链接。')
      const state = record(await request('join_status', { join_id: pending.join_id })).state
      verifiedView(state, status.network_id).members.find(member => member.device_id === own().deviceId) ?? (() => { throw new Error(`加入结果没有包含${self}。`) })()
      await authenticate(await request('authenticate', { network_id: status.network_id }), active, { kind: 'network', networkId: status.network_id })
      // The link is single-use; a later visit to the join screen starts empty.
      setInitialInvitation('')
      return
    }
    if (status.status === 'rejected' || status.status === 'expired') {
      // The join is gone, so the poller can no longer tell it apart from a stale one; say why here.
      const failure = new Error(status.reason === 'commitment_mismatch' ? '加入请求校验失败，已作废。请重新生成加入链接。' : status.status === 'expired' ? '加入链接已过期。请重新生成。' : '这次加入被拒绝了。要再试，请在已有设备上重新添加。')
      setJoin(null); setError(failure.message)
      throw failure
    }
    const code = pin && status.joiner_nonce ? shortCode(pin.network_id, pending.invite_id, own().deviceId, pin.admin_id, pin.admin_nonce, pending.nonce) : undefined
    const next = { ...mine, status: status.status, code }
    joiningRef.current = next; setJoining(next)
  }
  const cancelJoin = disconnect
  async function createLocalNetwork(nameValue: string, password: string, nodeName?: string) {
    if (localSetup?.kind !== 'uninitialized') throw new Error('这台电脑已经设置过。请用加入链接或恢复文件。')
    const name = deviceName(nameValue)
    const { capability } = localSetup
    const active = await open(localSetup.entry)
    if (active.info.initialized) throw new Error('这台电脑已经设置过。请用加入链接或恢复文件。')
    if (!password) throw new Error('请设置恢复密码。')
    const root = Identity.generate()
    let recovery: Uint8Array, genesis: Record<string, unknown>, networkId: string
    try {
      networkId = root.deviceId()
      genesis = JSON.parse(createGenesis(root, JSON.stringify({ device_id: own().deviceId, kind: 'client', name, manage: true }), JSON.stringify({ device_id: active.info.device_id, kind: 'node', name: nodeName ? deviceName(nodeName) : active.info.name, manage: false })))
      recovery = sealPackage({ networkId, genesis, checkpoints: [], events: [] }, root, password)
    } finally { root.free(); password = '' }
    const code = await recoveryCode(networkId)
    if (session.current !== active) { recovery.fill(0); throw changed() }
    let downloaded = false, cancelled = false
    const flow: Sensitive = {
      cancel() { cancelled = true; recovery.fill(0) },
      async download() { if (cancelled || session.current !== active) throw changed(); await platform.saveRecovery(recovery, recoveryFileName(code)); downloaded = true },
      async bootstrap(saved) {
        if (!saved || !downloaded) throw new Error('请先下载恢复文件，并确认已妥善保存。')
        if (cancelled || session.current !== active) throw changed()
        try {
          const auth = await scoped(active)('recover', { genesis, capability })
          if (cancelled) throw new Error('创建已提交，但这里已取消。请重新连接看一下结果。')
          await authenticate(auth, active, { kind: 'root', networkId, genesis })
        } finally { if (sensitive.current === flow) releaseSensitive(); else flow.cancel() }
      },
    }
    sensitive.current = flow; setReview({ kind: 'bootstrap', networkId })
  }
  async function openLocalAccess(thenRejoin = false) {
    if (localSetup?.kind !== 'initialized') throw new Error('请先打开这台电脑的本机页面。')
    // A member browser reconnects as itself; the launcher would start a separate local session on another origin.
    // A removed browser would only be refused again, so it takes the local session.
    const computer = localSetup.entry
    if (!thenRejoin && error !== removedText(self) && savedEntries().some(item => entryKey(item) === entryKey(computer))) { await connect(computer); return }
    const version = generation.current, controller = new AbortController(); connecting.current = controller
    const setup = await discoverLocalSetup(controller.signal)
    if (version !== generation.current) throw changed()
    if (setup?.kind !== 'initialized' || setup.entry.device_id !== localSetup.entry.device_id) throw new Error('本机页面对应的电脑已变化，请刷新页面。')
    setLocalSetup(setup)
    launchLocal(setup, thenRejoin)
  }
  function launchLocal(setup: Extract<LocalSetup, { kind: 'initialized' }>, thenRejoin = false) {
    if (!setup.local_launch) throw new Error('本机免确认打开已关闭。请用加入链接或恢复文件。')
    const url = new URL(setup.local_launch.url)
    url.hash = new URLSearchParams({ local: setup.local_launch.token, node: setup.local_launch.device_id, ...thenRejoin ? { rejoin: '1' } : {} }).toString()
    if (!page) throw new Error('请在这台电脑上打开它的本机页面。')
    page.open(url.href)
  }
  /**
   * Root recovery keeps every current member by default, adds this device as a manager and hands recovery to a new
   * file. It takes effect two days later unless a manager it would replace stops it; this browser keeps a record of it
   * to ask the Node how it stands. The new file is saved before the Node hears of it; a retry after an unknown result
   * sends the same checkpoint again.
   */
  async function beginRecovery(input: { file: RecoveryFile; password: string; name: string; entry: ConnectionEntry }) {
    const name = deviceName(input.name)
    const active = await open(input.entry)
    const request = scoped(active)
    let password = input.password
    input.password = ''
    const opened = await openPackage(input.file, password)
    let released = false
    const release = () => { if (released) return; released = true; opened.release(); password = '' }
    try {
      if (session.current !== active) throw changed()
      const me = own().deviceId
      const fresh = !active.info.initialized
      const capability = fresh && localSetup?.kind === 'uninitialized' && entryKey(localSetup.entry) === entryKey(active.entry) ? localSetup.capability : ''
      if (fresh && !capability) throw new Error('恢复到新电脑需要在那台电脑的本机页面进行。')
      if (!fresh && active.info.network_id !== opened.networkId) throw new Error('这台电脑属于另一组设备。')
      const { genesis } = opened.material
      let state: unknown = { genesis, checkpoints: opened.material.checkpoints, events: opened.material.events }
      let heads: unknown = {}
      // The Node checks the checkpoint's time against its own clock; an empty Node has not told it.
      let offset = 0
      if (!fresh) {
        const expires = Date.now() + 60_000
        const proof = `{"device_id":"${me}","expires_at_ms":${expires},"network_id":"${opened.networkId}"}`
        const signature = opened.root.sign('identity-recovery-read', new TextEncoder().encode(proof))
        const read = record(await request('recovery_state', { expires_at_ms: expires, signature, checkpoints: opened.material.checkpoints, events: opened.material.events }))
        state = read.state; heads = read.heads; offset = (nodeNow(read) ?? Date.now()) - Date.now()
      }
      if (session.current !== active || active.generation !== generation.current) throw changed()
      const view = verifiedView(state, opened.networkId, Date.now() + offset)
      if (view.recovery_key !== opened.root.deviceId()) throw superseded()
      const chain = array(record(state).checkpoints), events = array(record(state).events)
      const revoked = new Set(view.revoked.map(item => item.device_id))
      const members = view.members.filter(member => !revoked.has(member.device_id) && member.device_id !== me)
      if (!members.some(member => member.device_id === active.entry.device_id)) members.push({ device_id: active.entry.device_id, kind: 'node', name: active.info.name, manage: false })
      const newManager: Member = { device_id: me, kind: 'client', name, manage: true }
      const allowed = new Map(members.map(member => [member.device_id, member]))
      let prepared: { checkpoints: unknown[]; checkpoint: unknown } | null = null
      const flow: Sensitive = {
        cancel: release,
        async recover(selectedInput) {
          if (released || session.current !== active) throw new Error('恢复文件已关闭，请重新选择。')
          if (!prepared) {
            const selected = selectedInput.map(member => {
              const original = allowed.get(member.device_id)
              if (!original || original.kind !== member.kind || original.name !== member.name || member.kind === 'node' && member.manage) throw new Error('设备列表变了，请重新打开恢复文件。')
              return member
            })
            if (!selected.some(member => member.device_id === active.entry.device_id) || new Set(selected.map(member => member.device_id)).size !== selected.length) throw new Error('要保留正在连接的这台电脑。')
            selected.push(newManager)
            const next = Identity.generate()
            let verified: ReturnType<typeof verifyState> | null = null
            try {
              const code = await recoveryCode(next.deviceId())
              if (released || session.current !== active) throw new Error('恢复文件已关闭，请重新选择。')
              verified = verifyState(state, opened.networkId, Date.now() + offset)
              const checkpoint: unknown = JSON.parse(verified.createRecoveryCheckpoint(opened.root, next.deviceId(), JSON.stringify(selected), JSON.stringify(heads)))
              const checkpoints = [...chain, checkpoint]
              const bytes = sealPackage({ networkId: opened.networkId, genesis, checkpoints, events }, next, password)
              try { await platform.saveRecovery(bytes, recoveryFileName(code)) } finally { bytes.fill(0) }
              prepared = { checkpoints, checkpoint }
            } finally { next.free(); verified?.free() }
            password = ''
          }
          const wait: WaitRecord = { entry: active.entry, networkId: opened.networkId, genesis, checkpoint: prepared.checkpoint }
          store('recovery-wait', wait)
          let auth: Record<string, unknown>
          try { auth = record(await request('recover', { genesis, checkpoints: prepared.checkpoints, events, ...(fresh ? { capability } : {}) })) }
          catch (cause) { throw new Error(`${messageOf(cause)} 再点一次“恢复”会重试同一次恢复，不会再下载文件。`) }
          if (released) throw new Error('恢复已提交，但这里已取消。请重新连接看一下结果，并用刚下载的恢复文件。')
          try {
            if (auth.principal !== null) await recoveredAs(auth, active, wait)
            // A waiting browser is a guest, whose channel times out; the wait is checked over fresh ones.
            else { resetConnection(); noteRecovery(auth) }
          } finally { if (sensitive.current === flow) releaseSensitive(); else release() }
        },
      }
      sensitive.current = flow
      setReview({ kind: 'recovery', networkId: opened.networkId, members, newManager, requiredNode: active.entry.device_id })
    } catch (cause) { release(); throw cause }
  }
  /** Where this browser's recovery stands, as a Node that does not count it a member yet answered. */
  function noteRecovery(auth: Record<string, unknown>) {
    const status = auth.recovery ? record(auth.recovery) : null
    if (status?.status === 'waiting' && typeof status.effective_at_ms === 'number') setRecovery({ status: 'waiting', effectiveAt: Date.now() + status.effective_at_ms - (nodeNow(auth) ?? Date.now()) })
    else if (status?.status === 'stopped') setRecovery({ status: 'stopped' })
    else dismissRecovery()
  }
  async function recoveredAs(auth: unknown, active: Session, wait: WaitRecord) {
    const view = await authenticate(auth, active, { kind: 'root', networkId: wait.networkId, genesis: wait.genesis, checkpoint: wait.checkpoint })
    dismissRecovery()
    // Still a member of the directory in effect, it follows the wait on the card every manager sees.
    if (view.recovery_key !== record(wait.checkpoint).recovery_key) return
    try { store('recovered', true) } catch {}
    setRecovered(true)
  }
  /** Asks the Node holding this browser's recovery how it stands; once it took effect, connects as the manager it made. */
  async function checkRecovery(wait: WaitRecord) {
    const channel = await connectDevice(wait.entry, { timeoutMs: 10000 })
    let auth: Record<string, unknown>
    try { auth = record(await channel.request('authenticate', { network_id: wait.networkId })) } finally { channel.close() }
    if (auth.principal === null) return noteRecovery(auth)
    const active = await open(wait.entry)
    await recoveredAs(await callSession(active, 'authenticate', { network_id: wait.networkId }), active, wait)
  }
  /** Forgets this browser's recovery; with no connection open, the Node it went through stays the one to try again on. */
  function dismissRecovery() {
    const wait = loadWait()
    try { store('recovery-wait', null) } catch {}
    setRecovery(null)
    if (wait && !session.current) setEntry(wait.entry)
  }
  /** A new recovery file under a new password. Recovery moves to its key, so every older file stops working. */
  async function replaceRecoveryFile(input: { file: RecoveryFile; oldPassword: string; password: string }) {
    const connection = ready
    if (!connection?.isCurrent()) throw new Error('先连接到自己的设备，再换恢复文件。')
    let password = input.password
    input.password = ''
    const opened = await openPackage(input.file, input.oldPassword)
    input.oldPassword = ''
    let verified: ReturnType<typeof verifyState> | null = null
    try {
      if (opened.networkId !== connection.principal.network_id) throw new Error('恢复文件属于另一组设备。')
      const response = record(await connection.request('directory'))
      const state = record(response.state), now = nodeNow(response)
      verified = verifyState(state, opened.networkId, now)
      if (verifiedView(state, opened.networkId, now).recovery_key !== opened.root.deviceId()) throw superseded()
      const next = Identity.generate()
      let checkpoints: unknown[]
      try {
        const code = await recoveryCode(next.deviceId())
        const checkpoint: unknown = JSON.parse(verified.createKeyRotation(opened.root, next.deviceId()))
        checkpoints = [...array(state.checkpoints), checkpoint]
        const bytes = sealPackage({ networkId: opened.networkId, genesis: state.genesis, checkpoints, events: array(state.events) }, next, password)
        try { await platform.saveRecovery(bytes, recoveryFileName(code)) } finally { bytes.fill(0) }
      } finally { next.free(); password = '' }
      await connection.request('recover', { genesis: state.genesis, checkpoints, events: state.events })
      await refreshDirectory()
    } finally { opened.release(); verified?.free() }
  }
  async function replaceLostKey() { const me = publicIdentity(await initClient({ replaceLostKey: true })); identityRef.current = me; setIdentity(me); setPhase('disconnected'); setError('') }
  useEffect(() => page?.watchInvitation(invitation => { setInitialInvitation(invitation); setError('') }), [])
  useEffect(() => ready ? connectLogSink((records) => ready.request('log', { records })) : undefined, [ready])
  useEffect(() => {
    let mounted = true
    let version = generation.current
    const setupController = new AbortController()
    setEntries(savedEntries())
    const { invitation, localLaunch } = page?.takeLaunch() ?? { invitation: '', localLaunch: null }
    if (invitation) setInitialInvitation(invitation)
    initClient().then(publicIdentity).then(async me => {
      if (!mounted) return
      identityRef.current = me; setIdentity(me)
      if (version !== generation.current) return
      // A browser that was in its network resumes it; the join page must not flash in between.
      setPhase(!invitation && resumeTargets().length ? 'reconnecting' : 'disconnected')
      // A recovery sent from here comes first; one its Node no longer knows is forgotten.
      const wait = invitation ? null : loadWait()
      if (wait) {
        setRecovery({ status: 'waiting', effectiveAt: null })
        await checkRecovery(wait).catch(() => {})
        if (!mounted || loadWait() || session.current) return
      }
      // A launch link opens the localhost page; a browser already joined there keeps that identity instead.
      const launch = invitation || resumeTargets().length ? null : localLaunch
      // Only the local launcher's origin serves /api/local/info; a browser learns it is on one from a launch link.
      // After 断开 only a click (a launch link) opens it again, not the remembered local session.
      const stay = load('stay-disconnected') === true
      if (!invitation && (launch || !stay && load('local-origin') === true)) {
        const local = await localEntry(me, launch)
        if (!mounted || version !== generation.current) return
        // Kept while the Node is down, so the next load on this origin still asks it.
        if (local) {
          store('local-origin', true)
          const opening = connect(local); version = generation.current
          if (await opening && launch?.rejoin) await rejoin(local).catch(cause => { if (mounted) setError(messageOf(cause)) })
          return
        }
      }
      const pending = invitation ? null : loadJoining()
      if (pending) {
        const opening = open(pending.entry); version = generation.current
        const active = await opening
        if (!mounted || session.current !== active) return
        joiningRef.current = pending; setJoining(pending)
        await checkJoin()
      } else if (!invitation) {
        let local: LocalSetup | null = null
        try { local = await discoverLocalSetup(setupController.signal) }
        catch (cause) { if (mounted && version === generation.current) setError(messageOf(cause)) }
        if (!mounted || version !== generation.current) return
        setLocalSetup(local)
        if (launch && local?.kind === 'initialized' && local.local_launch) { launchLocal(local); return }
        if (local?.kind === 'uninitialized') { setPhase('disconnected'); return }
        if (local?.kind === 'initialized' && local.removed) { setPhase('removed'); setRemovedFrom(local.entry); return }
        if (stay) return
        if (local?.kind === 'initialized') {
          const opening = connect(local.entry); version = generation.current
          void opening.catch(cause => { if (mounted && version === generation.current) setError(messageOf(cause)) })
          return
        }
        const [first, ...rest] = resumeTargets()
        if (first) {
          // Resume this browser's network through its entrances, starting with the last one used.
          reconnect.current = first; resumeQueue.current = rest
          step({ type: 'ready', joined: true }); setPhase('reconnecting'); scheduleReconnect(true)
          return
        }
        if (!shouldAutoConnect({ hasInvitation: false, savedCount: savedEntries().length })) return
        const [only] = savedEntries()
        if (only) { const opening = connect(only); version = generation.current; void opening.catch(cause => { if (mounted && version === generation.current) setError(messageOf(cause)) }) }
      }
    }).catch(cause => { if (mounted && version === generation.current) { setPhase(cause instanceof Error && 'code' in cause && cause.code === 'key_lost' ? 'key_lost' : 'blocked'); setError(messageOf(cause)) } })
    return () => { mounted = false; setupController.abort(); generation.current += 1; reconnect.current = null; if (reconnectTimer.current !== null) clearTimeout(reconnectTimer.current); reconnectTimer.current = null; sensitive.current?.cancel(); sensitive.current = null; connecting.current?.abort(); const previous = session.current; session.current = null; previous?.channel.close(); listeners.current.clear() }
  }, [])
  useEffect(() => {
    // One tab per origin settles the outbox; a follower takes the leader's new entrance right away.
    const { joinTabs } = platform
    if (!joinTabs) return
    leaderRef.current = false
    const channel = joinTabs<TabMessage>('vibeface-tabs', {
      lead() { leaderRef.current = true; return () => { leaderRef.current = false } },
      message(message) {
        for (const listener of tabListeners.current) listener(message)
        const target = reconnect.current
        if (message.type !== 'entrance' || !target || session.current || target.networkId !== message.networkId) return
        if (reconnectTimer.current !== null) { clearTimeout(reconnectTimer.current); reconnectTimer.current = null }
        reconnect.current = { ...target, entry: message.entry }
        scheduleReconnect(true)
      },
    })
    tabChannel.current = channel
    return () => { tabChannel.current = null; leaderRef.current = true; channel.close() }
  }, [])
  useEffect(() => {
    if (link.kind !== 'online' || !link.catchingUp || !ready) return
    const active = session.current
    if (!active) return
    const timer = setInterval(() => { void checkCaughtUp(active).catch(() => {}) }, 2000)
    return () => clearInterval(timer)
  }, [link, ready])
  useEffect(() => {
    if (!joining || joining.status === 'joined') return
    const pending = joining, version = generation.current
    const timer = setInterval(() => { checkJoin().catch(cause => { if (joiningRef.current === pending && version === generation.current) setError(messageOf(cause)) }) }, 2000)
    return () => clearInterval(timer)
  }, [joining])
  useEffect(() => {
    if (recovery?.status !== 'waiting') return
    const timer = setInterval(() => { const wait = loadWait(); if (wait && !session.current) void checkRecovery(wait).catch(() => {}) }, 30_000)
    return () => clearInterval(timer)
  }, [recovery?.status])
  useEffect(() => {
    if (!ready?.principal.can_manage) return
    // ponytail: any invalidation refreshes the directory and join list; filter on join events if this gets chatty.
    let timer: ReturnType<typeof setTimeout> | undefined
    const stop = ready.subscribe(event => {
      if (event.type !== 'invalidate') return
      clearTimeout(timer); timer = setTimeout(() => { void refreshDirectory().catch(() => {}) }, 300)
    })
    return () => { clearTimeout(timer); stop() }
  }, [ready])
  const recoveryOk = ready && load(`recovery-ok.${ready.principal.network_id}`)
  const pendingRecovery = ready?.principal.can_manage && directory?.pending && directory.pending.checkpoint !== recoveryOk ? directory.pending : null
  const value: DeviceContextValue = {
    connectionGeneration: () => generation.current, link, entrances, tabs,
    identity, ready, entries, phase, error, entry, info, directory, recoverySince, knownName, joins, joining, review, initialInvitation, localSetup, localAutoAccess, setLocalAutoAccess,
    removedLocally: phase === 'removed' && removedFrom?.local === true, rejoined, nodeJoining, rejoin, joinThisComputer, cancelNodeJoin,
    connect: async (item) => { await connect(item) }, entryName: (item) => { const name = load(`name-for.${item.device_id}`); return typeof name === 'string' ? name : '' }, disconnect, retryNow, saveEntry, removeEntry, replaceLostKey, join, checkJoin, cancelJoin, createLocalNetwork, openLocalAccess, refreshDirectory, createInvitation,
    startApproval, approveJoin, rejectJoin, revoke, beginRecovery, replaceRecoveryFile,
    async downloadBackup() { if (!sensitive.current?.download) throw new Error('当前没有可下载的恢复文件。'); await sensitive.current.download() },
    async confirmBootstrap(saved) { if (!sensitive.current?.bootstrap) throw new Error('这一步已失效，请重新开始创建。'); await sensitive.current.bootstrap(saved) },
    async confirmRecovery(members) { if (!sensitive.current?.recover) throw new Error('这一步已失效，请重新选择恢复文件。'); await sensitive.current.recover(members) },
    cancelSensitive,
    pendingRecovery, vetoRecovery, nodeClockOffset,
    dismissPendingRecovery() { if (!ready || !pendingRecovery) return; try { store(`recovery-ok.${ready.principal.network_id}`, pendingRecovery.checkpoint) } catch {} setDismissals(count => count + 1) },
    recovery, dismissRecovery, recovered,
    finishRecovered() { try { store('recovered', null) } catch {} setRecovered(false) },
  }
  return <DeviceContext.Provider value={value}>{children}</DeviceContext.Provider>
}
export function useDevice() { const value = useContext(DeviceContext); if (!value) throw new Error('useDevice must be used inside DeviceProvider'); return value }
