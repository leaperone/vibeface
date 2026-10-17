import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { clientVaultStatus, connectDevice, exportDeviceVault, initClient, lockClient, readInvitation, signClient } from '../../../transport.js'
import type { ClientVaultOptions, DeviceChannel, VaultProfile } from '../../../transport.js'
import {
  array, boolean, deviceName, digest, entryKey, integer, messageOf, parseEntry, parseInfo, parseInvitation,
  parseMember, parsePairReceipt, parseReview, parseSigning, parseView, record, same, signingPayload, text,
} from '@/lib/node'
import type { ConnectionEntry, EntryInfo, Member, MembershipView, NodeRole, PairReceipt, PublicIdentity, ReadyConnection, WaitingPair } from '@/lib/node'
import type { AuthenticationTrust } from '@/lib/secure'
import { prepareRpc } from '@/lib/rpc-action'
import type { AuthorizationTrust } from '@/lib/authorization'
import { mergeAuthorizationTrust, parseAuthorizationTrust, verifyScopedAuthentication } from '@/lib/authorization'
import { Identity, downloadPackage, openPackage, sealPackage, verifiedView, verifyRecoveryResult, verifyState } from '@/lib/secure'

type Phase = 'initializing' | 'blocked' | 'vault_empty' | 'vault_locked' | 'vault_unavailable' | 'key_lost' | 'disconnected' | 'connecting' | 'unpaired' | 'ready'
type Review = { kind: 'bootstrap'; networkId: string } | { kind: 'recovery'; networkId: string; members: Member[]; newManager: Member; requiredNode: string }
type Sensitive = { cancel(): void; download?: () => void; bootstrap?: (saved: boolean) => Promise<void>; recover?: (members: Member[]) => Promise<void> }
type Session = { authorization?: ReturnType<typeof verifyScopedAuthentication>; channel: DeviceChannel; entry: ConnectionEntry; info: EntryInfo; generation: number }
type DeviceContextValue = {
  identity: PublicIdentity | null; vaults: VaultProfile[]; spaces: AuthorizationTrust[]; ready: ReadyConnection | null; entries: ConnectionEntry[]; phase: Phase; error: string
  entry: ConnectionEntry | null; info: EntryInfo | null; directory: MembershipView | null; pairs: PairReceipt[]; waiting: WaitingPair | null; review: Review | null; initialInvitation: string
  connect(entry: ConnectionEntry, spaceId?: string): Promise<void>; selectSpace(spaceId: string): Promise<void>; disconnect(): void; saveEntry(entry: ConnectionEntry): void; removeEntry(entry: ConnectionEntry): void
  initializeVault(options: ClientVaultOptions): Promise<void>; lockVault(): void; backupVault(): Promise<void>; pair(invitation: string, name: string, password?: string): Promise<void>; acceptPair(): Promise<void>; checkPair(): Promise<void>; cancelPair(): void
  refreshDirectory(): Promise<void>; createInvitation(kind: 'client' | 'node', roles: NodeRole[]): Promise<string>; approvePair(requestId: string): Promise<void>; revoke(subjectId: string): Promise<void>
  beginRecovery(input: { file: File; password: string; name: string; entry: ConnectionEntry; bootstrap: string }): Promise<void>
  reencrypt(input: { file: File; oldPassword: string; password: string }): Promise<void>
  downloadBackup(): void; confirmBootstrap(saved: boolean): Promise<void>; confirmRecovery(members: Member[]): Promise<void>; cancelSensitive(): void
}
const DeviceContext = createContext<DeviceContextValue | null>(null)
const changed = () => new Error('设备连接已切换。原操作结果需要核对，不会自动重发。')
function load(key: string, required = false): unknown { try { const value = localStorage.getItem(`vibeface.v3.${key}`); return value ? JSON.parse(value) : null } catch { if (required) throw new Error('无法读取已保存的账户信任，请保留浏览器数据并检查存储权限。'); return null } }
function store(key: string, value: unknown) { try { localStorage.setItem(`vibeface.v3.${key}`, JSON.stringify(value)) } catch { throw new Error('浏览器无法保存设备记录。请允许此站点使用本地存储。') } }
function savedEntries(profile: string): ConnectionEntry[] {
  const value = load(`profile.${profile}.connections`)
  if (!Array.isArray(value)) return []
  return value.flatMap(item => { try { return [parseEntry(item)] } catch { return [] } })
}
function loadWaiting(profile: string): WaitingPair | null {
  try {
    const value = load(`profile.${profile}.waiting-pair`)
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

export function DeviceProvider({ children }: { children: ReactNode }) {
  const [identity, setIdentity] = useState<PublicIdentity | null>(null)
  const [vaults, setVaults] = useState<VaultProfile[]>([])
  const [spaces, setSpaces] = useState<AuthorizationTrust[]>([])
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
  const generation = useRef(0)
  const session = useRef<Session | null>(null)
  const connecting = useRef<AbortController | null>(null)
  const sensitive = useRef<Sensitive | null>(null)
  const listeners = useRef(new Set<() => void>())
  const identityRef = useRef<PublicIdentity | null>(null)
  const waitingRef = useRef<WaitingPair | null>(null)

  function storageProfile(own = identityRef.current) {
    if (!own) throw new Error('请先解锁设备配置。')
    return own.profileId ?? `temporary-${own.nodeId}`
  }
  const profileKey = (key: string) => `profile.${storageProfile()}.${key}`
  const savedSpaces = () => array(load(profileKey('policy-trusts'), true) ?? []).map(parseAuthorizationTrust)

  const cancelSensitive = useCallback(() => { sensitive.current?.cancel(); sensitive.current = null; setReview(null) }, [])
  const disconnect = useCallback(() => {
    generation.current += 1
    cancelSensitive()
    connecting.current?.abort(); connecting.current = null
    const previous = session.current; session.current = null
    previous?.authorization?.release(); previous?.channel.close()
    listeners.current.clear(); setReady(null); setDirectory(null); setPairs([]); setInfo(null); setEntry(null)
    waitingRef.current = null; setWaiting(null); setPhase('disconnected'); setError('')
  }, [cancelSensitive])
  const saveEntry = useCallback((candidate: ConnectionEntry) => {
    const parsed = parseEntry(candidate)
    const next = [...savedEntries(storageProfile()).filter(item => entryKey(item) !== entryKey(parsed)), parsed]
    store(profileKey('connections'), next); setEntries(next)
  }, [])
  const removeEntry = useCallback((candidate: ConnectionEntry) => {
    const next = savedEntries(storageProfile()).filter(item => entryKey(item) !== entryKey(candidate))
    store(profileKey('connections'), next); setEntries(next)
  }, [])
  function current() { const value = session.current; if (!value) throw new Error('先连接并核对自己的设备。'); return value }
  async function callSession(active: Session, method: string, params: Record<string, unknown> = {}, invocation?: unknown) {
    if (session.current !== active || active.generation !== generation.current) throw changed()
    const result = await active.channel.request(method, params, invocation)
    if (session.current !== active || active.generation !== generation.current) throw changed()
    return result
  }
  function scoped(active = current()) { return (method: string, params: Record<string, unknown> = {}) => callSession(active, method, params) }
  async function authenticate(authValue: unknown, active = current(), expected: AuthenticationTrust | AuthorizationTrust | null = null) {
    const request = scoped(active)
    const own = identityRef.current
    if (!own) throw new Error('浏览器密钥尚未初始化。')
    if (expected && expected.kind !== 'policy') throw new Error('旧格式账户授权不能用于当前版本。请使用新格式账户连接卡。')
    if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能安全保存账户政策。请更换安全浏览器入口。')
    const authorization = await navigator.locks.request(profileKey('policy-trust-writer'), () => {
      if (session.current !== active) throw changed()
      const known = savedSpaces()
      const selectedSpace = load(profileKey('last-space'))
      const candidate = expected ?? known.find(trust => trust.spaceId === selectedSpace) ?? known[0]
      const trust = mergeAuthorizationTrust(parseAuthorizationTrust(candidate), known)
      const verified = verifyScopedAuthentication(authValue, own, trust, active.channel.publicKey)
      try {
        const others = known.filter(trust => trust.spaceId !== verified.trust.spaceId)
        const next = [...others.map(trust => mergeAuthorizationTrust(trust, [verified.trust])), verified.trust]
        store(profileKey('policy-trusts'), next)
        store(profileKey('last-space'), verified.scope.space_id)
        setSpaces(next)
      } catch (error) { verified.release(); throw error }
      return verified
    })
    if (session.current !== active) { authorization.release(); throw changed() }
    active.authorization?.release(); active.authorization = authorization
    saveEntry(active.entry)
    store(profileKey('last-entry'), active.entry)
    try { await request('subscribe') }
    catch (error) { authorization.release(); if (active.authorization === authorization) active.authorization = undefined; throw error }
    const connection: ReadyConnection = {
      generation: active.generation, entry: active.entry, scope: authorization.scope, audience: authorization.audience, info: active.info,
      authorization: { enrollmentRights: authorization.enrollmentRights, actions: authorization.actions, authorityEpoch: authorization.authorityEpoch },
      request: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request(method, params) },
      rpc: async (method, params) => {
        if (session.current !== active) throw changed()
        const prepared = prepareRpc({ method, params: params ?? {}, spaceId: authorization.scope.space_id, authorizationId: crypto.randomUUID() })
        const bodyJson = JSON.stringify(prepared.body)
        const body = record(JSON.parse(bodyJson))
        const template = record(await request('prepare_invocation', { body }))
        const normalized = record(template.body)
        if (!same(normalized, body)) throw new Error('节点准备的请求与当前操作不同。请核对最终路径、目标和权限后重新提交。')
        const invocation = record(JSON.parse(authorization.sign(JSON.stringify(record(template.invocation)), JSON.stringify(normalized), prepared.action, prepared.authorizationId)))
        return callSession(active, 'rpc', normalized, invocation)
      },
      subscribe(listener) { if (session.current !== active) return () => {}; listeners.current.add(listener); return () => { listeners.current.delete(listener) } },
      isCurrent: () => session.current === active && generation.current === active.generation,
    }
    setDirectory(null); setReady(connection); setPhase('ready'); setError(''); waitingRef.current = null; setWaiting(null); store(profileKey('waiting-pair'), null)
  }
  async function open(entryValue: ConnectionEntry) {
    const parsed = parseEntry(entryValue)
    disconnect(); setPhase('connecting'); setEntry(parsed)
    const version = generation.current
    const controller = new AbortController(); connecting.current = controller
    try {
      const channel = await connectDevice(parsed, {
        signal: controller.signal,
        onEvent(value) { const event = record(value); if (version === generation.current && event.type === 'changed') for (const listener of listeners.current) listener() },
        onClose(reason) {
          if (version !== generation.current) return
          generation.current += 1; session.current?.authorization?.release(); session.current = null; cancelSensitive(); listeners.current.clear(); setReady(null); setDirectory(null); setPairs([]); setWaiting(null); waitingRef.current = null; setPhase('disconnected'); setError(messageOf(reason))
        },
      })
      if (version !== generation.current) { channel.close(); throw changed() }
      const details = parseInfo(await channel.request('info'))
      if (version !== generation.current) { channel.close(); throw changed() }
      if (details.node_id !== parsed.node_id || details.public_key !== channel.publicKey) { channel.close(); throw new Error('入口报告的设备与已核验指纹不一致。') }
      const active = { channel, entry: parsed, info: details, generation: version }
      session.current = active; setInfo(details); setPhase('unpaired'); return active
    } catch (cause) {
      if (version === generation.current) { disconnect(); setError(messageOf(cause)) }
      throw cause
    }
  }
  async function connect(candidate: ConnectionEntry, spaceId?: string) {
    const known = savedSpaces()
    const selectedSpace = spaceId ?? load(profileKey('last-space'), true)
    if (selectedSpace !== null && typeof selectedSpace !== 'string') throw new Error('已保存的空间选择无效。请保留浏览器数据并核查。')
    const trust = selectedSpace ? known.find(trust => trust.spaceId === selectedSpace) : known[0]
    if (!trust) throw new Error('请先使用新格式连接卡为此设备配置取得账户授权。')
    const active = await open(candidate)
    const request = scoped(active)
    let auth: unknown
    try {
      auth = await request('authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: identityRef.current?.publicKey })
    }
    catch (cause) { if (isResponseError(cause)) { setError('此设备尚未取得所选空间授权。请在已授权设备上批准。'); return }; throw cause }
    await authenticate(auth, active, trust)
  }
  async function selectSpace(spaceId: string) {
    if (!ready?.isCurrent()) throw changed()
    if (ready.scope.space_id === spaceId) return
    await connect(ready.entry, spaceId)
  }
  async function refreshDirectory() {
    if (!ready?.isCurrent()) return
    const request = scoped()
    const response = record(await request('directory'))
    const view = verifiedView(response.state, ready.scope.space_id, ownerOf(response.state))
    if (!same(response.view, view)) throw new Error('成员目录与已签名的授权不一致。')
    const pending = ready.authorization.enrollmentRights.includes('enroll_device') ? array(await request('pair_requests')).map(parsePairReceipt) : []
    if (!ready.isCurrent()) throw changed()
    setDirectory(view); setPairs(pending)
  }
  async function createInvitation(kind: 'client' | 'node', selectedRoles: NodeRole[]) {
    if (!ready?.authorization.enrollmentRights.includes('enroll_device') || !identityRef.current) throw new Error('此设备没有管理权限。')
    const request = scoped()
    const requestedRoles = kind === 'node' ? [...new Set(selectedRoles)].sort() : []
    if (kind === 'node' && (!requestedRoles.includes('relay') || requestedRoles.some(role => ['gateway', 'scheduler', 'executor'].includes(role)) && !requestedRoles.includes('replica'))) throw new Error('运行节点需要中继权限，接入、调度和执行同时需要同步权限。')
    const prepared = record(await request('prepare_invite', { kind, node_roles: requestedRoles, request_id: crypto.randomUUID() }))
    const invite = record(prepared.invite); const payload = record(invite.payload)
    if (payload.new_kind !== kind || !same(payload.node_roles, requestedRoles) || payload.manage !== false || payload.inviter_subject_id !== identityRef.current.nodeId || payload.network_id !== ready.scope.space_id || payload.epoch !== ready.authorization.authorityEpoch || integer(payload.expires_at_ms) <= Date.now()) throw new Error('邀请模板与所选权限不一致。')
    const capabilityHash = [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text(prepared.capability))))].map(byte => byte.toString(16).padStart(2, '0')).join('')
    if (payload.capability_hash !== capabilityHash) throw new Error('邀请密钥与签名模板不一致。')
    invite.inviter_signature = sign(prepared.signing, 'identity-invite', payload)
    const result = record(await request('commit_invite', { invite, capability: prepared.capability }))
    if (!same(result.invite, invite) || result.capability !== prepared.capability) throw new Error('保存后的邀请与已签名内容不一致。')
    return JSON.stringify({ url: ready.entry.address, node_id: ready.entry.node_id, relay: ready.entry.relay, invite: result })
  }
  async function approvePair(requestId: string) {
    if (!ready?.authorization.enrollmentRights.includes('enroll_device') || !identityRef.current) throw new Error('此设备没有管理权限。')
    const request = scoped()
    const pending = pairs.find(item => item.request_id === requestId)
    if (!pending || !pending.new_subject_signature || pending.review.expires_at_ms <= Date.now()) throw new Error('新设备尚未确认，或请求已过期。请刷新后核对。')
    const proposal = record(await request('prepare_pair', { request_id: requestId }))
    const event = record(proposal.event)
    const expected = { version: 1, network_id: ready.scope.space_id, epoch: ready.authorization.authorityEpoch, parents: pending.review.parents, issuer: identityRef.current.nodeId, action: { type: 'grant', member: pending.review.new_member }, signature: '' }
    if (!same(proposal.review, pending.review) || !same(event, expected)) throw new Error('配对设备或权限发生变化。请刷新后核对。')
    const inviter_signature = sign(proposal.review_signing, 'identity-pair-review', pending.review)
    event.signature = sign(proposal.signing, 'identity-governance', expected)
    await request('pair_confirm', { request_id: requestId, inviter_signature, event }); await refreshDirectory()
  }
  async function revoke(subjectId: string) {
    if (!ready?.authorization.enrollmentRights.includes('revoke_device') || !identityRef.current || !directory) throw new Error('此设备没有管理权限。')
    const request = scoped()
    const member = directory.members.find(item => item.subject_id === subjectId)
    if (!member || member.manage) throw new Error('管理客户端的变更需要根恢复确认。')
    const prepared = record(await request('prepare_revoke', { subject_id: subjectId }))
    const event = record(prepared.event)
    const expected = { version: 1, network_id: ready.scope.space_id, epoch: ready.authorization.authorityEpoch, parents: directory.heads, issuer: identityRef.current.nodeId, action: { type: 'revoke', subject_id: subjectId }, signature: '' }
    if (!same(event, expected)) throw new Error('撤销模板与当前选择不一致。请刷新目录后重试。')
    event.signature = sign(prepared.signing, 'identity-governance', expected)
    await request('revoke', { event }); await refreshDirectory()
  }
  async function pair(invitationText: string, nameValue: string, password?: string) {
    const invitation = parseInvitation(readInvitation(invitationText))
    setInitialInvitation('')
    const name = deviceName(nameValue)
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
        networkId = root.nodeId()
        prepared = record(await request('prepare_genesis', { owner_public_key: root.publicKeyHex(), client_name: name }))
        const state = record(prepared.state); const genesis = record(state.genesis)
        const members = array(genesis.initial_members).map(parseMember)
        const expectedMembers = [
          { subject_id: own.nodeId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: true },
          { subject_id: active.entry.node_id, public_key: active.info.public_key, kind: 'node', name: 'This device', node_roles: [...active.info.roles].sort(), manage: false },
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
          finally { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }
        },
      }
      sensitive.current = flow; setReview({ kind: 'bootstrap', networkId }); return
    }
    const prepared = record(await request('prepare_pair_request', { invite: invitation.invite, capability: invitation.capability, name }))
    const pairRequest = record(prepared.request)
    const member: Member = { subject_id: own.nodeId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: false }
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
    waitingRef.current = next; store(profileKey('waiting-pair'), next); setWaiting(next)
  }
  async function acceptPair() {
    const request = scoped()
    const pending = waitingRef.current
    if (!pending || pending.review.new_member.subject_id !== identityRef.current?.nodeId || pending.review.expires_at_ms <= Date.now()) throw new Error('配对信息失效。请重新取得邀请。')
    await request('pair_accept', { request_id: pending.request_id, signature: sign(pending.signing, 'identity-pair-review', pending.review) })
    if (waitingRef.current !== pending) return
    const next = { ...pending, accepted: true }; waitingRef.current = next; store(profileKey('waiting-pair'), next); setWaiting(next)
    await checkPair()
  }
  async function checkPair() {
    const request = scoped()
    const pending = waitingRef.current
    if (!pending?.accepted) return
    const response = record(await request('pair_status', { request_id: pending.request_id }))
    if (waitingRef.current !== pending) return
    if (response.status === 'granted') await authenticate(await request('authenticate', { network_id: pending.network_id }), current(), { kind: 'network', networkId: pending.network_id })
    else if (response.status !== 'pending') throw new Error('配对请求未完成。请在管理设备上核对，或创建新邀请。')
  }
  function cancelPair() { waitingRef.current = null; setWaiting(null); store(profileKey('waiting-pair'), null) }
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
      if (!active.info.initialized && !capability) throw new Error('新节点恢复需要此节点本机生成的初始化邀请。')
      const own = identityRef.current
      if (!own) throw new Error('浏览器密钥尚未初始化。')
      const challengeResult = record(await request('recovery_challenge', { network_id: opened.networkId, owner_public_key: opened.root.publicKeyHex(), genesis: opened.payload.genesis }))
      const challenge = record(challengeResult.challenge)
      if (challenge.network_id !== opened.networkId || challenge.owner_public_key !== opened.root.publicKeyHex() || challenge.subject_id !== own.nodeId || challenge.target_node_id !== active.entry.node_id || challenge.challenge_id !== challengeResult.challenge_id || challenge.genesis_hash !== await digest(opened.payload.genesis) || integer(challenge.expires_at_ms) <= Date.now() || integer(challenge.expires_at_ms) > Date.now() + 120000 || !/^[a-f0-9]{64}$/.test(text(challenge.nonce))) throw new Error('恢复挑战与当前设备或恢复包不一致。')
      const proof = opened.root.sign('identity-recovery-access', new TextEncoder().encode(signingPayload(challengeResult.signing, 'identity-recovery-access', challenge)))
      const result = record(await request('recovery_context', { challenge_id: challenge.challenge_id, signature: proof, recovery_epoch: opened.epoch, ...(!active.info.initialized ? { state: { genesis: opened.payload.genesis, checkpoint: null, events: [] }, capability } : {}) }))
      if (!same(record(result.state).genesis, opened.payload.genesis)) throw new Error('恢复目标的创世授权与恢复包不一致。')
      verified = verifyState(result.state, opened.networkId, opened.root.publicKeyHex())
      const view = parseView(JSON.parse(verified.viewJson()))
      if (!same(result.view, view)) throw new Error('恢复目录与已签名的成员记录不一致。')
      const members = view.members.filter(member => !view.revoked.includes(member.subject_id) && member.subject_id !== own.nodeId)
      if (!members.some(member => member.subject_id === active.entry.node_id)) members.push({ subject_id: active.entry.node_id, public_key: active.info.public_key, kind: 'node', name: `恢复入口 ${active.entry.node_id.slice(0, 10)}`, node_roles: [...active.info.roles].sort(), manage: false })
      const newManager: Member = { subject_id: own.nodeId, public_key: own.publicKey, kind: 'client', name, node_roles: [], manage: true }
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
          if (!selected.some(member => member.subject_id === active.entry.node_id) || new Set(selected.map(member => member.subject_id)).size !== selected.length) throw new Error('恢复必须保留当前目标，且不能重复选择成员。')
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
          } finally { if (sensitive.current === flow) cancelSensitive(); else release() }
        },
      }
      sensitive.current = flow
      setReview({ kind: 'recovery', networkId: opened.networkId, members, newManager, requiredNode: active.entry.node_id })
    } catch (cause) { release(); throw cause }
  }
  async function reencrypt(input: { file: File; oldPassword: string; password: string }) {
    const version = generation.current
    const opened = await openPackage(input.file, input.oldPassword)
    try {
      if (version !== generation.current) throw changed()
      if (ready && opened.networkId !== ready.scope.space_id) throw new Error('恢复包属于其他个人网络。')
      const bytes = sealPackage(opened.payload, input.password)
      try { downloadPackage(bytes, opened.networkId) } finally { bytes.fill(0) }
    } finally { opened.release(); input.oldPassword = ''; input.password = '' }
  }
  async function initializeVault(options: ClientVaultOptions) {
    const version = generation.current
    const own = await initClient(options)
    if (version !== generation.current) throw changed()
    identityRef.current = own; setIdentity(own); setPhase('disconnected'); setError('')
    setEntries(savedEntries(storageProfile(own))); setSpaces(savedSpaces())
    const pending = initialInvitation ? null : loadWaiting(storageProfile(own))
    if (pending && pending.review.new_member.subject_id === own.nodeId) {
      await open(pending.entry)
      waitingRef.current = pending; setWaiting(pending)
    } else if (!initialInvitation) {
      const lastEntry = load(profileKey('last-entry'), true)
      if (lastEntry) await connect(parseEntry(lastEntry))
    }
  }
  function lockVault() {
    disconnect(); lockClient(); identityRef.current = null; setIdentity(null); setEntries([]); setSpaces([])
    setPhase('initializing')
    const version = generation.current
    void clientVaultStatus().then(status => {
      if (version === generation.current) { setVaults(status.profiles); setPhase(status.status === 'locked' ? 'vault_locked' : status.status === 'key_lost' ? 'key_lost' : status.status === 'unavailable' ? 'vault_unavailable' : 'vault_empty'); if (status.status === 'unavailable') setError(status.error) }
    }).catch(cause => { if (version === generation.current) { setPhase('blocked'); setError(messageOf(cause)) } })
  }
  async function backupVault() {
    const version = generation.current
    const bytes = new TextEncoder().encode(await exportDeviceVault())
    try {
      if (version !== generation.current) throw changed()
      const url = URL.createObjectURL(new Blob([bytes], { type: 'application/json' }))
      const link = document.createElement('a'); link.href = url; link.download = 'vibeface-device-vault-v3.json'; link.click()
      setTimeout(() => URL.revokeObjectURL(url), 1000)
    } finally { bytes.fill(0) }
  }
  useEffect(() => {
    let mounted = true
    const fragment = new URLSearchParams(location.hash.slice(1))
    const invitation = fragment.get('bootstrap') || fragment.get('invite') || ''
    if (location.hash) { const clean = new URL(location.href); clean.hash = ''; history.replaceState(null, '', clean) }
    if (invitation) setInitialInvitation(invitation)
    clientVaultStatus().then(status => {
      if (mounted) { setVaults(status.profiles); setPhase(status.status === 'locked' ? 'vault_locked' : status.status === 'key_lost' ? 'key_lost' : status.status === 'unavailable' ? 'vault_unavailable' : 'vault_empty'); if (status.status === 'unavailable') setError(status.error) }
    }).catch(cause => { if (mounted) { setPhase('blocked'); setError(messageOf(cause)) } })
    return () => { mounted = false; generation.current += 1; lockClient(); sensitive.current?.cancel(); sensitive.current = null; connecting.current?.abort(); const previous = session.current; session.current = null; previous?.authorization?.release(); previous?.channel.close(); listeners.current.clear() }
  }, [])
  useEffect(() => {
    if (!waiting?.accepted) return
    const version = generation.current
    const timer = setInterval(() => { checkPair().catch(cause => { if (version === generation.current) setError(messageOf(cause)) }) }, 3000)
    return () => clearInterval(timer)
  }, [waiting])
  const value: DeviceContextValue = {
    identity, vaults, spaces, ready, entries, phase, error, entry, info, directory, pairs, waiting, review, initialInvitation,
    connect, selectSpace, disconnect, saveEntry, removeEntry, initializeVault, lockVault, backupVault, pair, acceptPair, checkPair, cancelPair, refreshDirectory, createInvitation, approvePair, revoke, beginRecovery, reencrypt,
    downloadBackup() { if (!sensitive.current?.download) throw new Error('当前没有待下载的恢复包。'); sensitive.current.download() },
    async confirmBootstrap(saved) { if (!sensitive.current?.bootstrap) throw new Error('初始化确认已失效。'); await sensitive.current.bootstrap(saved) },
    async confirmRecovery(members) { if (!sensitive.current?.recover) throw new Error('恢复确认已失效。'); await sensitive.current.recover(members) },
    cancelSensitive,
  }
  return <DeviceContext.Provider value={value}>{children}</DeviceContext.Provider>
}
export function useDevice() { const value = useContext(DeviceContext); if (!value) throw new Error('DeviceProvider 缺失。'); return value }
