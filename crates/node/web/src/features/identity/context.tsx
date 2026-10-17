import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { approveClientEnrollmentIntent, clientVaultStatus, connectDevice, createClientEnrollmentOffer, exportDeviceVault, initClient, lockClient, prepareClientDeviceEnrollment, prepareClientDeviceRevocation, prepareClientFirstAuthenticatorIntent, prepareClientFirstAuthenticatorRegistration, prepareClientPasskeyDeviceEnrollment, prepareClientRecoveryContext, protectClientVaultWithPrf, readInvitation, signClientEnrollmentPossession, signClientPasskeyEnrollmentIntent, signClientPasskeyEnrollmentPossession } from '../../../transport.js'
import type { ClientVaultOptions, DeviceChannel, VaultProfile } from '../../../transport.js'
import {
  array, entryKey, integer, messageOf, parseEntry, parseInfo, parseConnectionCard, record, same, text,
} from '@/lib/node'
import type { AuthorizedRequest, ConnectionCard, ConnectionEntry, EntryInfo, AccountDeviceDirectory, PublicIdentity, ReadyConnection, RpcFacts } from '@/lib/node'
import { prepareRpc } from '@/lib/rpc-action'
import { prepareContext, verifyPreparedBody } from '@/lib/rpc-prepare'
import type { AuthorizationSelection, AuthorizationTrust } from '@/lib/authorization'
import { mergeAuthorizationTrust, parseAuthorizationSelection, parseAuthorizationTrust, subscriptionRenewalDelay, verifyScopedAuthentication } from '@/lib/authorization'
import { downloadPackage } from '@/lib/secure'
import { createAccount, openAccountPackage, recoverAccount, sealAccountPackage } from '@/lib/account'
import { finishAccountDeviceChange, finishFirstAuthenticatorRegistration, nodeFingerprint, verifyAccountEnrollment, verifyAccountEnrollmentFromOffer, verifyDeviceEnrollmentIntentForOffer, verifyDeviceEnrollmentOffer, verifyPasskeyEnrollmentRequestFromOffer } from '../../../generated/vibeface_secure.js'
import { enrollmentHref, parseEnrollmentReference } from '@/lib/enrollment-reference'
import type { EnrollmentReference } from '@/lib/enrollment-reference'
import { parseEnrollmentRequest, requestDeviceEnrollment, verifyEnrollmentStatus } from '@/lib/enrollment'
import type { EnrollmentRequest, EnrollmentStatus } from '@/lib/enrollment'
import { assertNodePasskey } from '@/lib/node-passkey'
import { registerNodePasskey } from '@/lib/node-passkey-registration'

