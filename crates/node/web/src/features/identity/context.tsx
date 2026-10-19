import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { connectDevice, initClient, readInvitation, signClient, validTarget } from '../../../transport.js'
import type { DeviceChannel } from '../../../transport.js'
import {
  array, boolean, deviceName, digest, entryKey, integer, messageOf, parseEntry, parseInfo, parseInvitation, parseLocalJoin, parseLocalSetup,
  parseMember, parsePairReceipt, parseReview, parseSigning, parseView, record, same, signingPayload, text,
} from '@/lib/node'
import type { ConnectionEntry, EntryInfo, LocalJoin, LocalSetup, Member, MembershipView, NodeRole, PairReceipt, PublicIdentity, ReadyConnection, WaitingPair } from '@/lib/node'
import { shouldAutoConnect } from './connection-lane'
import { localBrowserEntry } from '@/lib/local-access'
import { parseChanged } from '@/lib/progress'
import type { ChangedEvent } from '@/lib/progress'
import type { AuthenticationTrust } from '@/lib/secure'
import { Identity, downloadPackage, openPackage, sealPackage, verifiedView, verifyAuthentication, verifyRecoveryResult, verifyState } from '@/lib/secure'

type Phase = 'initializing' | 'blocked' | 'key_lost' | 'disconnected' | 'connecting' | 'reconnecting' | 'unpaired' | 'ready'
type Review = { kind: 'bootstrap'; networkId: string } | { kind: 'recovery'; networkId: string; members: Member[]; newManager: Member; requiredNode: string }
type Sensitive = { cancel(): void; download?: () => void; bootstrap?: (saved: boolean) => Promise<void>; recover?: (members: Member[]) => Promise<void> }
type Session = { channel: DeviceChannel; entry: ConnectionEntry; info: EntryInfo; generation: number; networkId?: string }
type Reconnect = { entry: ConnectionEntry; networkId: string; attempt: number }
type AuthenticatedEntry = Pick<Reconnect, 'entry' | 'networkId'>
type DeviceContextValue = {
  connectionGeneration(): number
  identity: PublicIdentity | null; ready: ReadyConnection | null; entries: ConnectionEntry[]; phase: Phase; error: string
  localAutoAccess: boolean | null; setLocalAutoAccess(enabled: boolean): Promise<void>
  entry: ConnectionEntry | null; info: EntryInfo | null; directory: MembershipView | null; pairs: PairReceipt[]; waiting: WaitingPair | null; review: Review | null; initialInvitation: string; localSetup: LocalSetup | null; localJoin: LocalJoin | null
  connect(entry: ConnectionEntry): Promise<void>; disconnect(): void; saveEntry(entry: ConnectionEntry): void; removeEntry(entry: ConnectionEntry): void
  replaceLostKey(): Promise<void>; pair(invitation: string, name: string, password?: string): Promise<void>; acceptPair(): Promise<void>; checkPair(): Promise<void>; cancelPair(): void
  createLocalNetwork(name: string, password: string): Promise<void>; openLocalAccess(): Promise<void>
  acceptLocalJoin(): Promise<void>; refreshLocalJoin(): Promise<void>; useAnotherNodeInvitation(): void
  refreshDirectory(): Promise<void>; createInvitation(kind: 'client' | 'node', roles: NodeRole[]): Promise<string>; approvePair(requestId: string): Promise<void>; revoke(subjectId: string): Promise<void>
  beginRecovery(input: { file: File; password: string; name: string; entry: ConnectionEntry; bootstrap: string }): Promise<void>
  reencrypt(input: { file: File; oldPassword: string; password: string }): Promise<void>
  downloadBackup(): void; confirmBootstrap(saved: boolean): Promise<void>; confirmRecovery(members: Member[]): Promise<void>; cancelSensitive(): void
}
const DeviceContext = createContext<DeviceContextValue | null>(null)
class AuthenticationError extends Error {}
const changed = () => new Error('设备连接已切换。原操作结果需要核对，不会自动重发。')
function load(key: string): unknown { try { const value = localStorage.getItem(`vibeface.${key}`); return value ? JSON.parse(value) : null } catch { return null } }
function store(key: string, value: unknown) { try { localStorage.setItem(`vibeface.${key}`, JSON.stringify(value)) } catch { throw new Error('浏览器无法保存设备记录。请允许此站点使用本地存储。') } }
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
function loadWaiting(): WaitingPair | null {
  try {
    const value = load('waiting-pair')
    if (!value) return null
    const v = record(value)
    const review = parseReview(v.review)
    const signing = parseSigning(v.signing)
    signingPayload(signing, 'identity-pair-review', review)
    if (review.expires_at_ms <= Date.now()) return null
    return { request_id: text(v.request_id), network_id: text(v.network_id), entry: parseEntry(v.entry), review, signing, accepted: boolean(v.accepted) }
  } catch { return null }
}
function ownerOf(state: unknown) { return text(record(record(state).genesis).owner_public_key) }
function sign(signing: unknown, domain: string, expected: unknown) { return signClient(domain, signingPayload(signing, domain, expected)) }
function isResponseError(error: unknown) { return error instanceof Error && 'responded' in error && error.responded === true }
async function discoverLocalSetup(signal: AbortSignal): Promise<LocalSetup | null> {
  const url = new URL(location.href)
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) return null
  const response = await fetch('/api/local/setup', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}', cache: 'no-store', credentials: 'omit', signal })
  if (response.status === 404) return null
  if (!response.ok) throw new Error('无法读取此节点的本机设置。请核对本机地址并刷新页面。')
  return parseLocalSetup(await response.json(), url.origin)
}
async function localJoinRequest(action: 'request' | 'accept' | 'status', params: Record<string, unknown>, entry: ConnectionEntry, signal: AbortSignal): Promise<LocalJoin> {
  const response = await fetch(`/api/local/join/${action}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(params), cache: 'no-store', credentials: 'omit', signal })
  if (!response.ok) throw new Error('节点加入操作尚未确认。请先核对加入状态，再决定下一步。')
  const joined = parseLocalJoin(await response.json(), entry)
  if ((joined.kind === 'review' || joined.kind === 'pending') && await digest(joined.review) !== joined.review_hash) throw new Error('加入确认的内容与校验值不一致，已停止操作。')
  return joined
}

export function DeviceProvider({ children }: { children: ReactNode }) {
  const [identity, setIdentity] = useState<PublicIdentity | null>(null)
  const [ready, setReady] = useState<ReadyConnection | null>(null)
  const [entries, setEntries] = useState<ConnectionEntry[]>([])
  const [phase, setPhase] = useState<Phase>('initializing')
  const [error, setError] = useState('')
  const [entry, setEntry] = useState<ConnectionEntry | null>(null)
  const [info, setInfo] = useState<EntryInfo | null>(null)
  const [directory, setDirectory] = useState<MembershipView | null>(null)
  const [pairs, setPairs] = useState<PairReceipt[]>([])
  const [waiting, setWaiting] = useState<WaitingPair | null>(null)
  const [review, setReview] = useState<Review | null>(null)
  const [initialInvitation, setInitialInvitation] = useState('')
  const [localSetup, setLocalSetup] = useState<LocalSetup | null>(null)
  const [localJoin, setLocalJoin] = useState<LocalJoin | null>(null)
  const [localAutoAccess, setLocalAutoAccessState] = useState<boolean | null>(null)
  const generation = useRef(0)
  const session = useRef<Session | null>(null)
  const connecting = useRef<AbortController | null>(null)
  const sensitive = useRef<Sensitive | null>(null)
  const listeners = useRef(new Set<(event: ChangedEvent) => void>())
  const reconnect = useRef<Reconnect | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const identityRef = useRef<PublicIdentity | null>(null)
  const waitingRef = useRef<WaitingPair | null>(null)

  const releaseSensitive = useCallback(() => { sensitive.current?.cancel(); sensitive.current = null; setReview(null) }, [])
  const resetConnection = useCallback(() => {
    reconnect.current = null
    if (reconnectTimer.current !== null) clearTimeout(reconnectTimer.current)
    reconnectTimer.current = null
    generation.current += 1
    releaseSensitive()
    connecting.current?.abort(); connecting.current = null
    const previous = session.current; session.current = null
    previous?.channel.close()
    listeners.current.clear(); setReady(null); setDirectory(null); setPairs([]); setInfo(null); setEntry(null); setLocalAutoAccessState(null)
    waitingRef.current = null; setWaiting(null); setPhase('disconnected'); setError('')
  }, [releaseSensitive])
  const disconnect = useCallback(() => {
    resetConnection()
    try { store('last-authenticated', null); store('waiting-pair', null) } catch (cause) { setError(messageOf(cause)) }
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
  function current() { const value = session.current; if (!value) throw new Error('先连接并核对自己的设备。'); return value }
  async function callSession(active: Session, method: string, params: Record<string, unknown> = {}) {
    if (session.current !== active || active.generation !== generation.current) throw changed()
    const result = await active.channel.request(method, params)
    if (session.current !== active || active.generation !== generation.current) throw changed()
    return result
  }
  function scoped(active = current()) { return (method: string, params: Record<string, unknown> = {}) => callSession(active, method, params) }
  async function authenticate(authValue: unknown, active = current(), expected: AuthenticationTrust | null = null) {
    const request = scoped(active)
    const own = identityRef.current
    if (!own) throw new Error('浏览器密钥尚未初始化。')
    let verified: ReturnType<typeof verifyAuthentication>
    try {
      const trust: AuthenticationTrust | null = active.entry.local === true
        ? { kind: 'local', networkId: expected?.networkId ?? validTarget(record(record(authValue).principal).network_id), peerId: active.entry.peer_id, nodePublicKey: active.info.public_key }
        : expected
      verified = verifyAuthentication(authValue, own, trust)
      const pinnedNetwork = load(`network-for.${active.entry.peer_id}`)
      if (pinnedNetwork && pinnedNetwork !== verified.principal.network_id) throw new Error('此设备返回了不同的个人网络。请先核对原授权。')
    } catch (cause) { throw new AuthenticationError(messageOf(cause)) }
    if (session.current !== active) throw changed()
    const { principal, view } = verified
    await request('subscribe')
    if (session.current !== active || active.generation !== generation.current) throw changed()
    if (principal.source === 'membership' && active.entry.local) active.entry = { ...active.entry, local: false }
    setLocalAutoAccessState(typeof verified.auth.local_auto_access === 'boolean' ? verified.auth.local_auto_access : null)
    store(`network-for.${active.entry.peer_id}`, principal.network_id)
    saveEntry(active.entry)
    store('last-authenticated', { entry: active.entry, networkId: principal.network_id } satisfies AuthenticatedEntry)
    store('waiting-pair', null)
    active.networkId = principal.network_id
    reconnect.current = { entry: active.entry, networkId: principal.network_id, attempt: 0 }
    const connection: ReadyConnection = {
      generation: active.generation, entry: active.entry, principal, info: active.info,
      request: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request(method, params) },
      rpc: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request('rpc', { method, ...params }) },
      subscribe(listener) { if (session.current !== active || active.generation !== generation.current) return () => {}; listeners.current.add(listener); return () => { listeners.current.delete(listener) } },
      isCurrent: () => session.current === active && generation.current === active.generation,
    }
    setDirectory(view); setReady(connection); setPhase('ready'); setError(''); waitingRef.current = null; setWaiting(null)
    setLocalSetup(local => {
      if (!local) return null
      const entry = local.kind === 'uninitialized' ? local.invitation.entry : local.entry
      return entryKey(entry) === entryKey(active.entry) ? { kind: 'initialized', entry } : local
    })
  }
  function scheduleReconnect(immediate = false) {
    const target = reconnect.current
    if (!target || reconnectTimer.current !== null) return
    reconnectTimer.current = setTimeout(async () => {
      reconnectTimer.current = null
      if (reconnect.current !== target) return
      let active: Session | null = null
      try {
        if (target.entry.local) {
          const own = identityRef.current
          const local = own && await localBrowserEntry(own, null)
          if (reconnect.current !== target) return
          if (!local || entryKey(local) !== entryKey(target.entry)) {
            resetConnection(); setPhase('unpaired'); setError('本机自动访问已关闭或会话已失效。请从原本机设置页重新打开本机访问，或使用普通邀请配对。')
            return
          }
        }
        active = await open(target.entry, true)
        const auth = await callSession(active, 'authenticate', { network_id: target.networkId })
        await authenticate(auth, active, { kind: 'network', networkId: target.networkId })
      } catch (cause) {
        if (reconnect.current !== target) return
        if (isResponseError(cause) || cause instanceof AuthenticationError) {
          resetConnection(); setPhase('unpaired')
          let detail = messageOf(cause)
          try { store('last-authenticated', null) } catch (storageError) { detail += ` ${messageOf(storageError)}` }
          setError(`无法恢复原网络授权。请核对设备后重新连接，或使用邀请配对。${detail}`)
        } else if (active && session.current === active) active.channel.close()
        else scheduleReconnect()
      }
    }, immediate ? 0 : Math.min(1000 * 2 ** target.attempt++, 10000))
  }
  async function open(entryValue: ConnectionEntry, resuming = false) {
    const parsed = parseEntry(entryValue)
    if (!resuming) { resetConnection(); setPhase('connecting'); setEntry(parsed) }
    const version = generation.current
    const controller = new AbortController(); connecting.current = controller
    try {
      const channel = await connectDevice(parsed, {
        signal: controller.signal,
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
            setPhase('reconnecting'); setError('连接已断开，正在恢复只读订阅。未确认操作保留待核对，不会自动重发。')
            scheduleReconnect()
          } else {
            setReady(null); setDirectory(null); setPairs([]); setWaiting(null); waitingRef.current = null; setPhase('disconnected'); setError(messageOf(reason))
          }
        },
      })
      if (version !== generation.current) { channel.close(); throw changed() }
      const infoValue = await channel.request('info')
      if (version !== generation.current) { channel.close(); throw changed() }
      let details: EntryInfo
      try {
        details = parseInfo(infoValue)
        if (details.peer_id !== parsed.peer_id || details.public_key !== channel.publicKey) throw new Error('入口报告的设备与已核验设备编号不一致。')
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
  async function connect(candidate: ConnectionEntry) {
    const active = await open(candidate)
    const request = scoped(active)
    let auth: unknown
    try { auth = await request('authenticate', { network_id: load(`network-for.${active.entry.peer_id}`) ?? '' }) }
    catch (cause) { if (session.current !== active || active.generation !== generation.current) throw changed(); if (isResponseError(cause)) { setError('此浏览器尚未获得设备授权。请使用邀请配对，或导入恢复包。'); return }; throw cause }
    await authenticate(auth, active)
  }
  async function refreshDirectory() {
    if (!ready?.isCurrent()) return
    const active = current(), request = scoped(active)
    const response = record(await request('directory'))
    const view = verifiedView(response.state, ready.principal.network_id, ownerOf(response.state))
    if (!same(response.view, view)) throw new Error('成员目录与已签名的授权不一致。')
    const pending = ready.principal.can_manage ? array(await request('pair_requests')).map(parsePairReceipt) : []
    if (session.current !== active || active.generation !== generation.current) throw changed()
    setDirectory(view); setPairs(pending)
  }
  async function setLocalAutoAccess(enabled: boolean) {
    const active = current(), connection = ready
    if (!connection?.isCurrent() || !connection.principal.can_manage && !(connection.principal.source === 'local_session' && !enabled)) throw new Error('此连接不能变更本机自动访问设置。')
    try {
      const response = record(await callSession(active, 'local_access', { enabled }))
      setLocalAutoAccessState(boolean(response.enabled))
    } catch (cause) {
      if (connection.principal.source === 'local_session' && !enabled && !isResponseError(cause)) {
        disconnect(); setError('关闭本机自动访问的结果尚未确认。请在本机配置中核对，或重新连接检查。')
      }
      throw cause
    }
    if (connection.principal.source === 'local_session' && !enabled) disconnect()
  }
  async function createInvitation(kind: 'client' | 'node', selectedRoles: NodeRole[]) {
    if (!ready?.isCurrent() || !ready.principal.can_manage || !identityRef.current) throw new Error('当前连接尚未核验，或此设备没有管理权限。')
    const request = scoped()
    const requestedRoles = kind === 'node' ? [...new Set(selectedRoles)].sort() : []
    if (kind === 'node' && (!requestedRoles.includes('relay') || requestedRoles.some(role => ['gateway', 'scheduler'].includes(role)) && !requestedRoles.includes('replica'))) throw new Error('运行节点需要中继权限，接入和调度同时需要同步权限。')
    const prepared = record(await request('prepare_invite', { kind, node_roles: requestedRoles, request_id: crypto.randomUUID() }))
    const invite = record(prepared.invite); const payload = record(invite.payload)
    if (payload.new_kind !== kind || !same(payload.node_roles, requestedRoles) || payload.manage !== false || payload.inviter_subject_id !== identityRef.current.peerId || payload.network_id !== ready.principal.network_id || payload.epoch !== ready.principal.epoch || integer(payload.expires_at_ms) <= Date.now()) throw new Error('邀请模板与所选权限不一致。')
    const capabilityHash = [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text(prepared.capability))))].map(byte => byte.toString(16).padStart(2, '0')).join('')
    if (payload.capability_hash !== capabilityHash) throw new Error('邀请密钥与签名模板不一致。')
    invite.inviter_signature = sign(prepared.signing, 'identity-invite', payload)
    const result = record(await request('commit_invite', { invite, capability: prepared.capability }))
    if (!same(result.invite, invite) || result.capability !== prepared.capability) throw new Error('保存后的邀请与已签名内容不一致。')
    const route = ready.info.connection_entry ?? ready.entry
    return JSON.stringify({ url: route.address, peer_id: ready.entry.peer_id, invite: result })
  }
  async function approvePair(requestId: string) {
    if (!ready?.isCurrent() || !ready.principal.can_manage || !identityRef.current) throw new Error('当前连接尚未核验，或此设备没有管理权限。')
    const request = scoped()
    const pending = pairs.find(item => item.request_id === requestId)
    if (!pending || !pending.new_subject_signature || pending.review.expires_at_ms <= Date.now()) throw new Error('新设备尚未确认，或请求已过期。请刷新后核对。')
    const proposal = record(await request('prepare_pair', { request_id: requestId }))
    const event = record(proposal.event)
    const expected = { version: 1, network_id: ready.principal.network_id, epoch: ready.principal.epoch, parents: pending.review.parents, issuer: identityRef.current.peerId, action: { type: 'grant', member: pending.review.new_member }, signature: '' }
    if (!same(proposal.review, pending.review) || !same(event, expected)) throw new Error('配对设备或权限发生变化。请刷新后核对。')
    const inviter_signature = sign(proposal.review_signing, 'identity-pair-review', pending.review)
    event.signature = sign(proposal.signing, 'identity-governance', expected)
    await request('pair_confirm', { request_id: requestId, inviter_signature, event }); await refreshDirectory()
  }
  async function revoke(subjectId: string) {
    if (!ready?.isCurrent() || !ready.principal.can_manage || !identityRef.current || !directory) throw new Error('当前连接尚未核验，或此设备没有管理权限。')
    const request = scoped()
    const member = directory.members.find(item => item.subject_id === subjectId)
    if (!member || member.manage) throw new Error('管理客户端的变更需要根恢复确认。')
    const prepared = record(await request('prepare_revoke', { subject_id: subjectId }))
    const event = record(prepared.event)
    const expected = { version: 1, network_id: ready.principal.network_id, epoch: ready.principal.epoch, parents: directory.heads, issuer: identityRef.current.peerId, action: { type: 'revoke', subject_id: subjectId }, signature: '' }
    if (!same(event, expected)) throw new Error('撤销模板与当前选择不一致。请刷新目录后重试。')
    event.signature = sign(prepared.signing, 'identity-governance', expected)
    await request('revoke', { event }); await refreshDirectory()
  }
  async function pair(invitationText: string, nameValue: string, password?: string) {
    const invitation = parseInvitation(readInvitation(invitationText))
    const name = deviceName(nameValue)
    if (invitation.kind === 'node') {
      resetConnection()
      const version = generation.current, controller = new AbortController(); connecting.current = controller
      const setup = await discoverLocalSetup(controller.signal)
      if (version !== generation.current) throw changed()
      if (setup?.kind !== 'uninitialized') throw new Error('请在待加入节点的 localhost 页面打开此邀请。已有网络的节点不能加入另一网络。')
      setLocalSetup(setup)
      const result = await localJoinRequest('request', { invitation: JSON.stringify(readInvitation(invitationText)), name }, setup.invitation.entry, controller.signal)
      if (version !== generation.current) throw changed()
      if ((result.kind === 'review' || result.kind === 'pending') && (result.review.network_id !== invitation.payload.network_id || result.review.inviter_subject_id !== invitation.payload.inviter_subject_id || result.review.invite_id !== invitation.payload.invite_id || result.review.epoch !== invitation.payload.epoch || result.review.new_member.name !== name || !same(result.review.new_member.node_roles, invitation.payload.node_roles) || result.review.expires_at_ms > integer(invitation.payload.expires_at_ms))) throw new Error('节点加入确认与原邀请不一致，已停止操作。')
      setLocalJoin(result)
      if (result.kind === 'joined') await refreshJoinedSetup(setup.invitation.entry, version, controller.signal)
      return
    }
    const active = await open(invitation.entry)
    const request = scoped(active)
    const own = identityRef.current
    if (!own) throw new Error('浏览器密钥尚未初始化。')
    if (invitation.kind === 'bootstrap') {
      if (active.info.initialized) throw new Error('此节点已创建个人网络。请使用配对邀请或恢复包。')
      if (!password) throw new Error('请设置加密恢复包口令。')
      const root = Identity.generate()
      let recovery: Uint8Array
      let prepared: Record<string, unknown>
      let networkId: string
      try {
        networkId = root.peerId()
        prepared = record(await request('prepare_genesis', { owner_public_key: root.publicKeyHex(), client_name: name }))
        const state = record(prepared.state); const genesis = record(state.genesis)
        const members = array(genesis.initial_members).map(parseMember)
        const expectedMembers = [
          { subject_id: own.peerId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: true },
          { subject_id: active.entry.peer_id, public_key: active.info.public_key, kind: 'node', name: 'This device', node_roles: [...active.info.roles].sort(), manage: false },
        ] satisfies Member[]
        expectedMembers.sort((a, b) => a.subject_id.localeCompare(b.subject_id))
        if (genesis.version !== 1 || genesis.network_id !== networkId || genesis.owner_public_key !== root.publicKeyHex() || genesis.signature !== '' || state.checkpoint !== null || !same(state.events, []) || !same(members, expectedMembers)) throw new Error('首台设备的授权模板与当前设备不一致。')
        genesis.signature = root.sign('identity-genesis', new TextEncoder().encode(signingPayload(prepared.signing, 'identity-genesis', genesis)))
        const verified = verifyState(state, networkId, root.publicKeyHex()); verified.free()
        const payload = { version: 1, network_id: networkId, root_secret_key: root.secretHex(), genesis, recovery_epoch: 1, decryption_keys: [] }
        try { recovery = sealPackage(payload, password) } finally { payload.root_secret_key = '' }
      } finally { root.free(); password = '' }
      if (session.current !== active) { recovery.fill(0); throw changed() }
      let downloaded = false
      let cancelled = false
      const flow: Sensitive = {
        cancel() { cancelled = true; recovery.fill(0); invitation.capability = '' },
        download() { if (cancelled || session.current !== active) throw changed(); downloadPackage(recovery, networkId); downloaded = true },
        async bootstrap(saved) {
          if (!saved || !downloaded) throw new Error('请先下载加密恢复包，并确认妥善保存。')
          if (cancelled || session.current !== active) throw changed()
          try {
            const auth = await request('bootstrap', { capability: invitation.capability, state: prepared.state })
            if (cancelled) throw new Error('初始化已提交，但本地确认已取消。请重新连接核对网络状态。')
            const state = record(prepared.state)
            await authenticate(auth, active, { kind: 'root', networkId, ownerPublicKey: ownerOf(state), genesis: state.genesis })
          }
          finally { if (sensitive.current === flow) releaseSensitive(); else flow.cancel() }
        },
      }
      sensitive.current = flow; setReview({ kind: 'bootstrap', networkId }); return
    }
    const prepared = record(await request('prepare_pair_request', { invite: invitation.invite, capability: invitation.capability, name }))
    const pairRequest = record(prepared.request)
    const member: Member = { subject_id: own.peerId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: false }
    if (!same(pairRequest.member, member) || pairRequest.invite_id !== invitation.payload.invite_id || pairRequest.capability !== invitation.capability) throw new Error('配对请求与此浏览器身份不一致。')
    const expectedProposal = [text(pairRequest.request_id), text(pairRequest.invite_id), text(pairRequest.capability), member, text(pairRequest.nonce)]
    pairRequest.proof = sign(prepared.signing, 'identity-pair', expectedProposal)
    const receipt = record(await request('pair_request', { request: pairRequest }))
    const pairReview = parseReview(receipt.review)
    const signing = parseSigning(receipt.signing)
    signingPayload(signing, 'identity-pair-review', pairReview)
    if (receipt.request_id !== pairRequest.request_id || pairReview.network_id !== invitation.payload.network_id || pairReview.invite_id !== invitation.payload.invite_id || pairReview.epoch !== invitation.payload.epoch || pairReview.inviter_subject_id !== invitation.payload.inviter_subject_id || pairReview.proposal_hash !== await digest(expectedProposal) || !same(pairReview.new_member, member) || pairReview.expires_at_ms > integer(invitation.payload.expires_at_ms) || pairReview.expires_at_ms <= Date.now()) throw new Error('配对确认与原邀请或已签名请求不一致。')
    if (session.current !== active) throw changed()
    const next = { request_id: text(receipt.request_id), network_id: pairReview.network_id, entry: active.entry, review: pairReview, signing, accepted: false }
    waitingRef.current = next; store('waiting-pair', next); setWaiting(next)
  }
  async function createLocalNetwork(name: string, password: string) {
    if (localSetup?.kind !== 'uninitialized') throw new Error('此节点已加入个人网络。请使用邀请或恢复包授权此浏览器。')
    const { entry, capability } = localSetup.invitation
    await pair(JSON.stringify({ url: entry.address, peer_id: entry.peer_id, capability }), name, password)
  }
  async function openLocalAccess() {
    if (localSetup?.kind !== 'initialized') throw new Error('请先打开此节点的本机设置页。')
    const version = generation.current, controller = new AbortController(); connecting.current = controller
    const setup = await discoverLocalSetup(controller.signal)
    if (version !== generation.current) throw changed()
    if (setup?.kind !== 'initialized' || setup.entry.peer_id !== localSetup.entry.peer_id) throw new Error('本机设置的节点已变化，请刷新页面后核对。')
    setLocalSetup(setup)
    if (!setup.local_launch) throw new Error('本机自动访问已关闭。请使用成员邀请或恢复包授权此浏览器。')
    const url = new URL(setup.local_launch.url)
    url.hash = new URLSearchParams({ local: setup.local_launch.token, node: setup.local_launch.peer_id }).toString()
    location.assign(url.href)
  }
  async function refreshJoinedSetup(entry: ConnectionEntry, version: number, signal: AbortSignal) {
    setLocalSetup({ kind: 'initialized', entry })
    try {
      const setup = await discoverLocalSetup(signal)
      if (version !== generation.current) return
      if (setup?.kind !== 'initialized' || setup.entry.peer_id !== entry.peer_id) throw new Error('本机设置的节点状态已变化。')
      setLocalSetup(setup)
    } catch (cause) {
      if (version === generation.current) setError(`此节点已加入网络，但本机入口尚未刷新。请刷新此页后继续。${messageOf(cause)}`)
    }
  }
  async function refreshLocalJoin() {
    if (!localSetup) throw new Error('请先打开此节点的 localhost 页面。')
    const entry = localSetup.kind === 'uninitialized' ? localSetup.invitation.entry : localSetup.entry
    const version = generation.current, controller = new AbortController(); connecting.current = controller
    const result = await localJoinRequest('status', {}, entry, controller.signal)
    if (version !== generation.current) throw changed()
    setLocalJoin(result); setError('')
    if (result.kind === 'joined') await refreshJoinedSetup(entry, version, controller.signal)
  }
  async function acceptLocalJoin() {
    if (!localSetup || localJoin?.kind !== 'review') throw new Error('请先核对节点加入信息。')
    const entry = localSetup.kind === 'uninitialized' ? localSetup.invitation.entry : localSetup.entry
    const version = generation.current, controller = new AbortController(); connecting.current = controller
    const result = await localJoinRequest('accept', { request_id: localJoin.request_id, review_hash: localJoin.review_hash }, entry, controller.signal)
    if (version !== generation.current) throw changed()
    setLocalJoin(result); setError('')
    if (result.kind === 'joined') await refreshJoinedSetup(entry, version, controller.signal)
  }
  async function acceptPair() {
    const request = scoped()
    const pending = waitingRef.current
    if (!pending || pending.review.new_member.subject_id !== identityRef.current?.peerId || pending.review.expires_at_ms <= Date.now()) throw new Error('配对信息失效。请重新取得邀请。')
    await request('pair_accept', { request_id: pending.request_id, signature: sign(pending.signing, 'identity-pair-review', pending.review) })
    if (waitingRef.current !== pending) return
    const next = { ...pending, accepted: true }; waitingRef.current = next; store('waiting-pair', next); setWaiting(next)
    await checkPair()
  }
  async function checkPair() {
    const active = current(), request = scoped(active)
    const pending = waitingRef.current
    if (!pending?.accepted) return
    const response = record(await request('pair_status', { request_id: pending.request_id }))
    if (waitingRef.current !== pending) return
    if (response.status === 'granted') await authenticate(await request('authenticate', { network_id: pending.network_id }), active, { kind: 'network', networkId: pending.network_id })
    else if (response.status !== 'pending') throw new Error('配对请求未完成。请在管理设备上核对，或创建新邀请。')
  }
  const cancelPair = disconnect
  async function beginRecovery(input: { file: File; password: string; name: string; entry: ConnectionEntry; bootstrap: string }) {
    const name = deviceName(input.name)
    const active = await open(input.entry)
    const request = scoped(active)
    let password = input.password
    input.password = ''
    const opened = await openPackage(input.file, password)
    let capability = ''
    let verified: ReturnType<typeof verifyState> | null = null
    let released = false
    const release = () => { if (released) return; released = true; opened.release(); verified?.free(); verified = null; password = ''; capability = '' }
    try {
      if (session.current !== active) throw changed()
      if (input.bootstrap.trim()) {
        const bootstrap = parseInvitation(readInvitation(input.bootstrap))
        if (bootstrap.kind !== 'bootstrap' || entryKey(bootstrap.entry) !== entryKey(active.entry)) throw new Error('初始化邀请与恢复目标不一致。')
        capability = bootstrap.capability
      }
      if (!capability && !active.info.initialized && localSetup?.kind === 'uninitialized' && entryKey(localSetup.invitation.entry) === entryKey(active.entry)) capability = localSetup.invitation.capability
      if (!active.info.initialized && !capability) throw new Error('新节点恢复需要此节点本机生成的初始化邀请。')
      const own = identityRef.current
      if (!own) throw new Error('浏览器密钥尚未初始化。')
      const challengeResult = record(await request('recovery_challenge', { network_id: opened.networkId, owner_public_key: opened.root.publicKeyHex(), genesis: opened.payload.genesis }))
      const challenge = record(challengeResult.challenge)
      if (challenge.network_id !== opened.networkId || challenge.owner_public_key !== opened.root.publicKeyHex() || challenge.subject_id !== own.peerId || challenge.target_peer_id !== active.entry.peer_id || challenge.challenge_id !== challengeResult.challenge_id || challenge.genesis_hash !== await digest(opened.payload.genesis) || integer(challenge.expires_at_ms) <= Date.now() || integer(challenge.expires_at_ms) > Date.now() + 120000 || !/^[a-f0-9]{64}$/.test(text(challenge.nonce))) throw new Error('恢复挑战与当前设备或恢复包不一致。')
      const proof = opened.root.sign('identity-recovery-access', new TextEncoder().encode(signingPayload(challengeResult.signing, 'identity-recovery-access', challenge)))
      const result = record(await request('recovery_context', { challenge_id: challenge.challenge_id, signature: proof, recovery_epoch: opened.epoch, ...(!active.info.initialized ? { state: { genesis: opened.payload.genesis, checkpoint: null, events: [] }, capability } : {}) }))
      if (session.current !== active || active.generation !== generation.current) throw changed()
      if (!same(record(result.state).genesis, opened.payload.genesis)) throw new Error('恢复目标的创世授权与恢复包不一致。')
      verified = verifyState(result.state, opened.networkId, opened.root.publicKeyHex())
      const view = parseView(JSON.parse(verified.viewJson()))
      if (!same(result.view, view)) throw new Error('恢复目录与已签名的成员记录不一致。')
      const members = view.members.filter(member => !view.revoked.includes(member.subject_id) && member.subject_id !== own.peerId)
      if (!members.some(member => member.subject_id === active.entry.peer_id)) members.push({ subject_id: active.entry.peer_id, public_key: active.info.public_key, kind: 'node', name: `恢复入口 ${active.entry.peer_id.slice(0, 10)}`, node_roles: [...active.info.roles].sort(), manage: false })
      const newManager: Member = { subject_id: own.peerId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: true }
      const allowed = new Map(members.map(member => [member.subject_id, member]))
      const flow: Sensitive = {
        cancel: release,
        async recover(selectedInput) {
          if (released || !verified || session.current !== active) throw new Error('恢复材料已清除。请重新导入。')
          const selected = selectedInput.map(member => {
            const original = allowed.get(member.subject_id)
            if (!original || !same({ ...member, manage: original.manage }, original) || member.kind === 'node' && member.manage) throw new Error('所选成员与已验证目录不一致。')
            return member
          })
          if (!selected.some(member => member.subject_id === active.entry.peer_id) || new Set(selected.map(member => member.subject_id)).size !== selected.length) throw new Error('恢复必须保留当前目标，且不能重复选择成员。')
          selected.push(newManager); selected.sort((a, b) => a.subject_id.localeCompare(b.subject_id))
          try {
            const prepared = await request('prepare_recovery', { challenge_id: challenge.challenge_id, members: selected })
            if (released || !verified) throw changed()
            const checkpoint: unknown = JSON.parse(verified.createRecoveryCheckpoint(opened.root, JSON.stringify(selected), opened.epoch, JSON.stringify(prepared)))
            const authValue = await request('recover', { challenge_id: challenge.challenge_id, checkpoint, ...(!active.info.initialized ? { state: result.state, capability } : {}) })
            if (released) throw new Error('恢复授权已提交，但本地恢复已取消。请重新连接核对授权，并使用原恢复包。')
            const { auth, epoch } = verifyRecoveryResult(authValue, { networkId: opened.networkId, ownerPublicKey: opened.root.publicKeyHex(), genesis: opened.payload.genesis, checkpoint, manager: newManager })
            opened.payload.recovery_epoch = epoch
            const bytes = sealPackage(opened.payload, password)
            try { downloadPackage(bytes, opened.networkId) } finally { bytes.fill(0) }
            await authenticate(auth, active, { kind: 'root', networkId: opened.networkId, ownerPublicKey: opened.root.publicKeyHex(), genesis: opened.payload.genesis, checkpoint })
          } finally { if (sensitive.current === flow) releaseSensitive(); else release() }
        },
      }
      sensitive.current = flow
      setReview({ kind: 'recovery', networkId: opened.networkId, members, newManager, requiredNode: active.entry.peer_id })
    } catch (cause) { release(); throw cause }
  }
  async function reencrypt(input: { file: File; oldPassword: string; password: string }) {
    const version = generation.current
    const opened = await openPackage(input.file, input.oldPassword)
    try {
      if (version !== generation.current) throw changed()
      if (ready && opened.networkId !== ready.principal.network_id) throw new Error('恢复包属于其他个人网络。')
      const bytes = sealPackage(opened.payload, input.password)
      try { downloadPackage(bytes, opened.networkId) } finally { bytes.fill(0) }
    } finally { opened.release(); input.oldPassword = ''; input.password = '' }
  }
  async function replaceLostKey() { const own = await initClient({ replaceLostKey: true }); identityRef.current = own; setIdentity(own); setPhase('disconnected'); setError('') }
  useEffect(() => {
    let mounted = true
    let version = generation.current
    const setupController = new AbortController()
    setEntries(savedEntries())
    const fragment = new URLSearchParams(location.hash.slice(1))
    const invitation = fragment.get('bootstrap') || fragment.get('invite') || ''
    const localToken = fragment.get('local')
    const localLaunch = localToken ? { token: localToken, peerId: fragment.get('node') ?? '' } : null
    if (location.hash) { const clean = new URL(location.href); clean.hash = ''; history.replaceState(null, '', clean) }
    if (invitation) setInitialInvitation(invitation)
    initClient().then(async own => {
      if (!mounted) return
      identityRef.current = own; setIdentity(own)
      if (version !== generation.current) return
      setPhase('disconnected')
      if (!invitation) {
        const local = await localBrowserEntry(own, localLaunch)
        if (!mounted || version !== generation.current) return
        if (local) { const opening = connect(local); version = generation.current; await opening; return }
      }
      const pending = invitation ? null : loadWaiting()
      if (pending && pending.review.new_member.subject_id === own.peerId) {
        const opening = open(pending.entry); version = generation.current
        const active = await opening
        if (!mounted || session.current !== active) return
        waitingRef.current = pending; setWaiting(pending)
      } else if (!invitation) {
        let local: LocalSetup | null = null
        try { local = await discoverLocalSetup(setupController.signal) }
        catch (cause) { if (mounted && version === generation.current) setError(messageOf(cause)) }
        if (!mounted || version !== generation.current) return
        setLocalSetup(local)
        if (local?.kind === 'uninitialized') {
          try {
            const result = await localJoinRequest('status', {}, local.invitation.entry, setupController.signal)
            if (!mounted || version !== generation.current) return
            setLocalJoin(result)
            if (result.kind === 'joined') await refreshJoinedSetup(local.invitation.entry, version, setupController.signal)
          } catch (cause) { if (mounted && version === generation.current) setError(messageOf(cause)) }
          return
        }
        if (local?.kind === 'initialized') {
          const opening = connect(local.entry); version = generation.current
          void opening.catch(cause => { if (mounted && version === generation.current) setError(messageOf(cause)) })
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
    if (!waiting?.accepted) return
    const pending = waiting, version = generation.current
    const timer = setInterval(() => { checkPair().catch(cause => { if (waitingRef.current === pending && version === generation.current) setError(messageOf(cause)) }) }, 3000)
    return () => clearInterval(timer)
  }, [waiting])
  useEffect(() => {
    if (localJoin?.kind !== 'pending') return
    const version = generation.current
    const timer = setInterval(() => { refreshLocalJoin().catch(cause => { if (version === generation.current) setError(messageOf(cause)) }) }, 3000)
    return () => clearInterval(timer)
  }, [localJoin])
  const value: DeviceContextValue = {
    connectionGeneration: () => generation.current,
    identity, ready, entries, phase, error, entry, info, directory, pairs, waiting, review, initialInvitation, localSetup, localJoin, localAutoAccess, setLocalAutoAccess,
    connect, disconnect, saveEntry, removeEntry, replaceLostKey, pair, createLocalNetwork, openLocalAccess, acceptPair, checkPair, cancelPair, refreshDirectory, createInvitation, approvePair, revoke, beginRecovery, reencrypt,
    acceptLocalJoin, refreshLocalJoin, useAnotherNodeInvitation() { resetConnection(); setLocalJoin(null) },
    downloadBackup() { if (!sensitive.current?.download) throw new Error('当前没有待下载的恢复包。'); sensitive.current.download() },
    async confirmBootstrap(saved) { if (!sensitive.current?.bootstrap) throw new Error('初始化确认已失效。'); await sensitive.current.bootstrap(saved) },
    async confirmRecovery(members) { if (!sensitive.current?.recover) throw new Error('恢复确认已失效。'); await sensitive.current.recover(members) },
    cancelSensitive,
  }
  return <DeviceContext.Provider value={value}>{children}</DeviceContext.Provider>
}
export function useDevice() { const value = useContext(DeviceContext); if (!value) throw new Error('DeviceProvider 缺失。'); return value }