type Phase = 'initializing' | 'blocked' | 'vault_empty' | 'vault_locked' | 'vault_unavailable' | 'key_lost' | 'disconnected' | 'connecting' | 'unpaired' | 'no_project' | 'ready'
type Review = { kind: 'bootstrap' | 'recovery'; accountId: string; spaceId: string; serviceGrant: { projectId: string; actions: string[] }; targetChanged: boolean }
type Sensitive = { cancel(): void; download?: () => void; bootstrap?: (saved: boolean) => Promise<void>; recover?: (saved: boolean) => Promise<void>; refreshEnrollment?: () => Promise<void>; passkeyEnrollment?: (protectLocal: boolean) => Promise<void>; retryPasskeyRegistration?: () => Promise<void> }
type IncomingEnrollment = { accountId: string; newDevicePublicKey: string; requestedRights: string[]; status: EnrollmentStatus }
type ManagerEnrollment = { offer: string; reference: EnrollmentReference; material: ReturnType<ReturnType<typeof verifyScopedAuthentication>['enrollmentMaterial']> }
type Session = { renewal?: number; authorization?: ReturnType<typeof verifyScopedAuthentication>; channel: DeviceChannel; entry: ConnectionEntry; info: EntryInfo; generation: number }
function releaseSession(active: Session | null) {
  if (active?.renewal !== undefined) window.clearTimeout(active.renewal)
  active?.authorization?.release()
}
type DeviceContextValue = {
  identity: PublicIdentity | null; vaults: VaultProfile[]; spaces: AuthorizationTrust[]; ready: ReadyConnection | null; entries: ConnectionEntry[]; phase: Phase; error: string
  entry: ConnectionEntry | null; info: EntryInfo | null; directory: AccountDeviceDirectory | null; review: Review | null; initialInvitation: string
  connect(entry: ConnectionEntry, spaceId?: string, projectId?: string): Promise<void>; selectSpace(spaceId: string): Promise<void>; selectProject(projectId: string): Promise<void>; disconnect(): void; saveEntry(entry: ConnectionEntry): void; removeEntry(entry: ConnectionEntry): void
  initializeVault(options: ClientVaultOptions): Promise<void>; lockVault(): void; backupVault(): Promise<void>; useConnectionCard(content: string, password?: string): Promise<void>
  refreshDirectory(): Promise<void>
  createEnrollmentOffer(manageDevices: boolean): Promise<string>
  incomingEnrollment: IncomingEnrollment | null; enrollmentRequests: EnrollmentRequest[]; managerEnrollmentActive: boolean
  refreshIncomingEnrollment(): Promise<void>; refreshEnrollmentRequests(): Promise<EnrollmentRequest[]>; approveEnrollmentRequest(requestId: string): Promise<void>; completeEnrollmentRequest(requestId: string): Promise<void>; revokeDevice(deviceId: string): Promise<void>
  joinWithPasskey(protectLocal: boolean): Promise<void>
  localProtectionNotice: string
  firstPasskeyPending: boolean; registerAccountPasskey(input: { file: File; password: string; protectLocal: boolean }): Promise<void>; resumePasskeyRegistration(): Promise<void>
  beginRecovery(input: { file: File; password: string; connectionCard: string }): Promise<void>
  reencrypt(input: { file: File; oldPassword: string; password: string }): Promise<void>
  downloadBackup(): void; confirmBootstrap(saved: boolean): Promise<void>; confirmRecovery(saved: boolean): Promise<void>; cancelSensitive(): void
}
const DeviceContext = createContext<DeviceContextValue | null>(null)
const changed = () => new Error('设备连接已切换。原操作结果需要核对，不会自动重发。')
function readPersistent(key: string, required = false): unknown { try { const value = localStorage.getItem(`vibeface.v3.${key}`); return value ? JSON.parse(value) : null } catch { if (required) throw new Error('无法读取已保存的账户信任，请保留浏览器数据并检查存储权限。'); return null } }
function writePersistent(key: string, value: unknown) { try { localStorage.setItem(`vibeface.v3.${key}`, JSON.stringify(value)) } catch { throw new Error('浏览器无法保存设备记录。请允许此站点使用本地存储。') } }
function savedEntries(profile: string, read: (key: string) => unknown): ConnectionEntry[] {
  const value = read(`profile.${profile}.connections`)
  if (!Array.isArray(value)) return []
  return value.flatMap(item => { try { return [parseEntry(item)] } catch { return [] } })
}
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
  const [directory, setDirectory] = useState<AccountDeviceDirectory | null>(null)
  const [review, setReview] = useState<Review | null>(null)
  const [initialInvitation, setInitialInvitation] = useState('')
  const [incomingEnrollment, setIncomingEnrollment] = useState<IncomingEnrollment | null>(null)
  const [enrollmentRequests, setEnrollmentRequests] = useState<EnrollmentRequest[]>([])
  const [managerEnrollmentActive, setManagerEnrollmentActive] = useState(false)
  const [localProtectionNotice, setLocalProtectionNotice] = useState('')
  const [firstPasskeyPending, setFirstPasskeyPending] = useState(false)
  const managerEnrollment = useRef<ManagerEnrollment | null>(null)
  const generation = useRef(0)
  const session = useRef<Session | null>(null)
  const connecting = useRef<AbortController | null>(null)
  const sensitive = useRef<Sensitive | null>(null)
  const listeners = useRef(new Set<() => void>())
  const identityRef = useRef<PublicIdentity | null>(null)
  const temporaryRecords = useRef(new Map<string, unknown>())
  function load(key: string, required = false): unknown {
    return identityRef.current?.profileId === null ? temporaryRecords.current.get(key) ?? null : readPersistent(key, required)
  }
  function store(key: string, value: unknown) {
    if (identityRef.current?.profileId === null) temporaryRecords.current.set(key, JSON.parse(JSON.stringify(value)))
    else writePersistent(key, value)
  }

  function storageProfile(own = identityRef.current) {
    if (!own) throw new Error('请先解锁设备配置。')
    return own.profileId ?? `temporary-${own.nodeId}`
  }
  const profileKey = (key: string) => `profile.${storageProfile()}.${key}`
  const savedSpaces = () => array(load(profileKey('policy-trusts'), true) ?? []).map(parseAuthorizationTrust)

  const cancelSensitive = useCallback(() => { sensitive.current?.cancel(); sensitive.current = null; setReview(null); setIncomingEnrollment(null); setFirstPasskeyPending(false) }, [])
  const disconnect = useCallback(() => {
    generation.current += 1
    cancelSensitive()
    connecting.current?.abort(); connecting.current = null
    const previous = session.current; session.current = null
    releaseSession(previous); previous?.channel.close()
    listeners.current.clear(); setReady(null); setDirectory(null); setInfo(null); setEntry(null)
    managerEnrollment.current = null; setEnrollmentRequests([]); setManagerEnrollmentActive(false)
    setLocalProtectionNotice('')
    setPhase('disconnected'); setError('')
  }, [cancelSensitive])
  const saveEntry = useCallback((candidate: ConnectionEntry) => {
    const parsed = parseEntry(candidate)
    const next = [...savedEntries(storageProfile(), load).filter(item => entryKey(item) !== entryKey(parsed)), parsed]
    store(profileKey('connections'), next); setEntries(next)
  }, [])
  const removeEntry = useCallback((candidate: ConnectionEntry) => {
    const next = savedEntries(storageProfile(), load).filter(item => entryKey(item) !== entryKey(candidate))
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
  async function persistPolicyTrust(active: Session, trust: AuthorizationTrust, signal?: AbortSignal) {
    const own = identityRef.current
    if (!own) throw changed()
    const profile = storageProfile(own), key = `profile.${profile}.policy-trusts`, temporary = own.profileId === null
    if (!('locks' in navigator)) throw new Error('需要 Web Locks 才能保存已提交的账户政策。请保留当前页面并核对设备权限。')
    await navigator.locks.request(`profile.${profile}.policy-trust-writer`, () => {
      signal?.throwIfAborted()
      if (temporary && session.current !== active) throw changed()
      const known = array((temporary ? temporaryRecords.current.get(key) : readPersistent(key, true)) ?? []).map(parseAuthorizationTrust)
      const updated = mergeAuthorizationTrust(trust, known)
      const next = [...known.filter(item => item.spaceId !== updated.spaceId).map(item => mergeAuthorizationTrust(item, [updated])), updated]
      if (temporary) temporaryRecords.current.set(key, JSON.parse(JSON.stringify(next)))
      else writePersistent(key, next)
      if (session.current === active) setSpaces(next)
    })
    if (session.current !== active || signal?.aborted) throw changed()
  }
  async function saveLocalPasskeyProtection(result: { credentialId: string; localPrf: Uint8Array | null; prfSalt: Uint8Array | null } | null, signal: AbortSignal) {
    signal.throwIfAborted()
    if (!result?.localPrf || !result.prfSalt) {
      setLocalProtectionNotice('账户 Passkey 已验证。本次验证未取得本机 Passkey 保护结果，请继续使用独立口令。')
      return
    }
    try {
      await protectClientVaultWithPrf({ credentialId: result.credentialId, salt: result.prfSalt, prf: result.localPrf }, signal)
      signal.throwIfAborted(); setLocalProtectionNotice('此设备配置已启用本机 Passkey 保护。独立口令仍可使用。')
    } catch (error) {
      signal.throwIfAborted(); setLocalProtectionNotice(`账户 Passkey 已验证。本机保护未保存，请继续使用独立口令。${messageOf(error)}`)
    }
  }
  const targetKey = (spaceId: string, entry: ConnectionEntry) => profileKey(`target.${JSON.stringify([spaceId, entryKey(entry)])}`)
  async function authenticate(authValue: unknown, active = current(), expected: AuthorizationTrust | null = null, projectId?: string, selection?: AuthorizationSelection, signal?: AbortSignal) {
    signal?.throwIfAborted()
    const cancel = () => { if (session.current === active) active.channel.close() }
    signal?.addEventListener('abort', cancel, { once: true })
    try { await authenticateSession(authValue, active, expected, projectId, selection, signal) }
    finally { signal?.removeEventListener('abort', cancel) }
  }
  async function authenticateSession(authValue: unknown, active: Session, expected: AuthorizationTrust | null, projectId?: string, selection?: AuthorizationSelection, signal?: AbortSignal) {
    signal?.throwIfAborted()
    const request = scoped(active)
    const own = identityRef.current
    if (!own) throw new Error('浏览器密钥尚未初始化。')
    if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能安全保存账户政策。请更换安全浏览器入口。')
    const authorization = await navigator.locks.request(profileKey('policy-trust-writer'), () => {
      signal?.throwIfAborted()
      if (session.current !== active) throw changed()
      const known = savedSpaces()
      const selectedSpace = load(profileKey('last-space'))
      const candidate = expected ?? known.find(trust => trust.spaceId === selectedSpace) ?? known[0]
      const trust = mergeAuthorizationTrust(parseAuthorizationTrust(candidate), known)
      const verified = verifyScopedAuthentication(authValue, own, trust, active.channel.publicKey, selection)
      try {
        const others = known.filter(trust => trust.spaceId !== verified.trust.spaceId)
        const next = [...others.map(trust => mergeAuthorizationTrust(trust, [verified.trust])), verified.trust]
        store(profileKey('policy-trusts'), next)
        store(profileKey('last-space'), verified.scope.space_id)
        setSpaces(next)
      } catch (error) { verified.release(); throw error }
      return verified
    })
    if (session.current !== active || signal?.aborted) { authorization.release(); throw changed() }
    releaseSession(active); active.authorization = authorization
    saveEntry(active.entry)
    store(profileKey('last-entry'), active.entry)
    const projectKey = profileKey(`project.${authorization.scope.space_id}.${authorization.audience.service}`)
    const selectedProject = projectId ?? load(projectKey)
    const project = authorization.projects.find(project => project.id === selectedProject) ?? (projectId ? undefined : authorization.projects[0])
    if (!project) {
      setReady(null); setPhase('no_project')
      setError(projectId ? '所选项目已不在当前授权内，请重新核对项目权限。' : '账户已验证，当前入口没有可用的已授权项目。请在 Node 上登记项目并授权后重新连接。')
      return
    }
    store(projectKey, project.id)
    store(targetKey(authorization.scope.space_id, active.entry), { resourceId: project.id, audience: authorization.audience })
    const selectedProjectId = project.id
    async function signedCall(method: string, params: Record<string, unknown> = {}, channelMethod = 'rpc', beforeSend?: (proof: AuthorizedRequest) => void, facts?: RpcFacts) {
      if (session.current !== active) throw changed()
      if (params.resource_id !== undefined && params.resource_id !== selectedProjectId) throw new Error('请求属于其他项目，已拒绝签名。')
      params = { ...params, resource_id: selectedProjectId }
      if (['notifications', 'ack'].includes(method)) {
        if (params.client_id !== undefined && params.client_id !== authorization.scope.device_id) throw new Error('通知请求属于其他设备，已拒绝签名。')
        params = { ...params, client_id: authorization.scope.device_id }
      }
      const prepared = prepareRpc({ method, params, spaceId: authorization.scope.space_id, authorizationId: crypto.randomUUID() })
      const body = record(JSON.parse(JSON.stringify(prepared.body)))
      const rawContext = prepareContext(body, authorization.audience.device, selectedProjectId, facts)
      const intent = record(JSON.parse(authorization.prepare(JSON.stringify(body), JSON.stringify(rawContext), prepared.action, prepared.authorizationId)))
      const template = record(await request('prepare_invocation', { body, intent }))
      const normalized = record(template.body)
      verifyPreparedBody(body, normalized, facts)
      const invocationTemplate = record(template.invocation)
      if (!same(invocationTemplate.context, rawContext.context)) throw new Error('节点准备的执行范围与本地已验证的工单或会话不同。')
      const invocation = record(JSON.parse(authorization.sign(JSON.stringify(invocationTemplate), JSON.stringify(normalized), prepared.action, prepared.authorizationId)))
      beforeSend?.({ body: normalized, invocation })
      const result = await callSession(active, channelMethod, normalized, invocation)
      if (channelMethod === 'subscribe') {
        const expiresAt = integer(record(invocation.payload).expires_at_ms)
        const delay = subscriptionRenewalDelay(expiresAt, Date.now())
        if (active.renewal !== undefined) window.clearTimeout(active.renewal)
        active.renewal = window.setTimeout(() => {
          if (session.current !== active) return
          void signedCall('conversations', {}, 'subscribe').catch(error => {
            if (session.current !== active) return
            active.channel.close(); setError(messageOf(error))
          })
        }, delay)
      }
      return { result, body: normalized, invocation }
    }
    try { await signedCall('conversations', {}, 'subscribe') }
    catch (error) { authorization.release(); if (active.authorization === authorization) active.authorization = undefined; throw error }
    const connection: ReadyConnection = {
      generation: active.generation, entry: active.entry, scope: authorization.scope, audience: authorization.audience, info: active.info,
      project, projects: authorization.projects, audiencePublicKey: text(authorization.policy.audience_public_key),
      authorization: { enrollmentRights: authorization.enrollmentRights, actions: authorization.actions, authorityEpoch: authorization.authorityEpoch },
      request: (method, params) => { if (session.current !== active) return Promise.reject(changed()); return request(method, params) },
      rpc: (method, params, facts) => signedCall(method, params, 'rpc', undefined, facts).then(reply => reply.result),
      rpcWithProof: (method, params, beforeSend, facts) => signedCall(method, params, 'rpc', beforeSend, facts),
      subscribe(listener) { if (session.current !== active) return () => {}; listeners.current.add(listener); return () => { listeners.current.delete(listener) } },
      isCurrent: () => session.current === active && generation.current === active.generation,
    }
    setDirectory(null); setReady(connection); setPhase('ready'); setError('')
  }
  async function open(entryValue: ConnectionEntry, expectedPublicKey?: string) {
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
          generation.current += 1; releaseSession(session.current); session.current = null; cancelSensitive(); listeners.current.clear(); setReady(null); setDirectory(null); setPhase('disconnected'); setError(messageOf(reason))
        },
      })
      if (version !== generation.current) { channel.close(); throw changed() }
      if (expectedPublicKey && channel.publicKey !== expectedPublicKey) { channel.close(); throw new Error('连接卡中的可信公钥与加密连接目标不一致。') }
      const details = parseInfo(await channel.request('identity', { method: 'status', params: {} }))
      if (version !== generation.current) { channel.close(); throw changed() }
      if (details.node_id !== parsed.node_id || details.public_key !== channel.publicKey) { channel.close(); throw new Error('入口报告的设备与已核验指纹不一致。') }
      const active = { channel, entry: parsed, info: details, generation: version }
      session.current = active; setInfo(details); setPhase('unpaired'); return active
    } catch (cause) {
      if (version === generation.current) { disconnect(); setError(messageOf(cause)) }
      throw cause
    }
  }
  async function connect(candidate: ConnectionEntry, spaceId?: string, projectId?: string) {
    const known = savedSpaces()
    const selectedSpace = spaceId ?? load(profileKey('last-space'), true)
    if (selectedSpace !== null && typeof selectedSpace !== 'string') throw new Error('已保存的空间选择无效。请保留浏览器数据并核查。')
    const trust = selectedSpace ? known.find(trust => trust.spaceId === selectedSpace) : known[0]
    if (!trust) throw new Error('请先使用新格式连接卡为此设备配置取得账户授权。')
    const savedTarget = load(targetKey(trust.spaceId, candidate), true)
    const selection = savedTarget ? parseAuthorizationSelection(savedTarget) : undefined
    if (selection && projectId) selection.resourceId = projectId
    const active = await open(candidate)
    const request = scoped(active)
    let auth: unknown
    try {
      auth = await request('authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: identityRef.current?.publicKey, ...(selection ? { resource_id: selection.resourceId, audience: selection.audience } : projectId ? { resource_id: projectId } : {}) })
    }
    catch (cause) { if (isResponseError(cause)) { setError('此设备尚未取得所选空间授权。请在已授权设备上批准。'); return }; throw cause }
    await authenticate(auth, active, trust, projectId ?? selection?.resourceId, selection)
  }
  async function selectSpace(spaceId: string) {
    if (!ready?.isCurrent()) throw changed()
    if (ready.scope.space_id === spaceId) return
    await connect(ready.entry, spaceId)
  }
  async function selectProject(projectId: string) {
    if (!ready?.isCurrent()) throw changed()
    if (ready.project.id === projectId) return
    if (!ready.projects.some(project => project.id === projectId)) throw new Error('项目不在已验证授权内。')
    await connect(ready.entry, ready.scope.space_id, projectId)
  }
  async function refreshDirectory() {
    if (!ready?.isCurrent()) return
    const active = current()
    const authorization = active.authorization
    if (!authorization || !authorization.enrollmentRights.some(right => right === 'enroll_device' || right === 'revoke_device')) throw new Error('此设备没有账户设备管理权限。')
    const response = await callSession(active, 'identity', { method: 'device_directory', params: { account_id: authorization.scope.account_id, space_id: authorization.scope.space_id, device_public_key: identityRef.current?.publicKey, resource_id: ready.project.id, audience: authorization.audience } })
    const directory = authorization.directory(response)
    if (!ready.isCurrent()) throw changed()
    setDirectory(directory)
  }
  async function createEnrollmentOffer(manageDevices: boolean) {
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    if (!authorization || !authorization.enrollmentRights.includes('enroll_device')) throw new Error('当前设备没有新增账户设备的权限。')
    if (active.entry.relay || ready.audiencePublicKey !== active.channel.publicKey) throw new Error('请使用目标 Node 的直接入口，再生成新设备邀请。')
    const material = authorization.enrollmentMaterial()
    if (material.expiresAt === null) throw new Error('当前设备没有有效的账户管理期限。')
    const rights = manageDevices ? authorization.enrollmentRights.filter(right => right === 'enroll_device' || right === 'revoke_device').sort() : []
    const offer = createClientEnrollmentOffer(ready.scope.account_id, active.entry.address, active.channel.publicKey, JSON.stringify(rights), material.expiresAt, JSON.stringify(material.pins), JSON.stringify(material.bundle))
    const checked = verifyDeviceEnrollmentOffer(offer, Date.now())
    try {
      const expected = { version: 2, kind: 'vibeface-device-enrollment', endpoint: active.entry.address, node_public_key: active.channel.publicKey,
        account_id: checked.accountId(), offer_hash: checked.offerHash(), expires_at_ms: checked.expiresAtMs() }
      const reply = record(await callSession(active, 'identity', { method: 'enrollment_offer_publish', params: { offer: JSON.parse(offer) } }))
      const reference = parseEnrollmentReference(reply.reference)
      if (!same(reference, expected)) throw new Error('节点返回的邀请引用与本地签名邀请不同。')
      managerEnrollment.current = { offer, reference, material }; setEnrollmentRequests([]); setManagerEnrollmentActive(true)
      return enrollmentHref(location.href, reference)
    } finally { checked.free() }
  }
  async function refreshEnrollmentRequests() {
    const active = current(), flow = managerEnrollment.current
    if (!flow) throw new Error('请先生成此设备的新邀请。')
    const response = record(await callSession(active, 'identity', { method: 'enrollment_pending', params: { account_id: flow.reference.account_id, offer_hash: flow.reference.offer_hash } }))
    if (managerEnrollment.current !== flow) throw changed()
    const requests = array(response.requests).map(parseEnrollmentRequest)
    if (new Set(requests.map(item => item.requestId)).size !== requests.length) throw new Error('设备接入列表存在重复申请。')
    setEnrollmentRequests(requests)
    return requests
  }
  async function managerRequest(requestId: string) {
    const flow = managerEnrollment.current
    if (!flow) throw new Error('设备邀请已结束，请重新生成。')
    const shown = enrollmentRequests.find(item => item.requestId === requestId)
    if (!shown) throw new Error('请先刷新并核对要批准的完整设备公钥。')
    const requests = await refreshEnrollmentRequests()
    if (managerEnrollment.current !== flow) throw changed()
    const request = requests.find(item => item.requestId === requestId)
    if (!request || !same(request, shown)) throw new Error('此设备申请已变化。请重新核对最新显示的完整公钥和状态，再明确批准。')
    const offer = verifyDeviceEnrollmentOffer(flow.offer, Date.now())
    try {
      const intent = JSON.parse(verifyDeviceEnrollmentIntentForOffer(offer, JSON.stringify(request.unsignedIntent), request.newDevicePublicKey, Date.now()))
      if (!same(intent, request.unsignedIntent)) throw new Error('设备申请与原邀请不同。')
    } finally { offer.free() }
    return { flow, request }
  }
  async function approveEnrollmentRequest(requestId: string) {
    const active = current(), { flow, request } = await managerRequest(requestId)
    if (request.status !== 'requested') throw new Error('申请已不处于等待批准状态，请重新核对。')
    const signed = JSON.parse(approveClientEnrollmentIntent(JSON.stringify(request.unsignedIntent), JSON.stringify(flow.material.pins), JSON.stringify(flow.material.bundle)))
    await callSession(active, 'identity', { method: 'enrollment_approve', params: { account_id: flow.reference.account_id, offer_hash: flow.reference.offer_hash, request_id: requestId, intent: signed } })
    await refreshEnrollmentRequests()
  }
  async function applyDeviceChange(prepared: ReturnType<typeof prepareClientDeviceRevocation>, active: Session, requestId?: string, revokedDeviceId?: string) {
    let verified: ReturnType<typeof finishAccountDeviceChange> | null = null
    try {
      const reply = record(await callSession(active, 'identity', { method: 'account_device_commit', params: { request: JSON.parse(prepared.requestJson()), ...(requestId ? { request_id: requestId } : {}) } }))
      verified = finishAccountDeviceChange(prepared, JSON.stringify(record(reply.committed)), Date.now())
      if (!ready?.isCurrent() || !identityRef.current) throw changed()
      const trust = parseAuthorizationTrust({ kind: 'policy', accountId: ready.scope.account_id, spaceId: ready.scope.space_id, pins: JSON.parse(verified.pinsJson()) })
      await persistPolicyTrust(active, trust)
      if (revokedDeviceId === ready.scope.device_id) { lockVault(); return }
      const auth = await callSession(active, 'authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: identityRef.current.publicKey, resource_id: ready.project.id, audience: ready.audience })
      await authenticate(auth, active, trust, ready.project.id, { resourceId: ready.project.id, audience: ready.audience })
      managerEnrollment.current = null; setManagerEnrollmentActive(false); setEnrollmentRequests([])
      await refreshDirectory()
    } finally { verified?.free(); prepared.free() }
  }
  async function completeEnrollmentRequest(requestId: string) {
    const active = current(), { flow, request } = await managerRequest(requestId)
    if (request.status !== 'possessed' || !request.intent || !request.certificate || !same(request.intent.payload, request.unsignedIntent)) throw new Error('新设备尚未提供与原申请一致的密钥持有证明。')
    const qualified = verifyAccountEnrollment(JSON.stringify(request.intent), JSON.stringify(flow.material.pins), JSON.stringify(flow.material.bundle), Date.now())
    try {
      const prepared = prepareClientDeviceEnrollment(qualified, JSON.stringify(request.certificate), JSON.stringify(flow.material.pins), JSON.stringify(flow.material.bundle))
      await applyDeviceChange(prepared, active, requestId)
    } finally { qualified.free() }
  }
  async function revokeDevice(deviceId: string) {
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    if (!authorization?.enrollmentRights.includes('revoke_device')) throw new Error('当前设备没有撤销账户设备的权限。')
    const material = authorization.enrollmentMaterial()
    const prepared = prepareClientDeviceRevocation(ready.scope.account_id, deviceId, JSON.stringify(material.pins), JSON.stringify(material.bundle))
    await applyDeviceChange(prepared, active, undefined, deviceId)
  }
  async function registerAccountPasskey(input: { file: File; password: string; protectLocal: boolean }) {
    if (!ready?.isCurrent() || !identityRef.current) throw changed()
    const connection = ready, active = current(), authorization = active.authorization, own = identityRef.current
    if (!authorization?.enrollmentRights.includes('manage_authenticators')) throw new Error('当前设备没有账户 Passkey 管理权限。')
    if (connection.audiencePublicKey !== active.channel.publicKey) throw new Error('请直接连接账户所属 Node，再登记账户 Passkey。')
    if (input.protectLocal && !own.profileId) throw new Error('本机 Passkey 保护需要独立口令设备配置。')
    const material = authorization.enrollmentMaterial()
    if (material.expiresAt === null) throw new Error('当前账户管理授权没有有效期限。')
    cancelSensitive()
    const controller = new AbortController()
    let opened: Awaited<ReturnType<typeof openAccountPackage>> | null = null
    let registration: Awaited<ReturnType<typeof registerNodePasskey>> | null = null
    let prepared: ReturnType<typeof prepareClientFirstAuthenticatorRegistration> | null = null
    let intentJson = ''
    let completion: Record<string, unknown> | null = null
    const finish = async (receipt: Record<string, unknown>) => {
      if (!prepared) throw new Error('原 Passkey 注册材料已释放，请重新连接核对。')
      controller.signal.throwIfAborted()
      const verified = finishFirstAuthenticatorRegistration(prepared, JSON.stringify(array(receipt.journal)), Date.now())
      let trust: AuthorizationTrust
      try { trust = parseAuthorizationTrust({ kind: 'policy', accountId: connection.scope.account_id, spaceId: connection.scope.space_id, pins: JSON.parse(verified.pinsJson()) }) }
      finally { verified.free() }
      await persistPolicyTrust(active, trust)
      controller.signal.throwIfAborted()
      const auth = await callSession(active, 'authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: own.publicKey, resource_id: connection.project.id, audience: connection.audience })
      await authenticate(auth, active, trust, connection.project.id, { resourceId: connection.project.id, audience: connection.audience }, controller.signal)
      if (input.protectLocal) await saveLocalPasskeyProtection(registration, controller.signal)
      if (sensitive.current === flow) cancelSensitive()
    }
    const flow: Sensitive = {
      cancel() { controller.abort(); opened?.release(); opened = null; registration?.release(); registration = null; prepared?.free(); prepared = null; intentJson = ''; completion = null; input.password = '' },
      async retryPasskeyRegistration() {
        if (!prepared || !completion || !intentJson) throw new Error('当前没有需要核对的原注册提交。')
        controller.signal.throwIfAborted()
        const pending = record(await callSession(active, 'identity', { method: 'passkey_prepare_registration', params: { intent: JSON.parse(intentJson), origin: location.origin } }))
        controller.signal.throwIfAborted()
        if (pending.status === 'completed') await finish(record(pending.receipt))
        else {
          if (pending.status !== 'pending') throw new Error('节点未确认原注册的待处理状态。')
          const receipt = record(await callSession(active, 'identity', { method: 'passkey_complete_registration', params: { completion, origin: location.origin } }))
          controller.signal.throwIfAborted(); await finish(receipt)
        }
      },
    }
    sensitive.current = flow
    try {
      opened = await openAccountPackage(input.file, input.password)
      input.password = ''; controller.signal.throwIfAborted()
      if (opened.scope.account_id !== connection.scope.account_id || opened.entry.nodePublicKey !== active.channel.publicKey) throw new Error('恢复包与当前账户或可信 Node 不同。')
      intentJson = prepareClientFirstAuthenticatorIntent(opened.material, JSON.stringify(material.pins), JSON.stringify(material.bundle), material.expiresAt)
      const pending = record(await callSession(active, 'identity', { method: 'passkey_prepare_registration', params: { intent: JSON.parse(intentJson), origin: location.origin } }))
      controller.signal.throwIfAborted()
      if (pending.status === 'completed') throw new Error('节点记录此注册已完成。请重新连接核对账户 Passkey，不会重复登记。')
      if (pending.status !== 'pending') throw new Error('节点未确认原 Passkey 注册请求。')
      registration = await registerNodePasskey({ pendingResponse: pending, signedIntent: JSON.parse(intentJson), nodePublicKey: active.channel.publicKey, signal: controller.signal, requestPrf: input.protectLocal })
      controller.signal.throwIfAborted()
      if (!opened) throw changed()
      prepared = prepareClientFirstAuthenticatorRegistration(opened.material, JSON.stringify(material.pins), JSON.stringify(material.bundle), intentJson, registration.registration)
      opened.release(); opened = null
      completion = { intent: JSON.parse(intentJson), pending_id: registration.pendingId, response: registration.response, journal: JSON.parse(prepared.completionJournalJson()) }
      setFirstPasskeyPending(true)
      const receipt = record(await callSession(active, 'identity', { method: 'passkey_complete_registration', params: { completion, origin: location.origin } }))
      controller.signal.throwIfAborted(); await finish(receipt)
    } catch (error) {
      if (!prepared || controller.signal.aborted) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }
      throw error
    } finally { input.password = ''; opened?.release(); opened = null }
  }
  async function resumePasskeyRegistration() {
    if (!sensitive.current?.retryPasskeyRegistration) throw new Error('当前没有待核对的账户 Passkey 注册。')
    await sensitive.current.retryPasskeyRegistration()
  }
  async function beginEnrollment(reference: EnrollmentReference) {
    if (savedSpaces().some(trust => trust.accountId !== reference.account_id)) throw new Error('请为另一个账户创建独立设备配置。')
    const own = identityRef.current
    if (!own) throw new Error('请先创建或解锁新设备配置。')
    const active = await open({ address: reference.endpoint, node_id: nodeFingerprint(reference.node_public_key), relay: false }, reference.node_public_key)
    const controller = new AbortController()
    let pending: Awaited<ReturnType<typeof requestDeviceEnrollment>> | null = null
    let passkey: {
      intentJson: string; offer: ReturnType<typeof verifyDeviceEnrollmentOffer>; qualified: ReturnType<typeof verifyPasskeyEnrollmentRequestFromOffer>
      prepared: ReturnType<typeof prepareClientPasskeyDeviceEnrollment> | null; completion: Record<string, unknown> | null
      assertion: Awaited<ReturnType<typeof assertNodePasskey>> | null
    } | null = null
    const update = (status: EnrollmentStatus) => setIncomingEnrollment({ accountId: reference.account_id, newDevicePublicKey: own.publicKey, requestedRights: pending?.rights ?? [], status })
    const flow: Sensitive = {
      cancel() {
        controller.abort(); pending = null
        passkey?.prepared?.free(); passkey?.qualified.free(); passkey?.offer.free()
        passkey?.assertion?.localPrf?.fill(0); passkey?.assertion?.prfSalt?.fill(0); passkey = null
      },
      async passkeyEnrollment(protectLocal) {
        if (!pending || controller.signal.aborted) throw changed()
        if (protectLocal && !own.profileId) throw new Error('本机 Passkey 保护需要独立口令设备配置。临时会话只验证账户身份。')
        const original = pending
        if (!passkey) {
          const offer = verifyDeviceEnrollmentOffer(original.offerJson, Date.now())
          try {
            const intentJson = signClientPasskeyEnrollmentIntent(JSON.stringify(original.unsignedIntent), offer)
            const qualified = verifyPasskeyEnrollmentRequestFromOffer(intentJson, offer, Date.now())
            passkey = { intentJson, offer, qualified, prepared: null, completion: null, assertion: null }
          } catch (error) { offer.free(); throw error }
        }
        const attempt = passkey
        const preparedResponse = record(await callSession(active, 'identity', { method: 'passkey_prepare_device', params: { intent: JSON.parse(attempt.intentJson), origin: location.origin } }))
        controller.signal.throwIfAborted()
        let receipt: Record<string, unknown>
        if (preparedResponse.status === 'completed') receipt = record(preparedResponse.receipt)
        else {
          if (preparedResponse.status !== 'pending') throw new Error('节点未确认原 Passkey 接入请求。')
          if (!attempt.completion) {
            attempt.assertion?.localPrf?.fill(0); attempt.assertion?.prfSalt?.fill(0); attempt.assertion = null
            const accountProof = record(JSON.parse(original.offerJson).payload)
            attempt.assertion = await assertNodePasskey({ pendingResponse: preparedResponse, signedIntent: JSON.parse(attempt.intentJson), nodePublicKey: reference.node_public_key,
              pins: array(original.trust.pins).filter(value => record(record(value).domain).kind === 'account'), journal: array(accountProof.manager_journal),
              signal: controller.signal, requestPrf: protectLocal,
              verify: (pendingId, response) => callSession(active, 'identity', { method: 'passkey_verify_device', params: { intent: JSON.parse(attempt.intentJson), pending_id: pendingId, response, origin: location.origin } }),
            })
            if (controller.signal.aborted) { attempt.assertion.localPrf?.fill(0); attempt.assertion.prfSalt?.fill(0) }
            controller.signal.throwIfAborted()
            const certificate = signClientPasskeyEnrollmentPossession(attempt.qualified, attempt.assertion.pendingNonce)
            attempt.prepared = prepareClientPasskeyDeviceEnrollment(attempt.intentJson, certificate, JSON.stringify(attempt.assertion.proof), attempt.offer)
            const request = record(JSON.parse(attempt.prepared.requestJson()))
            attempt.completion = { intent: JSON.parse(attempt.intentJson), pending_id: attempt.assertion.pendingId, journal: [{ kind: 'account', value: record(request.proposal) }] }
          }
          receipt = record(await callSession(active, 'identity', { method: 'passkey_complete_device', params: { completion: attempt.completion, origin: location.origin } }))
          controller.signal.throwIfAborted()
        }
        let trust = original.trust
        if (attempt.prepared) {
          const commits = array(receipt.journal).map(value => record(value)).filter(entry => entry.kind === 'account_commit')
          if (commits.length !== 1) throw new Error('Passkey 接入回执缺少唯一账户提交证明。请保留原请求并核对。')
          const finished = finishAccountDeviceChange(attempt.prepared, JSON.stringify(record(commits[0].value)), Date.now())
          try { trust = parseAuthorizationTrust({ ...original.trust, pins: JSON.parse(finished.pinsJson()) }) }
          finally { finished.free() }
          await persistPolicyTrust(active, trust)
        }
        controller.signal.throwIfAborted()
        const auth = await callSession(active, 'authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: own.publicKey })
        controller.signal.throwIfAborted()
        await authenticate(auth, active, trust, undefined, undefined, controller.signal)
        if (protectLocal) await saveLocalPasskeyProtection(attempt.assertion, controller.signal)
        if (sensitive.current === flow) cancelSensitive()
      },
      async refreshEnrollment() {
        if (!pending || controller.signal.aborted) throw changed()
        const original = pending
        const response = await callSession(active, 'identity', { method: 'enrollment_status', params: { account_id: reference.account_id, offer_hash: reference.offer_hash, request_id: original.requestId } })
        controller.signal.throwIfAborted()
        const status = verifyEnrollmentStatus(response, original)
        if (status.status === 'requested') { update(status.status); return }
        if (!status.intent) throw new Error('节点未提供原设备签署的批准意图。')
        const offer = verifyDeviceEnrollmentOffer(original.offerJson, Date.now())
        let qualified: ReturnType<typeof verifyAccountEnrollmentFromOffer> | null = null
        try {
          qualified = verifyAccountEnrollmentFromOffer(JSON.stringify(status.intent), offer, Date.now())
          const certificate = JSON.parse(signClientEnrollmentPossession(qualified, original.nonce))
          if (status.status === 'approved') {
            await callSession(active, 'identity', { method: 'enrollment_possession', params: { account_id: reference.account_id, offer_hash: reference.offer_hash, request_id: original.requestId, certificate } })
            controller.signal.throwIfAborted(); update('possessed'); return
          }
          if (!status.certificate || !same(status.certificate, certificate)) throw new Error('接入记录中的持有证明与此设备原证明不同。')
          if (status.status === 'possessed') { update(status.status); return }
          if (!status.completion) throw new Error('节点未确认账户设备变更已提交。')
          const auth = await callSession(active, 'authenticate', { account_id: original.trust.accountId, space_id: original.trust.spaceId, device_public_key: own.publicKey })
          controller.signal.throwIfAborted()
          await authenticate(auth, active, original.trust, undefined, undefined, controller.signal)
          if (sensitive.current === flow) cancelSensitive()
        } finally { qualified?.free(); offer.free() }
      },
    }
    sensitive.current = flow
    try {
      pending = await requestDeviceEnrollment(reference, own.publicKey, (method, params) => callSession(active, 'identity', { method, params }), controller.signal)
      controller.signal.throwIfAborted()
      await persistPolicyTrust(active, pending.trust, controller.signal)
      saveEntry(active.entry); store(profileKey('last-entry'), active.entry); store(profileKey('last-space'), pending.trust.spaceId)
      controller.signal.throwIfAborted(); update('requested')
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel(); throw error }
  }
  async function refreshIncomingEnrollment() {
    if (!sensitive.current?.refreshEnrollment) throw new Error('当前没有待核对的设备接入申请。')
    await sensitive.current.refreshEnrollment()
  }
  async function joinWithPasskey(protectLocal: boolean) {
    if (!sensitive.current?.passkeyEnrollment) throw new Error('请先读取来自自己设备的有效邀请。')
    await sensitive.current.passkeyEnrollment(protectLocal)
  }
  async function useConnectionCard(content: string, password?: string) {
    const invitation = readInvitation(content)
    if (record(invitation).kind === 'vibeface-device-enrollment') { setInitialInvitation(''); await beginEnrollment(parseEnrollmentReference(invitation)); return }
    const card = parseConnectionCard(invitation)
    setInitialInvitation(''); content = ''
    if (card.kind === 'connection') {
      password = ''
      const active = await open(card.entry, card.nodePublicKey)
      const own = identityRef.current
      const trust = savedSpaces().find(trust => trust.spaceId === load(profileKey('last-space'))) ?? savedSpaces()[0]
      if (!trust || !own) throw new Error('此设备配置没有账户信任。请导入完整账户恢复包，或取得新设备授权。')
      const auth = await callSession(active, 'authenticate', { account_id: trust.accountId, space_id: trust.spaceId, device_public_key: own.publicKey })
      await authenticate(auth, active, trust)
      return
    }
    if (savedSpaces().length) throw new Error('此设备配置已属于一个账户。请为新账户创建独立设备配置。')
    if (!password) throw new Error('请设置完整账户恢复包的加密口令。')
    const active = await open(card.entry, card.nodePublicKey)
    if (active.info.initialized) throw new Error('此节点已被认领，请使用已授权设备登录或账户恢复。')
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof createAccount>> | null = null
    let downloaded = false
    let submitted = false
    const flow: Sensitive = {
      cancel() { controller.abort(); prepared?.bytes.fill(0); card.capability = ''; card.claimNonce = '' },
      download() {
        if (controller.signal.aborted || session.current !== active || !prepared) throw changed()
        downloadPackage(prepared.bytes, prepared.scope.account_id); downloaded = true
      },
      async bootstrap(saved) {
        if (!saved || !downloaded || !prepared) throw new Error('请先下载完整账户恢复包，并确认妥善保存。')
        if (controller.signal.aborted || session.current !== active || submitted) throw changed()
        const created = prepared
        if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能保存账户信任。')
        await navigator.locks.request(profileKey('policy-trust-writer'), () => {
          if (controller.signal.aborted || session.current !== active) throw changed()
          const trust = mergeAuthorizationTrust(created.trust, savedSpaces())
          store(profileKey('policy-trusts'), [trust]); store(profileKey('last-space'), trust.spaceId); setSpaces([trust])
          saveEntry(active.entry); store(profileKey('last-entry'), active.entry)
        })
        if (controller.signal.aborted || session.current !== active) throw changed()
        submitted = true
        try {
          await callSession(active, 'identity', { method: 'bootstrap', params: { pins: created.trust.pins, authorization: created.authorization } })
          if (controller.signal.aborted) throw new Error('初始化已提交，但本地确认已取消。请重新连接核对账户状态。')
          const auth = await callSession(active, 'authenticate', { account_id: created.trust.accountId, space_id: created.trust.spaceId, device_public_key: identityRef.current?.publicKey })
          await authenticate(auth, active, created.trust, undefined, undefined, controller.signal)
        } finally { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }
      },
    }
    sensitive.current = flow
    try {
      prepared = await createAccount({ card, password, signal: controller.signal, call: (method, params) => callSession(active, 'identity', { method, params }) })
      if (controller.signal.aborted || session.current !== active) { prepared.bytes.fill(0); throw changed() }
      setReview({ kind: 'bootstrap', accountId: prepared.scope.account_id, spaceId: prepared.scope.space_id, serviceGrant: prepared.serviceGrant, targetChanged: false })
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel(); throw error }
    finally { password = '' }
  }
  async function beginRecovery(input: { file: File; password: string; connectionCard: string }) {
    if (savedSpaces().length) throw new Error('账户恢复需要新的独立设备配置。请先锁定并创建新配置。')
    const version = generation.current
    let password = input.password; input.password = ''
    const opened = await openAccountPackage(input.file, password).catch(error => { password = ''; throw error })
    if (version !== generation.current) { opened.release(); password = ''; throw changed() }
    let card: ConnectionCard | null = null
    let publicKey: string
    let active: Session
    try {
      card = input.connectionCard.trim() ? parseConnectionCard(readInvitation(input.connectionCard)) : null
      publicKey = card?.nodePublicKey ?? opened.entry.nodePublicKey
      if (publicKey !== opened.entry.nodePublicKey) throw new Error('换 Node 恢复需要可信源政策见证，当前还不能在此页面提交。请先连接原 Node 恢复。')
      const target = card?.entry ?? { address: opened.entry.endpoint, node_id: nodeFingerprint(publicKey), relay: false }
      active = await open(target, publicKey)
    } catch (error) { opened.release(); password = ''; throw error }
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof recoverAccount>> | null = null
    let downloaded = false
    let submitted = false
    const flow: Sensitive = {
      cancel() { controller.abort(); opened.release(); password = ''; prepared?.bytes.fill(0); if (card?.kind === 'claim') { card.capability = ''; card.claimNonce = '' } },
      download() {
        if (controller.signal.aborted || session.current !== active || !prepared) throw changed()
        downloadPackage(prepared.bytes, prepared.scope.account_id); downloaded = true
      },
      async recover(saved) {
        if (!saved || !downloaded || !prepared) throw new Error('请先下载更新后的完整账户恢复包，并确认妥善保存。')
        if (controller.signal.aborted || session.current !== active || submitted) throw changed()
        const recovered = prepared
        if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能保存账户信任。')
        await navigator.locks.request(profileKey('policy-trust-writer'), () => {
          if (controller.signal.aborted || session.current !== active) throw changed()
          const trust = mergeAuthorizationTrust(recovered.trust, savedSpaces())
          store(profileKey('policy-trusts'), [trust]); store(profileKey('last-space'), trust.spaceId); setSpaces([trust])
          saveEntry(active.entry); store(profileKey('last-entry'), active.entry)
        })
        if (controller.signal.aborted || session.current !== active) throw changed()
        submitted = true
        try {
          await callSession(active, 'identity', { method: 'recover', params: recovered.commit })
          if (controller.signal.aborted) throw new Error('恢复已提交，但本地确认已取消。请重新连接核对授权状态。')
          const auth = await callSession(active, 'authenticate', { account_id: recovered.trust.accountId, space_id: recovered.trust.spaceId, device_public_key: identityRef.current?.publicKey })
          await authenticate(auth, active, recovered.trust, undefined, undefined, controller.signal)
        } finally { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }
      },
    }
    sensitive.current = flow
    try {
      if (!active.info.initialized && card?.kind !== 'claim') throw new Error('恢复到新 Node 需要它本机生成的认领连接卡。')
      const request = record(JSON.parse(prepareClientRecoveryContext(opened.material, publicKey)))
      const nonce = text(record(request.payload).nonce)
      const response = record(await callSession(active, 'identity', { method: 'recovery_context', params: { request, ...(card?.kind === 'claim' ? { capability: card.capability } : {}) } }))
      if (controller.signal.aborted || session.current !== active) throw changed()
      prepared = await recoverAccount({ material: opened.material, latestBundle: response.authorization, claim: response.claim,
        nodePublicKey: publicKey, claimNonce: nonce, endpoint: active.entry.address, password, signal: controller.signal,
        possession: params => callSession(active, 'identity', { method: 'recovery_node_possession', params }),
      })
      if (controller.signal.aborted || session.current !== active) { prepared.bytes.fill(0); throw changed() }
      setReview({ kind: 'recovery', accountId: prepared.scope.account_id, spaceId: prepared.scope.space_id, serviceGrant: prepared.serviceGrant, targetChanged: publicKey !== opened.entry.nodePublicKey })
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel(); throw error }
    finally { opened.release(); password = '' }
  }
  async function reencrypt(input: { file: File; oldPassword: string; password: string }) {
    const version = generation.current
    const opened = await openAccountPackage(input.file, input.oldPassword)
      .catch(error => { input.password = ''; throw error })
      .finally(() => { input.oldPassword = '' })
    try {
      if (version !== generation.current) throw changed()
      if (ready && opened.scope.account_id !== ready.scope.account_id) throw new Error('恢复包属于其他账户。')
      const bytes = sealAccountPackage(opened.material, input.password)
      try { downloadPackage(bytes, opened.scope.account_id) } finally { bytes.fill(0) }
    } finally { opened.release(); input.password = '' }
  }
  async function initializeVault(options: ClientVaultOptions) {
    const version = generation.current
    const own = await initClient(options)
    if (version !== generation.current) throw changed()
    identityRef.current = own; setIdentity(own); setPhase('disconnected'); setError('')
    setEntries(savedEntries(storageProfile(own), load)); setSpaces(savedSpaces())
    if (!initialInvitation) {
      const lastEntry = load(profileKey('last-entry'), true)
      if (lastEntry) await connect(parseEntry(lastEntry))
    }
  }
  function lockVault() {
    disconnect(); lockClient(); temporaryRecords.current.clear(); identityRef.current = null; setIdentity(null); setEntries([]); setSpaces([])
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
    const invitation = fragment.get('enroll') || fragment.get('bootstrap') || fragment.get('invite') || ''
    if (location.hash) { const clean = new URL(location.href); clean.hash = ''; history.replaceState(null, '', clean) }
    if (invitation) setInitialInvitation(invitation)
    clientVaultStatus().then(status => {
      if (mounted) { setVaults(status.profiles); setPhase(status.status === 'locked' ? 'vault_locked' : status.status === 'key_lost' ? 'key_lost' : status.status === 'unavailable' ? 'vault_unavailable' : 'vault_empty'); if (status.status === 'unavailable') setError(status.error) }
    }).catch(cause => { if (mounted) { setPhase('blocked'); setError(messageOf(cause)) } })
    return () => { mounted = false; generation.current += 1; lockClient(); temporaryRecords.current.clear(); sensitive.current?.cancel(); sensitive.current = null; connecting.current?.abort(); const previous = session.current; session.current = null; releaseSession(previous); previous?.channel.close(); listeners.current.clear() }
  }, [])
  const value: DeviceContextValue = {
    identity, vaults, spaces, ready, entries, phase, error, entry, info, directory, review, initialInvitation,
    connect, selectSpace, selectProject, disconnect, saveEntry, removeEntry, initializeVault, lockVault, backupVault, useConnectionCard, refreshDirectory, createEnrollmentOffer, beginRecovery, reencrypt,
    incomingEnrollment, enrollmentRequests, managerEnrollmentActive, localProtectionNotice, refreshIncomingEnrollment, refreshEnrollmentRequests, approveEnrollmentRequest, completeEnrollmentRequest, revokeDevice, joinWithPasskey,
    firstPasskeyPending, registerAccountPasskey, resumePasskeyRegistration,
    downloadBackup() { if (!sensitive.current?.download) throw new Error('当前没有待下载的恢复包。'); sensitive.current.download() },
    async confirmBootstrap(saved) { if (!sensitive.current?.bootstrap) throw new Error('初始化确认已失效。'); await sensitive.current.bootstrap(saved) },
    async confirmRecovery(saved) { if (!sensitive.current?.recover) throw new Error('恢复确认已失效。'); await sensitive.current.recover(saved) },
    cancelSensitive,
  }
  return <DeviceContext.Provider value={value}>{children}</DeviceContext.Provider>
}
export function useDevice() { const value = useContext(DeviceContext); if (!value) throw new Error('DeviceProvider 缺失。'); return value }
