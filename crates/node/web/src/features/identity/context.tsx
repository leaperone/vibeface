import { parseConnectionCard, parseGatewayConnectionCard } from '@/lib/connection-file'
import { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { prepareClientGatewayOwnerRoute, prepareClientGatewayReply, buildClientRemoteShareIntent, approveClientEnrollmentIntent, clientVaultStatus, connectDevice, createClientEnrollmentOffer, exportDeviceVault, initClient, lockClient, prepareClientDeviceEnrollment, prepareClientDeviceRevocation, prepareClientAccountNodeEnrollment, prepareClientFirstAuthenticatorIntent, prepareClientFirstAuthenticatorRegistration, prepareClientMembershipChange, prepareClientDirectionalGrant, prepareClientGrantRevocation, prepareClientTeamCreation, restoreClientTeamCreation, prepareClientPasskeyDeviceEnrollment, prepareClientRecoveryContext, prepareClientExecutionServiceRegistration, prepareClientRemoteExecutionShare, protectClientVaultWithPrf, readInvitation, signClientEnrollmentPossession, signClientPasskeyEnrollmentIntent, signClientPasskeyEnrollmentPossession } from '../../../transport.js'
import type { ClientVaultOptions, DeviceChannel, VaultProfile } from '../../../transport.js'
import {
  array, entryKey, integer, messageOf, parseEntry, parseInfo, record, same, text,
} from '@/lib/node'
import type { AuthorizedRequest, ConnectionCard, ConnectionEntry, EntryInfo, AccountDeviceDirectory, PublicIdentity, ReadyConnection, RpcFacts, GatewayConnectionCard } from '@/lib/node'
import { prepareRpc } from '@/lib/rpc-action'
import { prepareContext, verifyPreparedBody } from '@/lib/rpc-prepare'
import { validateRpcReplay } from '@/lib/rpc-replay'
import type { AuthorizationSelection, AuthorizationTrust } from '@/lib/authorization'
import { mergeAuthorizationTrust, parseAuthorizationSelection, parseAuthorizationTrust, subscriptionRenewalDelay, verifyScopedAuthentication } from '@/lib/authorization'
import { readClientPolicyAuthorization } from '@/lib/client-policy'
import type { ClientPolicyFrame } from '@/lib/client-policy'
import { gatewayCardJson, gatewayRequestFromCard, parseGatewayOwnerRoute, parseGatewayRouteRequest, parseGatewayRouteStatus } from '@/lib/gateway-owner'
import type { GatewayOwnerInput, GatewayOwnerRoute } from '@/lib/gateway-owner'
import { readRemoteSharePolicy } from '@/lib/source-share-policy'
import { readRemoteExecutionPolicy } from '@/lib/remote-execution-policy'
import type { RemoteExecutionPolicyUse } from '@/lib/remote-execution-policy-ingress'
import { readRemoteExecutionHistoryPolicy } from '@/lib/remote-execution-history-policy'
import { callExecutionResource as callPinnedExecutionResource } from '@/lib/execution-resource-call'
import type { GatewayClientReplyHandle, VerifiedClientAuthorization, VerifiedEnrollmentOffer } from '../../../generated/vibeface_secure.js'
import { downloadPackage } from '@/lib/secure'
import { createAccount, openAccountPackage, recoverAccount, restoreAccountRecovery, sealAccountPackage } from '@/lib/account'
import { finishAccountDeviceChange, finishFirstAuthenticatorRegistration, finishSpaceGovernanceChange, gatewayRouteApprovalHash, finishExecutionServiceRegistration, selectExecutionServiceManagement, revalidateClientPolicyAuthorization, finishRemoteExecutionShare, verifyExecutionServiceSelection, finishTeamCreation, Identity, recoveryOpen, recoverySeal, deriveAuthorizationId, nodeFingerprint, verifySignedEvent, verifyAccountEnrollment, verifyAccountEnrollmentFromOffer, verifyDeviceEnrollmentIntentForManager, verifyDeviceEnrollmentOffer, verifyPasskeyEnrollmentRequestFromOffer } from '../../../generated/vibeface_secure.js'
import { enrollmentHref, parseEnrollmentReference } from '@/lib/enrollment-reference'
import type { EnrollmentReference } from '@/lib/enrollment-reference'
import { checkEnrollmentOffer, parseEnrollmentRequest, parseSavedEnrollmentRequest, requestDeviceEnrollment, verifyEnrollmentStatus } from '@/lib/enrollment'
import type { EnrollmentRequest, EnrollmentStatus } from '@/lib/enrollment'
import { assertNodePasskey } from '@/lib/node-passkey'
import { registerNodePasskey } from '@/lib/node-passkey-registration'
import { prepareNodeEnrollment } from '@/lib/node-enrollment'
import { prepareCrossNodeRecovery, restoreCrossNodeRecovery } from '@/lib/account-cross-node'
import type { CrossNodeRecoveryResult, CrossNodeRecoveryReview } from '@/lib/account-cross-node'
import type { TeamControlsProps } from './team-controls'
import { parseRemoteExecutionShare, parseRemoteExecutionShareInput, parseRemoteExecutionSourceTrust, remoteExecutionProposal } from '@/lib/remote-execution'
import type { RemoteExecutionRequestSummary } from '@/lib/remote-execution'

type Phase = 'initializing' | 'blocked' | 'vault_empty' | 'vault_locked' | 'vault_unavailable' | 'key_lost' | 'disconnected' | 'connecting' | 'unpaired' | 'no_project' | 'ready'
type Review = { kind: 'bootstrap' | 'recovery'; accountId: string; spaceId: string; serviceGrant: { projectId: string; actions: string[] }; targetChanged: boolean; sourceRead?: CrossNodeRecoveryReview['sourceRead']; recoveryStage?: 'prepared' | 'submitted' | 'committed' | 'installed' }
type Sensitive = { cancel(): void; download?: () => void; bootstrap?: (saved: boolean) => Promise<void>; recover?: (saved: boolean) => Promise<void>; refreshEnrollment?: () => Promise<void>; passkeyEnrollment?: (protectLocal: boolean) => Promise<void>; retryPasskeyRegistration?: () => Promise<void>; nodeEnrollment?: () => Promise<void>; governance?: () => Promise<void>; teamCreation?: (saved: boolean) => Promise<void>; keepOnNetworkClose?: () => boolean; retainForReconnect?: (entry: ConnectionEntry) => boolean; crossPrepare?: () => Promise<void> }
type RecoveryReadProject = { accountId: string; spaceId: string; projectId: string; entry: ConnectionEntry; nodePublicKey: string; expiresAt: number }
type RecoveryInput = { file: File; password: string; connectionCard: string; targetConnectionCard?: string; sourceReadProjectIds?: string[]; sourceReadExpiresAt?: number }
type IncomingEnrollment = { accountId: string; newDevicePublicKey: string; requestedRights: string[]; status: EnrollmentStatus }
type ManagerEnrollment = { offer: string; reference: EnrollmentReference; published: boolean }
type BusinessRoute = { kind: 'direct' } | {
  kind: 'gateway'; channel: DeviceChannel | null; card: GatewayConnectionCard; approvalHash: string; sourceRef: string;
  generation: number; status: 'connecting' | 'ready' | 'unavailable'; error: string
}
type Session = {
  renewal?: number; authorization?: ReturnType<typeof verifyScopedAuthentication>; channel: DeviceChannel; entry: ConnectionEntry; info: EntryInfo; generation: number;
  businessRoute: BusinessRoute; businessGeneration: number; policyFrames: ClientPolicyFrame[]; connection?: ReadyConnection
}
type GovernanceConfirmation = { trust: AuthorizationTrust; authorized: boolean; grantLeaf?: string }
type GovernanceProtocol = {
  method: string
  params: Record<string, unknown>
  verify(committed: Record<string, unknown>): GovernanceConfirmation
  afterSaved(confirmed: GovernanceConfirmation, active: Session, signal: AbortSignal): void | Promise<void>
  dispose(): void
}
type ExecutionServiceContext = {
  spaceId: string; sourceSpaceId: string; sourceProjectId: string; nodePublicKey: string; resourceId: string
  trust: AuthorizationTrust; authorization: VerifiedClientAuthorization; grantLeaf: string; own: PublicIdentity
}
type ExecutionServiceBackup = { bytes: Uint8Array; accountId: string; own: PublicIdentity }
function releaseSession(active: Session | null) {
  if (active?.renewal !== undefined) window.clearTimeout(active.renewal)
  active?.authorization?.release()
}
type DeviceContextValue = {
  identity: PublicIdentity | null; vaults: VaultProfile[]; spaces: AuthorizationTrust[]; ready: ReadyConnection | null; entries: ConnectionEntry[]; phase: Phase; error: string
  entry: ConnectionEntry | null; info: EntryInfo | null; directory: AccountDeviceDirectory | null; review: Review | null; initialInvitation: string
  connect(entry: ConnectionEntry, spaceId?: string, projectId?: string): Promise<void>; selectSpace(spaceId: string): Promise<void>; selectProject(projectId: string): Promise<void>; disconnect(): void; saveEntry(entry: ConnectionEntry): void; removeEntry(entry: ConnectionEntry): void
  initializeVault(options: ClientVaultOptions): Promise<void>; lockVault(): void; backupVault(): Promise<void>; useConnectionCard(content: string, password?: string): Promise<void>
  connectGatewayCard(content: string): Promise<void>; disconnectGateway(): void
  exportGatewayRequest(): Promise<string>; approveGatewayRoute(input: GatewayOwnerInput): Promise<void>; retryGatewayRoute(): Promise<void>; ownerGatewayRoute: GatewayOwnerRoute | null
  refreshDirectory(): Promise<void>
  createEnrollmentOffer(manageDevices: boolean, route?: ConnectionEntry): Promise<string>
  incomingEnrollment: IncomingEnrollment | null; enrollmentRequests: EnrollmentRequest[]; managerEnrollmentActive: boolean; activeEnrollmentLink: string
  refreshIncomingEnrollment(): Promise<void>; refreshEnrollmentRequests(): Promise<EnrollmentRequest[]>; approveEnrollmentRequest(requestId: string): Promise<void>; completeEnrollmentRequest(requestId: string): Promise<void>; revokeDevice(deviceId: string): Promise<void>
  joinWithPasskey(protectLocal: boolean): Promise<void>
  localProtectionNotice: string
  restoreTeamCreation(input: { file: File; password: string }): Promise<void>
  createTeam: TeamControlsProps['createTeam']; changeMembership: TeamControlsProps['changeMembership']; grantProject: TeamControlsProps['grant']; revokeProjectGrant: TeamControlsProps['revokeGrant']
  governancePending: string; retryGovernance(): Promise<void>
  registerExecutionService: TeamControlsProps['registerExecutionService']; latestExecutionServiceResource: string
  createRemoteExecutionShare: TeamControlsProps['createRemoteExecutionShare']; latestRemoteExecutionShare: string
  inspectRemoteExecutionRequest: TeamControlsProps['inspectRemoteExecutionRequest']
  downloadExecutionServiceBackup(): void; executionServiceBackupAvailable: boolean
  executionServiceOptions: TeamControlsProps['executionServiceOptions']; refreshExecutionServices(): Promise<void>; selectExecutionService(resourceId: string): Promise<void>
  teamCreationReview: { accountId: string; teamId: string; sourceProjectId: string; nodePublicKey: string; submitted: boolean; committed: boolean } | null; confirmTeamCreation(saved: boolean): Promise<void>
  nodeEnrollmentReview: { accountId: string; nodePublicKey: string; address: string; submitted: boolean } | null
  beginNodeEnrollment(input: { file: File; password: string; connectionCard: string; certificateExpiresAt: number }): Promise<void>; confirmNodeEnrollment(): Promise<void>
  firstPasskeyPending: boolean; registerAccountPasskey(input: { file: File; password: string; protectLocal: boolean }): Promise<void>; resumePasskeyRegistration(): Promise<void>
  crossRecoveryPending: boolean; recoveryReadProjects: RecoveryReadProject[]; resumeCrossRecoveryPreparation(): Promise<void>; restorePendingCrossRecovery(input: { file: File; password: string }): Promise<void>
  beginRecovery(input: RecoveryInput): Promise<void>; restorePendingAccountRecovery(input: { file: File; password: string }): Promise<void>
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
function readRecoveryReadProjects(value: unknown): RecoveryReadProject[] {
  return array(value ?? []).map(value => {
    const input = record(value), accountId = text(input.accountId), spaceId = text(input.spaceId), projectId = text(input.projectId)
    const entry = parseEntry(input.entry), nodePublicKey = text(input.nodePublicKey), expiresAt = integer(input.expiresAt)
    if (!/^acc2_[a-f0-9]{64}$/.test(accountId) || !/^spc2_[a-f0-9]{64}$/.test(spaceId) || !/^res2_[a-f0-9]{64}$/.test(projectId)
      || nodeFingerprint(nodePublicKey) !== entry.node_id) throw new Error('已保存的原项目只读入口无效，请保留数据并核对。')
    return { accountId, spaceId, projectId, entry, nodePublicKey, expiresAt }
  })
}
function isResponseError(error: unknown) { return error instanceof Error && 'responded' in error && error.responded === true }

function executionManagement(authorization: VerifiedClientAuthorization, sourceProject: { space: string; resource: string }, targetPublicKey: string) {
  let cursor: string | null = null
  for (;;) {
    const page = selectExecutionServiceManagement(authorization, JSON.stringify(sourceProject), targetPublicKey, JSON.stringify(cursor), Date.now())
    try {
      const metadata = record(JSON.parse(page.metadataJson()))
      if (!same(metadata.source_project, sourceProject) || metadata.target_node_public_key !== targetPublicKey) throw new Error('执行资源管理选择改变了源项目或目标 Node。')
      const selection = page.selection()
      if (selection) return { selection, grant: text(metadata.management_grant) }
      if (metadata.next_cursor === null) throw new Error('当前项目没有可用于登记执行资源的管理授权。')
      const next = text(metadata.next_cursor)
      if (!/^h2_[a-f0-9]{64}$/.test(next) || cursor !== null && next <= cursor) throw new Error('执行资源管理选择分页无效。')
      cursor = next
    } finally { page.free() }
  }
}

export function DeviceProvider({ children }: { children: ReactNode }) {
  const [identity, setIdentity] = useState<PublicIdentity | null>(null)
  const [vaults, setVaults] = useState<VaultProfile[]>([])
  const [spaces, setSpaces] = useState<AuthorizationTrust[]>([])
  const [ready, setReady] = useState<ReadyConnection | null>(null)
  const [, setRouteRevision] = useState(0)
  const [ownerGatewayRoute, setOwnerGatewayRoute] = useState<GatewayOwnerRoute | null>(null)
  const policyFrames = useRef(new WeakMap<VerifiedClientAuthorization, ClientPolicyFrame[]>())
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
  const [governancePending, setGovernancePending] = useState('')
  const executionAuthorizations = useRef(new Set<VerifiedClientAuthorization>())
  const [executionServiceResource, setExecutionServiceResource] = useState<ExecutionServiceContext | null>(null)
  const [executionServiceResources, setExecutionServiceResources] = useState<ExecutionServiceContext[]>([])
  const [executionServiceBackups, setExecutionServiceBackups] = useState(new Map<string, ExecutionServiceBackup>())
  const [remoteExecutionShare, setRemoteExecutionShare] = useState<{ value: string; execution: ExecutionServiceContext } | null>(null)
  const [crossRecoveryPending, setCrossRecoveryPending] = useState(false)
  const [recoveryReadProjects, setRecoveryReadProjects] = useState<RecoveryReadProject[]>([])
  const [teamCreationReview, setTeamCreationReview] = useState<DeviceContextValue['teamCreationReview']>(null)
  const [nodeEnrollmentReview, setNodeEnrollmentReview] = useState<DeviceContextValue['nodeEnrollmentReview']>(null)
  const managerEnrollment = useRef<ManagerEnrollment | null>(null)
  const generation = useRef(0)
  const session = useRef<Session | null>(null)
  const connecting = useRef<AbortController | null>(null)
  const independentReads = useRef(new Set<AbortController>())
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
  const managerEnrollmentKey = (active: Session) => profileKey(`enrollment-offer.${active.entry.node_id}.${active.authorization?.scope.account_id}`)
  function savedManagerEnrollment(active: Session): ManagerEnrollment | null {
    if (!active.authorization?.enrollmentRights.includes('enroll_device')) return null
    const value = load(managerEnrollmentKey(active), true)
    if (!value) return null
    const input = record(value), rawReference = record(input.reference)
    if (integer(rawReference.expires_at_ms) <= Date.now()) return null
    const reference = parseEnrollmentReference(rawReference), offer = text(input.offer)
    checkEnrollmentOffer(offer, reference)
    if (reference.node_public_key !== active.channel.publicKey || reference.account_id !== active.authorization.scope.account_id
      || record(JSON.parse(offer)).issuer !== identityRef.current?.publicKey || typeof input.published !== 'boolean') throw new Error('已保存的邀请不属于当前管理设备、账户或节点。')
    return { offer, reference, published: input.published }
  }

  const cancelSensitive = useCallback(() => { sensitive.current?.cancel(); sensitive.current = null; setReview(null); setIncomingEnrollment(null); setFirstPasskeyPending(false); setNodeEnrollmentReview(null); setGovernancePending(''); setTeamCreationReview(null); setCrossRecoveryPending(false) }, [])
  const closeConnection = useCallback((preserveSensitive: boolean) => {
    generation.current += 1
    if (!preserveSensitive) cancelSensitive()
    connecting.current?.abort(); connecting.current = null
    for (const controller of independentReads.current) controller.abort()
    independentReads.current.clear()
    const previous = session.current; session.current = null
    releaseSession(previous); if (previous?.businessRoute.kind === 'gateway') previous.businessRoute.channel?.close(); previous?.channel.close()
    listeners.current.clear(); setReady(null); setDirectory(null); setInfo(null); setEntry(null); setOwnerGatewayRoute(null)
    managerEnrollment.current = null; setEnrollmentRequests([]); setManagerEnrollmentActive(false)
    for (const authorization of executionAuthorizations.current) authorization.free()
    executionAuthorizations.current.clear(); setExecutionServiceResource(null); setExecutionServiceResources([]); setRemoteExecutionShare(null)
    setLocalProtectionNotice('')
    setPhase('disconnected'); setError('')
  }, [cancelSensitive])
  const disconnect = useCallback(() => closeConnection(false), [closeConnection])
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
  function resumableAuthority(active: Session, own: PublicIdentity, available: () => boolean, extraSpaces: string[] = []) {
    const sourceNode = active.entry.node_id, sourceKey = active.channel.publicKey, scope = active.authorization?.scope
    const originalProfile = () => identityRef.current?.publicKey === own.publicKey && identityRef.current.profileId === own.profileId
    return {
      keepOnNetworkClose: available,
      retainForReconnect: (entry: ConnectionEntry) => available() && originalProfile() && entry.node_id === sourceNode,
      resume() {
        const resumed = current(), candidate = resumed.authorization?.scope
        if (!originalProfile() || resumed.entry.node_id !== sourceNode || resumed.channel.publicKey !== sourceKey
          || !scope || candidate?.account_id !== scope.account_id || candidate.space_id !== scope.space_id && !extraSpaces.includes(candidate.space_id)) {
          throw new Error('请用原设备配置重新连接原授权 Node 和空间，再核对原请求。')
        }
        return resumed
      },
    }
  }
  async function persistPolicyTrust(active: Session | null, trust: AuthorizationTrust, signal?: AbortSignal, own = identityRef.current) {
    if (!own) throw changed()
    const profile = storageProfile(own), key = `profile.${profile}.policy-trusts`, temporary = own.profileId === null
    if (!('locks' in navigator)) throw new Error('需要 Web Locks 才能保存已提交的账户政策。请保留当前页面并核对设备权限。')
    const updated = await navigator.locks.request(`profile.${profile}.policy-trust-writer`, () => {
      signal?.throwIfAborted()
      if (temporary && (active ? session.current !== active : identityRef.current !== own)) throw changed()
      const known = array((temporary ? temporaryRecords.current.get(key) : readPersistent(key, true)) ?? []).map(parseAuthorizationTrust)
      const updated = mergedRemotePolicyTrust(mergeAuthorizationTrust(trust, known), own)
      const next = [...known.filter(item => item.spaceId !== updated.spaceId).map(item => mergeAuthorizationTrust(item, [updated])), updated]
      if (temporary) temporaryRecords.current.set(key, JSON.parse(JSON.stringify(next)))
      else writePersistent(key, next)
      if (identityRef.current === own && (active === null || session.current === active)) setSpaces(next)
      return updated
    })
    if (identityRef.current !== own || active !== null && session.current !== active || signal?.aborted) throw changed()
    return updated
  }
  async function withPolicyTrust<T,>(own: PublicIdentity, work: () => T): Promise<T> {
    if (!('locks' in navigator)) throw new Error('此浏览器需要 Web Locks 才能使用已保存的账户政策。')
    return navigator.locks.request(`profile.${storageProfile(own)}.policy-trust-writer`, () => {
      if (identityRef.current !== own) throw changed()
      return work()
    })
  }
  function withSessionAuthorization<T>(active: Session, own: PublicIdentity, work: (authorization: VerifiedClientAuthorization) => T, signal?: AbortSignal): Promise<T> {
    return withPolicyTrust(own, () => {
      signal?.throwIfAborted()
      const authorization = active.authorization
      if (session.current !== active || identityRef.current !== own || !authorization) throw changed()
      authorization.revalidate(mergedRemotePolicyTrust(mergeAuthorizationTrust(authorization.trust, savedSpaces()), own))
      return authorization.withNative(work)
    })
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
  function ownerProjectSelection(bundle: unknown, projectId: string, nodePublicKey: string) {
    const grantLeaf = text(record(bundle).grant_leaf)
    return { ...parseAuthorizationSelection({ resourceId: projectId, audience: { device: deriveAuthorizationId('device', nodePublicKey), service: deriveAuthorizationId('service', nodePublicKey) }, grantLeaf }), ownerGrantLeaf: grantLeaf }
  }
  function gatewayKey(connection: ReadyConnection) {
    return profileKey(`gateway.${connection.scope.space_id}.${connection.project.id}.${connection.audiencePublicKey}`)
  }
  async function attachGateway(card: GatewayConnectionCard, active: Session, connection: ReadyConnection, persist: boolean) {
    const own = identityRef.current, payload = record(card.approval.payload), resource = record(payload.resource)
    const approvalHash = gatewayRouteApprovalHash(JSON.stringify(card.approval)), routeGeneration = integer(payload.generation)
    const route: Extract<BusinessRoute, { kind: 'gateway' }> = { kind: 'gateway', channel: null, card, approvalHash, sourceRef: '',
      generation: routeGeneration, status: 'connecting', error: '' }
    const previous = active.businessRoute
    active.businessRoute = route; active.businessGeneration += 1
    if (previous.kind === 'gateway') previous.channel?.close()
    const generation = active.businessGeneration
    const check = () => {
      if (session.current !== active || active.connection !== connection || identityRef.current !== own
        || active.businessRoute !== route || active.businessGeneration !== generation) throw changed()
    }
    setRouteRevision(value => value + 1)
    let opened: DeviceChannel | null = null
    try {
      if (!own || !connection.isCurrent() || payload.version !== 2 || payload.state !== 'active'
        || payload.gateway !== card.gateway.nodePublicKey || payload.client_key !== own.publicKey || payload.client_account !== connection.scope.account_id
        || payload.target_key !== connection.audiencePublicKey || resource.space !== connection.scope.space_id || resource.resource !== connection.project.id
        || nodeFingerprint(card.gateway.nodePublicKey) !== card.gateway.entry.node_id || integer(payload.issued_at_ms) > Date.now() || integer(payload.expires_at_ms) <= Date.now()) {
        throw new Error('网关路由与当前浏览器、项目、目标 Node 或有效期不同。')
      }
      const saved = load(gatewayKey(connection), true)
      if (saved) {
        const previous = record(saved), previousCard = parseGatewayConnectionCard(previous.card), previousPayload = record(previousCard.approval.payload)
        if (previousPayload.route_id === payload.route_id && (integer(previousPayload.generation) > routeGeneration
          || previousPayload.generation === routeGeneration && previous.approvalHash !== approvalHash)) throw new Error('网关路由版本回退或同版本批准内容发生了变化。')
      }
      if (persist) store(gatewayKey(connection), { card: { version: 2, kind: 'vibeface-gateway', gateway: {
        url: card.gateway.entry.address, node_id: card.gateway.entry.node_id, node_public_key: card.gateway.nodePublicKey, relay: card.gateway.entry.relay,
      }, approval: card.approval }, approvalHash })
      opened = await connectDevice(card.gateway.entry, { onClose(reason) {
        if (active.businessRoute !== route || session.current !== active) return
        route.channel = null; route.status = 'unavailable'; route.error = messageOf(reason)
        active.businessGeneration += 1; setRouteRevision(value => value + 1)
      } })
      check()
      if (opened.publicKey !== card.gateway.nodePublicKey) throw new Error('网关的实际加密公钥与连接卡不同。')
      route.channel = opened
      const send = async (params: Record<string, unknown>) => {
        check()
        if (!opened) throw changed()
        const result = record(await opened.request('gateway_source', params))
        check()
        return result
      }
      const committed = await send({ stage: 'route', approval: card.approval })
      if (committed.approval !== approvalHash || committed.route_id !== payload.route_id || committed.generation !== routeGeneration) throw new Error('网关安装的路由与本地固定的批准不同。')
      const totalPages = active.policyFrames.filter(frame => frame.kind === 'page').length
      let reference = '', pages = 0, remotePages = 0, remoteComplete = false, finished = false
      for (const frame of active.policyFrames) {
        let response: Record<string, unknown>
        if (frame.kind === 'begin') {
          if (reference) throw new Error('项目授权原件包含重复起点。')
          response = await send({ stage: 'begin', route_id: payload.route_id, request: frame.request, snapshot: frame.snapshot })
          reference = text(response.reference)
          if (!/^h2_[a-f0-9]{64}$/.test(reference)) throw new Error('网关返回了无效的项目授权引用。')
        } else if (frame.kind === 'page') {
          if (!reference || finished) throw new Error('项目授权原件顺序无效。')
          response = await send({ stage: 'page', reference, page: frame.page }); pages += 1
        } else {
          if (!reference || finished || pages === 0) throw new Error('项目授权原件缺少完整分页。')
          response = await send({ stage: 'finish', reference, witness: frame.witness, confirm_request: frame.confirm_request, confirmation: frame.confirmation })
          finished = true
        }
        if (response.reference !== reference || typeof response.pages !== 'number' || !Number.isSafeInteger(response.pages)
          || response.pages < pages || response.pages < remotePages || response.pages > totalPages || typeof response.complete !== 'boolean'
          || response.complete && response.pages !== totalPages || remoteComplete && !response.complete || finished && !response.complete) {
          throw new Error('网关导入的项目授权进度与原件不同。')
        }
        remotePages = response.pages; remoteComplete = response.complete
      }
      if (!finished) throw new Error('没有可上传的完整项目授权，请重新连接项目后再启用网关。')
      check(); route.sourceRef = reference; route.status = 'ready'; setError(''); setRouteRevision(value => value + 1)
    } catch (cause) {
      opened?.close()
      if (active.businessRoute === route && session.current === active) {
        route.channel = null; route.status = 'unavailable'; route.error = messageOf(cause); setRouteRevision(value => value + 1)
      }
      throw cause
    }
  }
  async function connectGatewayCard(content: string) {
    const active = current(), connection = active.connection, authorization = active.authorization
    const stored = connection ? load(gatewayKey(connection), true) : null
    const card = parseGatewayConnectionCard(content.trim() ? readInvitation(content) : record(stored).card)
    if (!connection?.isCurrent() || !authorization) throw changed()
    let reuseSource = false
    const own = identityRef.current
    if (!content.trim() && own && active.policyFrames.length) {
      try { await withSessionAuthorization(active, own, () => { reuseSource = true }) }
      catch { if (!connection.isCurrent()) throw changed() }
    }
    if (!reuseSource) {
      const selection = { resourceId: connection.project.id, audience: connection.audience, grantLeaf: authorization.grantLeaf }
      const fresh = await readAuthentication(active, authorization.trust, selection)
      await authenticate(fresh, active, authorization.trust, selection.resourceId, selection, undefined, false)
    }
    if (!active.connection) throw changed()
    await attachGateway(card, active, active.connection, true)
  }
  function disconnectGateway() {
    const active = current(), connection = active.connection, previous = active.businessRoute
    if (!connection?.isCurrent()) throw changed()
    store(gatewayKey(connection), null)
    active.businessRoute = { kind: 'direct' }; active.businessGeneration += 1
    if (previous.kind === 'gateway') previous.channel?.close()
    setError(''); setRouteRevision(value => value + 1)
  }
  const ownerGatewayKey = (publicKey: string) => profileKey(`gateway-owner.${publicKey}`)
  async function exportGatewayRequest() {
    const active = current(), connection = active.connection, own = identityRef.current
    if (!connection?.isCurrent() || !own) throw changed()
    return withSessionAuthorization(active, own, () => {
      const authorization = active.authorization
      if (!authorization || !connection.isCurrent()) throw changed()
      return JSON.stringify(parseGatewayRouteRequest({ version: 2, kind: 'vibeface-gateway-request',
        client_account: connection.scope.account_id, client_key: own.publicKey, target_key: connection.audiencePublicKey,
        resource: { space: connection.scope.space_id, resource: connection.project.id },
        allowed_actions: authorization.actions, source_pins: authorization.trust.pins }))
    })
  }
  async function installOwnerGatewayRoute(value: GatewayOwnerRoute, active: Session, connection: ReadyConnection) {
    const own = identityRef.current, card = parseGatewayConnectionCard(JSON.parse(value.card)), payload = record(card.approval.payload)
    if (!own) throw changed()
    const hash = gatewayRouteApprovalHash(JSON.stringify(card.approval))
    const check = () => {
      if (!own || session.current !== active || active.connection !== connection || !connection.isCurrent() || identityRef.current !== own) throw changed()
    }
    check()
    if (connection.authorization.isTeam || card.gateway.nodePublicKey !== connection.audiencePublicKey
      || payload.gateway !== connection.audiencePublicKey || payload.state !== 'active' && payload.state !== 'revoked') throw new Error('请在网关所属账户的个人空间核对此路由。')
    const controller = new AbortController(); independentReads.current.add(controller)
    let channel: DeviceChannel | null = null
    try {
      channel = await connectDevice(card.gateway.entry, { signal: controller.signal })
      check()
      if (channel.publicKey !== connection.audiencePublicKey) throw new Error('实际网关公钥与当前个人项目的 Node 不同。')
      const observed = parseGatewayRouteStatus(await channel.request('gateway_source', { stage: 'route_status', approval: card.approval }), card.approval)
      check()
      let status: GatewayOwnerRoute['status']
      if (observed.status === 'installed') status = 'installed'
      else if (observed.status === 'superseded') status = 'superseded'
      else if (observed.checkedAt >= integer(payload.expires_at_ms)) status = 'expired'
      else {
        const committed = record(await channel.request('gateway_source', { stage: 'route', approval: card.approval }))
        check()
        if (committed.approval !== hash || committed.route_id !== payload.route_id || committed.generation !== payload.generation) throw new Error('网关返回的路由与本地原签名不同，结果仍待核对。')
        status = 'installed'
      }
      const result: GatewayOwnerRoute = { card: value.card, status }
      await withPolicyTrust(own, () => {
        check()
        const pending = parseGatewayOwnerRoute(load(ownerGatewayKey(connection.audiencePublicKey), true), connection.audiencePublicKey)
        if (pending.card !== value.card) throw new Error('当前保存的网关路由已变化，请保留并核对新记录。')
        store(`${ownerGatewayKey(connection.audiencePublicKey)}.record.${hash}`, result)
        store(ownerGatewayKey(connection.audiencePublicKey), result); setOwnerGatewayRoute(result)
      })
    } finally { channel?.close(); independentReads.current.delete(controller) }
  }
  async function approveGatewayRoute(input: GatewayOwnerInput) {
    const active = current(), connection = active.connection, own = identityRef.current
    if (!connection?.isCurrent() || !own || connection.authorization.isTeam || active.channel.publicKey !== connection.audiencePublicKey) throw new Error('请先连接网关所属账户的个人项目。')
    if (sensitive.current) throw new Error('请先完成或关闭当前账户管理操作。')
    const pending = load(ownerGatewayKey(connection.audiencePublicKey), true)
    if (pending && parseGatewayOwnerRoute(pending, connection.audiencePublicKey).status === 'pending') throw new Error('存在结果待核对的原路由，请先核对原提交。')
    if (!Number.isSafeInteger(input.expiresAt) || input.expiresAt <= Date.now()) throw new Error('请选择晚于当前时刻的路由有效期。')
    let payload: Record<string, unknown>
    if (input.kind === 'approve') {
      const request = parseGatewayRouteRequest(readInvitation(input.request)), nonce = crypto.getRandomValues(new Uint8Array(32))
      if (request.client_key === connection.audiencePublicKey || request.target_key === connection.audiencePublicKey) throw new Error('网关必须与申请人设备和项目 Node 使用不同公钥。')
      payload = { version: 2, route_id: [...nonce].map(byte => byte.toString(16).padStart(2, '0')).join(''), generation: 1,
        gateway: connection.audiencePublicKey, client_account: request.client_account, client_key: request.client_key,
        target_key: request.target_key, resource: request.resource, allowed_actions: request.allowed_actions,
        source_pins: request.source_pins, state: 'active', issued_at_ms: Date.now(), expires_at_ms: input.expiresAt }
    } else {
      const card = parseGatewayConnectionCard(readInvitation(input.card)), previous = record(card.approval.payload)
      gatewayRouteApprovalHash(JSON.stringify(card.approval)); gatewayRequestFromCard(card)
      const generation = integer(previous.generation)
      if (card.gateway.nodePublicKey !== connection.audiencePublicKey || previous.gateway !== connection.audiencePublicKey || previous.state !== 'active' || !Number.isSafeInteger(generation + 1)) throw new Error('请选择当前网关的一份有效原路由。')
      payload = { ...previous, generation: generation + 1, state: 'revoked', issued_at_ms: Date.now(), expires_at_ms: input.expiresAt }
    }
    const controller = new AbortController()
    let opened: Awaited<ReturnType<typeof openAccountPackage>> | null = null
    const reservation: Sensitive = { cancel() { controller.abort(); opened?.release(); opened = null; input.password = '' } }
    const check = () => { controller.signal.throwIfAborted(); if (sensitive.current !== reservation || session.current !== active || active.connection !== connection || identityRef.current !== own || !connection.isCurrent()) throw changed() }
    sensitive.current = reservation
    let signedCard = ''
    try {
      opened = await openAccountPackage(input.file, input.password); input.password = ''; check()
      if (opened.scope.account_id !== connection.scope.account_id || opened.scope.space_id !== connection.scope.space_id) throw new Error('恢复包必须属于当前网关账户的个人空间。')
      const material = opened
      signedCard = await withSessionAuthorization(active, own, handle => {
        check()
        const previous = load(ownerGatewayKey(connection.audiencePublicKey), true)
        if (previous && parseGatewayOwnerRoute(previous, connection.audiencePublicKey).status === 'pending') throw new Error('另一页面已保存待核对的原路由，请先核对该记录。')
        const signed = prepareClientGatewayOwnerRoute(material.material, handle, JSON.stringify(payload))
        const approval = record(JSON.parse(signed))
        const hash = gatewayRouteApprovalHash(signed)
        if (!same(approval.payload, payload)) throw new Error('本地签名返回了不同的路由。')
        const card = gatewayCardJson(active.entry, connection.audiencePublicKey, approval)
        const retained: GatewayOwnerRoute = { card, status: 'pending' }
        store(`${ownerGatewayKey(connection.audiencePublicKey)}.record.${hash}`, retained)
        store(ownerGatewayKey(connection.audiencePublicKey), retained); setOwnerGatewayRoute(retained)
        return card
      }, controller.signal)
      check()
    } finally {
      opened?.release(); opened = null; input.password = ''
      if (sensitive.current === reservation) sensitive.current = null
    }
    await installOwnerGatewayRoute({ card: signedCard, status: 'pending' }, active, connection)
  }
  async function retryGatewayRoute() {
    const active = current(), connection = active.connection
    if (!connection?.isCurrent()) throw changed()
    const retained = parseGatewayOwnerRoute(load(ownerGatewayKey(connection.audiencePublicKey), true), connection.audiencePublicKey)
    await installOwnerGatewayRoute(retained, active, connection)
  }
  async function readAuthentication(active: Session, trust: AuthorizationTrust, selection?: AuthorizationSelection, signal?: AbortSignal, projectId = selection?.resourceId) {
    const own = identityRef.current
    if (!own) throw changed()
    const currentTrust = mergedRemotePolicyTrust(mergeAuthorizationTrust(trust, savedSpaces()), own)
    const frames: ClientPolicyFrame[] = []
    const authorization = await readClientPolicyAuthorization({ call: (method, params) => callSession(active, 'identity', { method, params }), trust: currentTrust,
      gatewayPublicKey: active.channel.publicKey, selection, resourceId: projectId, signal, onFrame: frame => frames.push(frame),
      check() { if (session.current !== active || identityRef.current !== own) throw changed() } })
    policyFrames.current.set(authorization, frames)
    return authorization
  }
  async function authenticate(authValue: VerifiedClientAuthorization, active: Session, expected: AuthorizationTrust | null = null, projectId?: string, selection?: AuthorizationSelection, signal?: AbortSignal, restoreGateway = true) {
    const cancel = () => { if (session.current === active) active.channel.close() }
    signal?.addEventListener('abort', cancel, { once: true })
    try { await authenticateSession(authValue, active, expected, projectId, selection, signal, restoreGateway) }
    finally { signal?.removeEventListener('abort', cancel) }
  }
  async function authenticateSession(authValue: VerifiedClientAuthorization, active: Session, expected: AuthorizationTrust | null, projectId?: string, selection?: AuthorizationSelection, signal?: AbortSignal, restoreGateway = true) {
    const frames = policyFrames.current.get(authValue) ?? []
    const request = scoped(active)
    const own = identityRef.current
    if (!own) { authValue.free(); throw new Error('浏览器密钥尚未初始化。') }
    const signingProfile: PublicIdentity = own
    const ownPublicKey = own.publicKey
    const authorization = await (async () => {
      let incoming: VerifiedClientAuthorization | null = authValue
      let verified: ReturnType<typeof verifyScopedAuthentication> | null = null
      try {
        signal?.throwIfAborted()
        if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能安全保存账户政策。请更换安全浏览器入口。')
        const check = () => { signal?.throwIfAborted(); if (session.current !== active || identityRef.current !== own) throw changed() }
        const latestTrust = () => {
          const known = savedSpaces(), selectedSpace = load(profileKey('last-space'))
          const candidate = expected ?? known.find(trust => trust.spaceId === selectedSpace) ?? known[0]
          return mergedRemotePolicyTrust(mergeAuthorizationTrust(parseAuthorizationTrust(candidate), known), own)
        }
        verified = await withPolicyTrust(own, () => {
          check()
          const trust = latestTrust(), adopted = incoming
          if (!adopted) throw changed()
          incoming = null
          return verifyScopedAuthentication(adopted, own, trust, active.channel.publicKey, selection)
        })
        const response = record(await request('authenticate', verified.clientPolicyContext()))
        check()
        if (!same(response.scope, verified.scope) || !same(response.policy, verified.policy) || !same(response.resources, verified.policy.resources)) throw new Error('Node 的会话范围与本地已确认的有限授权不同。')
        const confirmed = verified
        await withPolicyTrust(own, () => {
          check(); confirmed.revalidate(latestTrust())
          const known = savedSpaces(), others = known.filter(trust => trust.spaceId !== confirmed.trust.spaceId)
          const next = [...others.map(trust => mergeAuthorizationTrust(trust, [confirmed.trust])), confirmed.trust]
          store(profileKey('policy-trusts'), next); store(profileKey('last-space'), confirmed.scope.space_id); setSpaces(next)
        })
        verified = null
        return confirmed
      } finally { incoming?.free(); verified?.release() }
    })()
    if (session.current !== active || signal?.aborted) { authorization.release(); throw changed() }
    releaseSession(active); active.authorization = authorization; active.policyFrames = frames
    active.businessGeneration += 1
    if (active.businessRoute.kind === 'gateway') { active.businessRoute.channel?.close(); active.businessRoute.channel = null; active.businessRoute.status = 'unavailable'; active.businessRoute.error = '项目授权已刷新，正在重新连接网关。' }
    managerEnrollment.current = savedManagerEnrollment(active)
    setManagerEnrollmentActive(managerEnrollment.current?.published === true)
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
    const target = targetKey(authorization.scope.space_id, active.entry), previousTarget = load(target)
    const ownerGrantLeaf = previousTarget ? record(previousTarget).ownerGrantLeaf : undefined
    store(target, { resourceId: project.id, audience: authorization.audience, grantLeaf: authorization.grantLeaf, ...(ownerGrantLeaf === undefined ? {} : { ownerGrantLeaf }) })
    const selectedProjectId = project.id
    function currentProjectTrust() {
      return authorization.revalidate(mergedRemotePolicyTrust(mergeAuthorizationTrust(authorization.trust, savedSpaces()), signingProfile))
    }
    async function verifyRemotePolicy(value: Parameters<ReturnType<typeof verifyScopedAuthentication>['remote']>[0], context: Record<string, unknown>, action: string, purpose?: 'execution' | 'observation_read' | 'resolver', use?: RemoteExecutionPolicyUse) {
      const controller = new AbortController(); independentReads.current.add(controller)
      const check = () => { controller.signal.throwIfAborted(); if (session.current !== active || active.authorization !== authorization || identityRef.current !== signingProfile) throw changed() }
      let bChannel: DeviceChannel | null = null, cChannel: DeviceChannel | null = null
      let policy: Awaited<ReturnType<typeof readRemoteExecutionPolicy>> | null = null
      try {
        check()
        const proposed = remoteExecutionProposal(value)
        if (proposed.purpose === 'rebind') throw new Error('原 C 封存使用独立 Account Rebind 授权。')
        const remote = { ...value, trust: mergedRemotePolicyTrust(value.trust, signingProfile) }, sourceNodePublicKey = text(authorization.policy.audience_public_key)
        bChannel = active.channel.publicKey === sourceNodePublicKey ? active.channel : await connectDevice({ address: active.entry.address, node_id: nodeFingerprint(sourceNodePublicKey), relay: true }, { signal: controller.signal })
        check()
        const target = remote.share.selection.target_public_key, stored = savedEntries(storageProfile(signingProfile), load).find(entry => entry.node_id === nodeFingerprint(target))
        cChannel = await connectDevice(stored ?? { address: active.entry.address, node_id: nodeFingerprint(target), relay: true }, { signal: controller.signal })
        check()
        const invocation = record(context.context), executionSession = invocation.execution_session === null ? null : record(invocation.execution_session)
        if (invocation.sdk_session !== null) throw new Error('远端任务申请不能带 SDK 私有会话。')
        const preparedContext = { context: { conversation: invocation.conversation === null ? null : text(invocation.conversation), task: invocation.task === null ? null : text(invocation.task), sdk_session: null,
          execution_session: executionSession === null ? null : { target: text(executionSession.target), session_id: text(executionSession.session_id) }, executor_kind: invocation.executor_kind === null ? null : text(invocation.executor_kind), workspace_resource: text(invocation.workspace_resource) }, binding_generation: context.binding_generation === null ? null : integer(context.binding_generation),
          canonical_cwd: context.canonical_cwd === null ? null : text(context.canonical_cwd), execution_target_public_key: target }
        policy = await readRemoteExecutionPolicy({ bChannel, cChannel, sourceNodePublicKey, executionNodePublicKey: target,
          withSource: work => withSessionAuthorization(active, signingProfile, work, controller.signal), selection: remote.share.selection, context: preparedContext,
          action, purpose: purpose ?? (proposed.purpose === 'resolution' ? 'resolver' : 'execution'), use, executionTrust: remote.trust,
          latestExecutionTrust: () => mergedRemotePolicyTrust(remote.trust, signingProfile), check, signal: controller.signal })
        const completed = policy
        return await withSessionAuthorization(active, signingProfile, () => {
          check()
          return authorization.remote({ ...remote, trust: mergedRemotePolicyTrust(remote.trust, signingProfile) }, completed)
        }, controller.signal)
      } finally { policy?.free(); if (bChannel !== active.channel) bChannel?.close(); cChannel?.close(); independentReads.current.delete(controller) }
    }
    async function verifyRemoteHistory<T>(value: Parameters<ReturnType<typeof verifyScopedAuthentication>['remote']>[0], actual: Record<string, unknown>, original: AuthorizedRequest, binding: Record<string, unknown> | undefined,
      verify: (remote: typeof value, policy: Awaited<ReturnType<typeof readRemoteExecutionHistoryPolicy>>) => T): Promise<T> {
      const controller = new AbortController(); independentReads.current.add(controller)
      const check = () => { controller.signal.throwIfAborted(); if (session.current !== active || active.authorization !== authorization || identityRef.current !== signingProfile) throw changed() }
      const latestPins = () => {
        check()
        const source = mergedRemotePolicyTrust(mergeAuthorizationTrust(authorization.trust, savedSpaces()), signingProfile)
        const execution = mergedRemotePolicyTrust(value.trust, signingProfile, true)
        const pins = new Map<string, AuthorizationTrust['pins'][number]>()
        for (const pin of [...source.pins, ...execution.pins]) {
          const key = `${pin.domain.kind}:${pin.domain.id}`, previous = pins.get(key)
          if (previous && (previous.genesis_root !== pin.genesis_root || previous.minimum_head && pin.minimum_head && !same(previous.minimum_head, pin.minimum_head))) throw new Error('来源与执行端的已保存政策水位不同，请先同步并核对完整签名历史。')
          pins.set(key, { ...pin, minimum_head: pin.minimum_head ?? previous?.minimum_head ?? null })
        }
        return [...pins.entries()].sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0).map(([, pin]) => pin)
      }
      let bChannel: DeviceChannel | null = null, cChannel: DeviceChannel | null = null
      let policy: Awaited<ReturnType<typeof readRemoteExecutionHistoryPolicy>> | null = null
      try {
        check(); remoteExecutionProposal(value)
        const pins = latestPins(), sourceNodePublicKey = text(authorization.policy.audience_public_key), target = value.share.selection.target_public_key
        bChannel = active.channel.publicKey === sourceNodePublicKey ? active.channel : await connectDevice({ address: active.entry.address, node_id: nodeFingerprint(sourceNodePublicKey), relay: true }, { signal: controller.signal })
        check()
        const stored = savedEntries(storageProfile(signingProfile), load).find(entry => entry.node_id === nodeFingerprint(target))
        cChannel = await connectDevice(stored ?? { address: active.entry.address, node_id: nodeFingerprint(target), relay: true }, { signal: controller.signal })
        check()
        policy = await readRemoteExecutionHistoryPolicy({ bChannel, cChannel, sourceNodePublicKey, executionNodePublicKey: target, actual, original, binding,
          withSource: work => withSessionAuthorization(active, signingProfile, work, controller.signal), independentPins: pins, latestIndependentPins: latestPins, check, signal: controller.signal })
        const completed = policy
        return await withSessionAuthorization(active, signingProfile, () => {
          check()
          return verify({ ...value, trust: { ...mergedRemotePolicyTrust(value.trust, signingProfile, true), pins: latestPins() } }, completed)
        }, controller.signal)
      } finally { policy?.free(); if (bChannel !== active.channel) bChannel?.close(); cChannel?.close(); independentReads.current.delete(controller) }
    }
    async function signedCall(method: string, params: Record<string, unknown> = {}, channelMethod = 'rpc', beforeSend?: (proof: AuthorizedRequest) => void, facts?: RpcFacts, replay?: AuthorizedRequest) {
      const route: BusinessRoute = channelMethod === 'rpc' ? active.businessRoute : { kind: 'direct' }
      const routeGeneration = active.businessGeneration
      const check = () => {
        if (session.current !== active || active.authorization !== authorization || identityRef.current !== signingProfile
          || channelMethod === 'rpc' && (active.businessGeneration !== routeGeneration || active.businessRoute !== route)) throw changed()
      }
      check()
      const presenter = route.kind === 'gateway' ? route.card.gateway.nodePublicKey : active.channel.publicKey
      if (route.kind === 'gateway' && (route.status !== 'ready' || !route.channel)) throw new Error(route.error || '网关尚未连接，请重新启用网关或明确恢复直接连接。')
      const forward = async (original: Record<string, unknown>) => {
        if (route.kind !== 'gateway' || !route.channel) throw changed()
        const channel = route.channel
        const handle: GatewayClientReplyHandle = await withPolicyTrust(signingProfile, () => {
          check(); currentProjectTrust()
          return authorization.withNative(value => prepareClientGatewayReply(value, JSON.stringify(original), route.approvalHash, presenter))
        })
        try {
          check()
          let rawReply: unknown
          try { rawReply = await channel.request('gateway_source', { stage: 'forward', reference: route.sourceRef, original }) }
          catch (cause) { throw new Error(messageOf(cause)) }
          const reply = record(rawReply)
          check()
          const outcome = record(JSON.parse(handle.verifyReply(JSON.stringify(reply.forward), JSON.stringify(reply.reply), JSON.stringify(reply.outcome), Date.now())))
          check()
          if (outcome.kind === 'ok') return outcome.value
          if (outcome.kind !== 'error') throw new Error('网关返回的已验证结果格式无效。')
          const failure = record(outcome.value)
          throw Object.assign(new Error(typeof failure.message === 'string' ? failure.message : '项目节点拒绝了此请求。'), { responded: true })
        } finally { handle.free() }
      }
      const invoke = (body: Record<string, unknown>, invocation: unknown) => route.kind === 'gateway'
        ? forward({ kind: body.method === 'operation' ? 'status' : 'invoke', body, invocation })
        : callSession(active, channelMethod, body, invocation)
      if (params.resource_id !== undefined && params.resource_id !== selectedProjectId) throw new Error('请求属于其他项目，已拒绝签名。')
      params = { ...params, resource_id: selectedProjectId }
      if (['notifications', 'ack'].includes(method)) {
        if (params.client_id !== undefined && params.client_id !== authorization.scope.device_id) throw new Error('通知请求属于其他设备，已拒绝签名。')
        params = { ...params, client_id: authorization.scope.device_id }
      }
      const prepared = prepareRpc({ method, params, spaceId: authorization.scope.space_id, authorizationId: facts?.authorizationId ?? crypto.randomUUID() })
      if (!authorization.actions.includes(prepared.action)) throw new Error('当前入口未授权此操作，请核对项目权限。')
      const body = record(JSON.parse(JSON.stringify(prepared.body)))
      const projectMetadata = facts?.projectExecutionMetadata ? authorization.projectExecutionMetadata(facts.projectExecutionMetadata.source, facts.projectExecutionMetadata.binding) : undefined
      const rawContext = prepareContext(body, authorization.audience.device, selectedProjectId, facts, projectMetadata, text(authorization.policy.audience_public_key))
      const openRemote = () => facts?.remote ? verifyRemotePolicy({ ...facts.remote.authorization, trust: mergedRemotePolicyTrust(facts.remote.authorization.trust, signingProfile, true) }, rawContext, prepared.action) : Promise.resolve(null)
      let remote: ReturnType<typeof authorization.remote> | null = null
      const releaseRemote = () => { remote?.release(); remote = null }
      try {
        if (replay) {
          if (channelMethod !== 'rpc') throw new Error('原签名仅能用于显式核对原业务请求。')
          remote = await openRemote()
          const replayRemote = remote
          const proof = await withPolicyTrust(signingProfile, () => {
            check(); currentProjectTrust()
            return validateRpcReplay({ proof: replay, body, facts, scope: authorization.scope, projectId: selectedProjectId, audience: authorization.audience,
              ownPublicKey, gatewayPublicKey: presenter, audiencePublicKey: text(authorization.policy.audience_public_key), action: prepared.action, operationId: prepared.authorizationId, remoteReplay: replayRemote ? (body, invocation, action, operationId) => replayRemote.validateReplay(body, invocation, action, operationId, presenter) : undefined, projectMetadata })
          })
          check()
          beforeSend?.(proof)
          const result = await invoke(proof.body, proof.invocation)
          check()
          return { ...proof, result }
        }
        remote = await openRemote()
        const intent = await withPolicyTrust(signingProfile, () => {
          check(); currentProjectTrust()
          return record(JSON.parse((remote ?? authorization).prepare(JSON.stringify(body), JSON.stringify(rawContext), prepared.action, prepared.authorizationId, presenter)))
        })
        check()
        const template = record(await (route.kind === 'gateway' ? forward({ kind: 'prepare', body, intent }) : request('prepare_invocation', { body, intent })))
        check()
        const normalized = record(template.body)
        verifyPreparedBody(body, normalized, facts)
        const invocationTemplate = record(template.invocation)
        if (!same(invocationTemplate.context, rawContext.context)) throw new Error('节点准备的执行范围与本地已验证的工单或会话不同。')
        releaseRemote(); remote = await openRemote()
        const invocation = await withPolicyTrust(signingProfile, () => {
          check(); currentProjectTrust()
          return record(JSON.parse((remote ?? authorization).sign(JSON.stringify(invocationTemplate), JSON.stringify(normalized), prepared.action, prepared.authorizationId, presenter)))
        })
        check()
        beforeSend?.({ body: normalized, invocation })
        const result = await invoke(normalized, invocation)
        check()
        if (channelMethod === 'subscribe') {
          const expiresAt = integer(record(invocation.payload).expires_at_ms)
          const delay = subscriptionRenewalDelay(expiresAt, Date.now())
          if (active.renewal !== undefined) window.clearTimeout(active.renewal)
          active.renewal = window.setTimeout(() => {
            if (session.current !== active || active.authorization !== authorization) return
            void signedCall('conversations', {}, 'subscribe').catch(error => {
              if (session.current !== active || active.authorization !== authorization) return
              active.channel.close(); setError(messageOf(error))
            })
          }, delay)
        }
        return { result, body: normalized, invocation }
      } finally { releaseRemote() }
    }
    try {
      await signedCall('conversations', {}, 'subscribe')
      if (session.current !== active || active.authorization !== authorization) throw changed()
    }
    catch (error) { authorization.release(); if (active.authorization === authorization) active.authorization = undefined; throw error }
    const connection: ReadyConnection = {
      get gateway() {
        const route = active.businessRoute
        return route.kind === 'direct' ? null : { entry: route.card.gateway.entry, publicKey: route.card.gateway.nodePublicKey,
          routeId: text(record(route.card.approval.payload).route_id), status: route.status, error: route.error }
      },
      generation: active.generation, entry: active.entry, scope: authorization.scope, audience: authorization.audience, info: active.info,
      project, projects: authorization.projects, audiencePublicKey: text(authorization.policy.audience_public_key),
      authorization: { enrollmentRights: authorization.enrollmentRights, actions: authorization.actions, authorityEpoch: authorization.authorityEpoch, spaceGenesis: authorization.spaceGenesis, isTeam: authorization.isTeam },
      request: (method, params) => { if (session.current !== active || active.authorization !== authorization) return Promise.reject(changed()); return request(method, params) },
      rpc: (method, params, facts) => signedCall(method, params, 'rpc', undefined, facts).then(reply => reply.result),
      rpcWithProof: (method, params, beforeSend, facts, replay) => signedCall(method, params, 'rpc', beforeSend, facts, replay),
      remote: (value, rawContext, action, use) => verifyRemotePolicy(value, rawContext, action, undefined, use),
      currentRemoteExecutionTrust(trust) {
        if (session.current !== active || active.authorization !== authorization || identityRef.current !== own) throw changed()
        return mergedRemotePolicyTrust(trust, own, true)
      },
      async persistRemoteExecutionTrust(trust) {
        if (session.current !== active || active.authorization !== authorization || identityRef.current !== own) throw changed()
        const updated = await persistRemotePolicyTrust(trust, own)
        if (session.current !== active || active.authorization !== authorization) throw changed()
        return updated
      },
      projectExecutionMetadata: (source, binding) => { if (session.current !== active || active.authorization !== authorization) throw changed(); return authorization.projectExecutionMetadata(source, binding) },
      async remoteExecutionResultHistory(value, actual, original, binding) {
        return verifyRemoteHistory(value, actual, original, binding, (remote, policy) => authorization.remoteExecutionResultHistory(remote, policy, actual, original, binding))
      },
      async remoteResolutionHistory(value, actual, original) {
        return verifyRemoteHistory(value, actual, original, undefined, (remote, policy) => authorization.remoteResolutionHistory(remote, policy, actual, original))
      },
      async observeRemoteExecution(original, remote, rawContext) {
        if (session.current !== active || active.authorization !== authorization) throw changed()
        const handle = await verifyRemotePolicy(remote, rawContext, 'operation_resolve', 'observation_read')
        let signed: Record<string, unknown>
        try { signed = await withPolicyTrust(own, () => {
          if (session.current !== active || active.authorization !== authorization) throw changed()
          currentProjectTrust()
          return handle.observationRequest(original, crypto.randomUUID())
        }) } finally { handle.release() }
        if (session.current !== active || active.authorization !== authorization) throw changed()
        const result = await callSession(active, 'rpc', { method: 'execution_observation', observation_request: signed })
        if (session.current !== active || active.authorization !== authorization) throw changed()
        return result
      },
      async callExecutionResource(remote, body, beforeSend, replay) {
        const controller = new AbortController()
        independentReads.current.add(controller)
        const check = () => { controller.signal.throwIfAborted(); if (session.current !== active || active.authorization !== authorization || identityRef.current !== own) throw changed() }
        let channel: DeviceChannel | null = null
        try {
          check()
          if (remoteExecutionProposal(remote).purpose !== 'rebind') throw new Error('封存需要单独批准的原 C Rebind 授权。')
          const target = remote.share.selection.target_public_key, targetNode = nodeFingerprint(target)
          const stored = savedEntries(storageProfile(own), load).find(entry => entry.node_id === targetNode)
          channel = await connectDevice(stored ?? { address: active.entry.address, node_id: targetNode, relay: true }, { signal: controller.signal })
          check()
          if (channel.publicKey !== target) throw new Error('原执行端连接与独立保存的 C 公钥不同。')
          const compositeTrust = () => {
            check()
            const sourceTrust = currentProjectTrust(), executionTrust = mergedRemotePolicyTrust(remote.trust, own, true)
            const accountPin = sourceTrust.pins.find(pin => pin.domain.kind === 'account' && pin.domain.id === authorization.scope.account_id)
            if (!accountPin) throw new Error('原 B 账户缺少独立保存的根信任。')
            const pins = [...executionTrust.pins]
            const shared = pins.find(pin => same(pin.domain, accountPin.domain))
            if (shared) {
              if (shared.genesis_root !== accountPin.genesis_root || shared.minimum_head !== null && accountPin.minimum_head !== null && !same(shared.minimum_head, accountPin.minimum_head)) throw new Error('B 和 C 保存的账户水位尚未一致，请先完成有限政策同步再封存。')
              if (shared.minimum_head === null) pins[pins.indexOf(shared)] = accountPin
            } else pins.push(accountPin)
            pins.sort((left, right) => { const a = `${left.domain.kind}:${left.domain.id}`, b = `${right.domain.kind}:${right.domain.id}`; return a < b ? -1 : a > b ? 1 : 0 })
            return parseAuthorizationTrust({ kind: 'policy', accountId: authorization.scope.account_id, spaceId: remote.share.selection.execution_resource.space, pins })
          }
          const trust = await withPolicyTrust(own, compositeTrust)
          return await callPinnedExecutionResource({ channel, nodePublicKey: target, own, trust,
            selection: { resourceId: remote.share.selection.execution_resource.resource, grantLeaf: remote.share.selection.execution_grant,
              audience: { device: deriveAuthorizationId('device', target), service: deriveAuthorizationId('service', target) } }, body, check,
            withTrust: work => withPolicyTrust(own, work), latestTrust: compositeTrust,
            async persistTrust(updated) {
              check()
              const executionTrust = mergedRemotePolicyTrust(remote.trust, own, true)
              const execution = parseAuthorizationTrust({ ...executionTrust, pins: executionTrust.pins.map(pin => updated.pins.find(next => same(next.domain, pin.domain)) ?? pin) })
              await persistRemotePolicyTrust(execution, own)
              check()
              const sourceTrust = currentProjectTrust()
              const source = parseAuthorizationTrust({ ...sourceTrust, pins: sourceTrust.pins.map(pin => updated.pins.find(next => same(next.domain, pin.domain)) ?? pin) })
              await persistPolicyTrust(active, source, controller.signal, own)
            }, beforeSend, replay,
          })
        } finally { channel?.close(); independentReads.current.delete(controller) }
      },
      async createRemoteExecutionRequest(input) {
        if (session.current !== active || active.authorization !== authorization) throw changed()
        if (!Number.isSafeInteger(input.expiresAt) || input.expiresAt <= Date.now()) throw new Error('请明确选择晚于当前时刻的执行申请有效期。')
        const creation = verifySignedEvent(JSON.stringify(input.taskCreation), authorization.scope.space_id, selectedProjectId, text(authorization.policy.audience_public_key))
        let taskRef: string
        try {
          const event = record(JSON.parse(creation.bodyJson())), task = record(event.body)
          if (event.scope_id !== input.conversationId || task.type !== 'task_created' || task.task_id !== input.taskId || creation.hash !== input.taskRef) throw new Error('执行申请与已验证的项目、任务或会话不同。')
          taskRef = creation.hash
        } finally { creation.free() }
        const reply = await signedCall('execution_share_request', { conversation_id: input.conversationId, task_id: input.taskId, task_ref: taskRef,
          share_request: { session_id: input.sessionId, executor_kind: input.executorKind, expires_at_ms: input.expiresAt, purpose: input.purpose,
            ...(input.executionResource ? { execution_resource: input.executionResource, target_public_key: input.targetPublicKey } : {}),
            ...(input.oldBinding ? { old_binding: input.oldBinding, old_execution_receipt: input.oldExecutionReceipt } : {}) } }, 'rpc', undefined, { taskRef })
        const produced = record(reply.result), attestation = record(record(produced.task_creation).payload)
        if (!same(produced.project, { space: authorization.scope.space_id, resource: selectedProjectId })
          || produced.source_node_public_key !== authorization.policy.audience_public_key || produced.purpose !== input.purpose
          || produced.session_id !== input.sessionId || produced.executor_kind !== input.executorKind || produced.expires_at_ms !== input.expiresAt
          || attestation.task !== taskRef || attestation.task_id !== input.taskId || attestation.conversation_id !== input.conversationId
          || !same(produced.execution_resource, input.executionResource ?? null) || produced.target_public_key !== (input.targetPublicKey ?? null)
          || !same(produced.old_binding, input.oldBinding ?? null) || !same(produced.old_execution_receipt, input.oldExecutionReceipt ?? null)) throw new Error('Node 的精简执行申请与本地明确选择的任务或用途不同。')
        return withSessionAuthorization(active, own, handle => buildClientRemoteShareIntent(JSON.stringify(produced), handle))

      },
      exportExecutionSourceTrust() {
        if (session.current !== active || active.authorization !== authorization) throw changed()
        const nodePublicKey = text(authorization.policy.audience_public_key)
        return JSON.stringify({ version: 2, kind: 'vibeface-source-trust', trust: currentProjectTrust(), nodePublicKey,
          entry: { address: active.entry.address, node_id: nodeFingerprint(nodePublicKey), relay: active.entry.relay || active.channel.publicKey !== nodePublicKey } })
      },
      subscribe(listener) { if (session.current !== active || active.authorization !== authorization) return () => {}; listeners.current.add(listener); return () => { listeners.current.delete(listener) } },
      isCurrent: () => session.current === active && active.authorization === authorization && generation.current === active.generation,
    }
    active.connection = connection
    setReady(null)
    const storedOwnerRoute = load(ownerGatewayKey(connection.audiencePublicKey), true)
    setOwnerGatewayRoute(storedOwnerRoute ? parseGatewayOwnerRoute(storedOwnerRoute, connection.audiencePublicKey) : null)
    const saved = restoreGateway ? load(gatewayKey(connection), true) : null
    if (saved) {
      const stored = record(saved), card = parseGatewayConnectionCard(stored.card)
      if (gatewayRouteApprovalHash(JSON.stringify(card.approval)) !== stored.approvalHash) throw new Error('已保存的网关路由与原批准不同，请重新核对连接卡。')
      try { await attachGateway(card, active, connection, false) }
      catch (cause) { if (session.current === active) setError(messageOf(cause)) }
    }
    if (session.current !== active || active.authorization !== authorization || active.connection !== connection || identityRef.current !== signingProfile) throw changed()
    setDirectory(null); setReady(connection); setPhase('ready')
    if (active.businessRoute.kind === 'direct' || active.businessRoute.status === 'ready') setError('')
  }
  async function open(entryValue: ConnectionEntry, expectedPublicKey?: string) {
    const parsed = parseEntry(entryValue)
    const preserveSensitive = sensitive.current?.retainForReconnect?.(parsed) === true
    closeConnection(preserveSensitive); setPhase('connecting'); setEntry(parsed)
    const version = generation.current
    const controller = new AbortController(); connecting.current = controller
    try {
      const channel = await connectDevice(parsed, {
        signal: controller.signal,
        onEvent(value) { const event = record(value); if (version === generation.current && event.type === 'changed') for (const listener of listeners.current) listener() },
        onClose(reason) {
          if (version !== generation.current) return
          generation.current += 1; for (const controller of independentReads.current) controller.abort(); independentReads.current.clear(); if (session.current?.businessRoute.kind === 'gateway') session.current.businessRoute.channel?.close(); releaseSession(session.current); session.current = null; if (!sensitive.current?.keepOnNetworkClose?.()) cancelSensitive(); listeners.current.clear(); setReady(null); setDirectory(null); setPhase('disconnected'); setError(messageOf(reason))
        },
      })
      if (version !== generation.current) { channel.close(); throw changed() }
      if (expectedPublicKey && channel.publicKey !== expectedPublicKey) { channel.close(); throw new Error('连接卡中的可信公钥与加密连接目标不一致。') }
      const details = parseInfo(await channel.request('identity', { method: 'status', params: {} }))
      if (version !== generation.current) { channel.close(); throw changed() }
      if (details.node_id !== parsed.node_id || details.public_key !== channel.publicKey) { channel.close(); throw new Error('入口报告的设备与已核验指纹不一致。') }
      const active: Session = { channel, entry: parsed, info: details, generation: version, businessRoute: { kind: 'direct' }, businessGeneration: 0, policyFrames: [] }
      session.current = active; setInfo(details); setPhase('unpaired'); return active
    } catch (cause) {
      if (version === generation.current) { closeConnection(preserveSensitive); setError(messageOf(cause)) }
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
    if (selection && projectId && selection.resourceId !== projectId) { selection.resourceId = projectId; delete selection.grantLeaf }
    const active = await open(candidate)
    let auth: VerifiedClientAuthorization
    try {
      auth = await readAuthentication(active, trust, selection, undefined, projectId ?? selection?.resourceId)
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
    const connection = ready, active = current(), own = identityRef.current, authorization = active.authorization
    if (!own || !authorization || !authorization.enrollmentRights.some(right => right === 'enroll_device' || right === 'revoke_device')) throw new Error('此设备没有账户设备管理权限。')
    const selection = { resourceId: connection.project.id, audience: connection.audience, grantLeaf: authorization.grantLeaf }
    const handle = await readAuthentication(active, authorization.trust, selection)
    await authenticate(handle, active, authorization.trust, selection.resourceId, selection)
    const refreshed = active.authorization
    if (!refreshed) throw changed()
    const directory = await withSessionAuthorization(active, own, () => {
      if (active.authorization !== refreshed) throw changed()
      return refreshed.directory()
    })
    if (session.current !== active || active.authorization !== refreshed) throw changed()
    setDirectory(directory)
  }
  const createTeam: DeviceContextValue['createTeam'] = async input => {
    governanceMaterial()
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    const own = identityRef.current
    if (!own) throw changed()
    if (!authorization) throw changed()
    const recovery = authorization.accountRecoveryPolicy()
    if (recovery.threshold !== 1 || recovery.keys.length !== 1) throw new Error('当前创建入口需要单密钥账户恢复政策。请先核对账户恢复配置。')
    const ownerAccount = ready.scope.account_id
    const root = Identity.generate()
    let prepared: ReturnType<typeof prepareClientTeamCreation>
    try { prepared = await withSessionAuthorization(active, own, handle => prepareClientTeamCreation(root, ownerAccount, active.channel.publicKey, recovery.keys[0], ready.project.id, handle, input.certificateExpiresAt)) }
    catch (error) { input.password = ''; throw error }
    finally { root.free() }
    try { reviewPreparedTeam(prepared, input.password, active, own, ready.project.id) }
    finally { input.password = '' }
  }
  async function restoreTeamCreation(input: { file: File; password: string }) {
    governanceMaterial()
    if (!ready?.isCurrent()) throw changed()
    const active = current(), own = identityRef.current
    if (!own) throw changed()
    if (!input.file.size || input.file.size > 2 * 1024 * 1024 + 4096) throw new Error('请选择不超过 2 MiB 的待提交团队恢复包。')
    let cleartext: Uint8Array | null = null
    let prepared: ReturnType<typeof prepareClientTeamCreation> | null = null
    try {
      const encrypted = new Uint8Array(await input.file.arrayBuffer())
      if (session.current !== active || identityRef.current !== own) throw changed()
      governanceMaterial()
      try { cleartext = recoveryOpen(encrypted, input.password) }
      catch { throw new Error('无法解锁团队恢复包。请核对口令并使用未修改的原文件。') }
      const originalMaterial = new TextDecoder('utf-8', { fatal: true }).decode(cleartext)
      prepared = await withSessionAuthorization(active, own, handle => restoreClientTeamCreation(originalMaterial, handle))
      const original = record(record(JSON.parse(prepared.requestJson())).payload)
      if (original.owner_account !== ready.scope.account_id || original.target_node_public_key !== active.channel.publicKey
        || !same(original.source_project, { space: ready.scope.space_id, resource: ready.project.id })) {
        throw new Error('请使用原设备配置，连接原 Node 并选择原个人项目后，再恢复该团队请求。')
      }
      const restored = prepared
      prepared = null
      reviewPreparedTeam(restored, input.password, active, own, ready.project.id)
    } finally { cleartext?.fill(0); prepared?.free(); input.password = '' }
  }
  function reviewPreparedTeam(prepared: ReturnType<typeof prepareClientTeamCreation>, passwordInput: string, active: Session, own: PublicIdentity, sourceProjectId: string) {
    const authorization = active.authorization
    if (!authorization) { prepared.free(); throw changed() }
    const ownerAccount = authorization.scope.account_id, audience = authorization.audience, nodePublicKey = active.channel.publicKey
    let password = passwordInput
    const encoder = new TextEncoder()
    const seal = (value: string) => { const bytes = encoder.encode(value); try { return recoverySeal(bytes, password) } finally { bytes.fill(0) } }
    let backup: Uint8Array
    try { backup = seal(prepared.recoveryMaterialJson()) }
    catch (error) { prepared.free(); password = ''; throw error }
    const teamId = prepared.teamId(), request = JSON.parse(prepared.requestJson())
    const controller = new AbortController()
    let downloaded = false, running = false, released = false
    let receipt: Record<string, unknown> | null = null
    let committed: { trust: AuthorizationTrust; projectId: string; authorized: boolean } | null = null
    const release = () => { if (!released) { released = true; prepared.free(); backup.fill(0); password = '' } }
    const retained = resumableAuthority(active, own, () => !released && !controller.signal.aborted, [teamId])
    const flow: Sensitive = {
      keepOnNetworkClose: retained.keepOnNetworkClose, retainForReconnect: retained.retainForReconnect,
      cancel() { controller.abort(); if (!running) release() },
      download() { controller.signal.throwIfAborted(); downloadPackage(backup, teamId); downloaded = true },
      async teamCreation(saved) {
        if (running || released) throw new Error('原团队请求仍在等待结果或已关闭。')
        if (!saved || !downloaded) throw new Error('请先下载当前团队恢复包，并确认已妥善保存。')
        controller.signal.throwIfAborted(); const resumed = retained.resume(); running = true
        try {
          if (!committed) {
            setTeamCreationReview({ accountId: ownerAccount, teamId, sourceProjectId, nodePublicKey, submitted: true, committed: false })
            if (!receipt) {
              const reply = record(await callSession(resumed, 'identity', { method: 'team_create', params: { request } }))
              receipt = record(reply.acceptance)
            }
            const verified = finishTeamCreation(prepared, JSON.stringify(receipt), Date.now())
            try {
              const next = seal(verified.recoveryMaterialJson())
              backup.fill(0); backup = next
              committed = { trust: parseAuthorizationTrust({ kind: 'policy', accountId: ownerAccount, spaceId: teamId, pins: JSON.parse(verified.pinsJson()) }), projectId: verified.projectResourceId(), authorized: JSON.parse(verified.scopeJson()) !== null }
              downloaded = false; password = ''
            } finally { verified.free() }
            if (sensitive.current === flow && !controller.signal.aborted) setTeamCreationReview({ accountId: ownerAccount, teamId, sourceProjectId, nodePublicKey, submitted: true, committed: true })
            await persistPolicyTrust(resumed, committed.trust, undefined, own)
            controller.signal.throwIfAborted()
            return
          }
          await persistPolicyTrust(resumed, committed.trust, undefined, own)
          controller.signal.throwIfAborted()
          if (!committed.authorized) throw new Error('团队创建回执已核验并保存。当前团队授权已过期，请保存更新恢复包后重新取得授权。')
          const auth = await readAuthentication(resumed, committed.trust, { resourceId: committed.projectId, audience }, controller.signal)
          await authenticate(auth, resumed, committed.trust, committed.projectId, { resourceId: committed.projectId, audience }, controller.signal)
          if (sensitive.current === flow) cancelSensitive()
        } finally { running = false; if (controller.signal.aborted) release() }
      },
    }
    sensitive.current = flow
    setTeamCreationReview({ accountId: ownerAccount, teamId, sourceProjectId, nodePublicKey, submitted: false, committed: false })
  }
  async function confirmTeamCreation(saved: boolean) {
    if (!sensitive.current?.teamCreation) throw new Error('当前没有待核对的团队创建请求。')
    await sensitive.current.teamCreation(saved)
  }
  async function submitGovernance(prepared: ReturnType<typeof prepareClientMembershipChange>, label: string, protocol?: GovernanceProtocol) {
    if (!ready?.isCurrent()) { prepared.free(); throw changed() }
    const active = current(), authorization = active.authorization
    const own = identityRef.current
    if (!own) { prepared.free(); throw changed() }
    if (!authorization) { prepared.free(); throw changed() }
    const selection = { resourceId: ready.project.id, audience: ready.audience }
    const request = JSON.parse(prepared.requestJson())
    const commit = protocol ?? {
      method: 'space_policy_commit', params: { request },
      verify(committed: Record<string, unknown>) {
        const verified = finishSpaceGovernanceChange(prepared, JSON.stringify(committed), Date.now())
        try { return { trust: parseAuthorizationTrust({ ...authorization.trust, pins: JSON.parse(verified.pinsJson()) }), authorized: JSON.parse(verified.scopeJson()) !== null } }
        finally { verified.free() }
      },
      afterSaved() {},
      dispose() {},
    }
    cancelSensitive()
    const controller = new AbortController()
    let running = false, released = false, published = false
    let confirmed: GovernanceConfirmation | null = null
    const release = () => { if (!released) { released = true; prepared.free(); commit.dispose() } }
    const retained = resumableAuthority(active, own, () => !released && !controller.signal.aborted)
    const flow: Sensitive = {
      keepOnNetworkClose: retained.keepOnNetworkClose, retainForReconnect: retained.retainForReconnect,
      cancel() { controller.abort(); if (!running) release() },
      async governance() {
        if (running || released) throw new Error('原政策请求仍在等待结果或已关闭。')
        controller.signal.throwIfAborted(); const resumed = retained.resume(); running = true
        try {
          if (!confirmed) {
            const reply = record(await callSession(resumed, 'identity', { method: commit.method, params: commit.params }))
            confirmed = commit.verify(record(reply.committed))
          }
          confirmed.trust = await persistPolicyTrust(resumed, confirmed.trust, undefined, own)
          controller.signal.throwIfAborted()
          if (!confirmed.authorized) { lockVault(); return }
          if (!published) {
            const confirmation = confirmed
            await withPolicyTrust(own, () => {
              controller.signal.throwIfAborted()
              confirmation.trust = mergedRemotePolicyTrust(confirmation.trust, own)
            })
            await commit.afterSaved(confirmation, resumed, controller.signal)
            controller.signal.throwIfAborted(); published = true
          }
          const refreshTrust = mergeAuthorizationTrust(authorization.trust, [confirmed.trust])
          const auth = await readAuthentication(resumed, refreshTrust, selection, controller.signal)
          await authenticate(auth, resumed, refreshTrust, selection.resourceId, selection, controller.signal)
          if (sensitive.current === flow) cancelSensitive()
        } finally { running = false; if (controller.signal.aborted) release() }
      },
    }
    sensitive.current = flow; setGovernancePending(label)
    await flow.governance?.()
  }
  function governanceMaterial() {
    if (sensitive.current) throw new Error('请先核对或关闭当前待处理的敏感请求。')
    if (!ready?.isCurrent()) throw changed()
    const authorization = current().authorization
    if (!authorization) throw changed()
    const own = identityRef.current
    if (!own) throw changed()
    return { ...authorization.enrollmentMaterial(), pins: mergedRemotePolicyTrust(authorization.trust, own).pins, spaceId: ready.scope.space_id }
  }
  function prepareGovernance<T>(work: (authorization: VerifiedClientAuthorization) => T) {
    governanceMaterial()
    const active = current(), own = identityRef.current
    if (!own) throw changed()
    return withSessionAuthorization(active, own, work)
  }
  const changeMembership: DeviceContextValue['changeMembership'] = async input => {
    const material = governanceMaterial()
    const intent = { space: material.spaceId, subject_account: input.subjectAccount, roles: input.roles, expires_at_ms: input.expiresAt }
    await submitGovernance(await prepareGovernance(handle => prepareClientMembershipChange(JSON.stringify(intent), handle)), '成员政策变更')
  }
  const grantProject: DeviceContextValue['grantProject'] = async input => {
    const material = governanceMaterial()
    if (!ready || input.scope.value.space !== ready.scope.space_id || input.scope.value.resource !== ready.project.id) throw new Error('授权范围与当前项目不同。')
    const intent = { space: material.spaceId, grantee: input.grantee, scope: input.scope, actions: input.actions, audience_node_public_key: input.audienceNodePublicKey,
      constraints: input.constraints, remaining_delegations: input.remainingDelegations, expires_at_ms: input.expiresAt }
    await submitGovernance(await prepareGovernance(handle => prepareClientDirectionalGrant(JSON.stringify(intent), handle)), '项目授权')
  }
  const revokeProjectGrant: DeviceContextValue['revokeProjectGrant'] = async hash => {
    const material = governanceMaterial()
    await submitGovernance(await prepareGovernance(handle => prepareClientGrantRevocation(material.spaceId, hash, handle)), '授权撤销')
  }
  const registerExecutionService: DeviceContextValue['registerExecutionService'] = async input => {
    const material = governanceMaterial()
    if (!ready?.isCurrent()) throw changed()
    if (!Number.isSafeInteger(input.expiresAt) || input.expiresAt <= Date.now()) throw new Error('请明确选择晚于当前时刻的登记有效期。')
    const connection = ready, active = current(), nodePublicKey = connection.audiencePublicKey, own = identityRef.current
    if (!own) throw changed()
    if (nodePublicKey !== active.channel.publicKey) throw new Error('请直接连接项目所在的已核验 Node，再登记它的执行资源。当前入口暂不转送账户根批准。')
    const controller = new AbortController()
    let password = input.password, opened: Awaited<ReturnType<typeof openAccountPackage>> | null = null
    const reservation: Sensitive = { cancel() { controller.abort(); opened?.release(); opened = null; password = ''; input.password = '' } }
    const check = () => { controller.signal.throwIfAborted(); if (sensitive.current !== reservation || session.current !== active || identityRef.current !== own || !connection.isCurrent()) throw changed() }
    sensitive.current = reservation
    let transferred = false, resourceId = '', backup: Uint8Array | null = null, backupTransferred = false
    try {
      opened = await openAccountPackage(input.file, password)
      check()
      if (opened.scope.account_id !== connection.scope.account_id) throw new Error('恢复包必须属于此 Node 所属的个人账户。')
      const personalSpaceId = opened.scope.space_id
      const known = [...savedSpaces(), ...savedRemotePolicyTrusts(own)].filter(item => item.accountId === connection.scope.account_id && item.spaceId === personalSpaceId)
      if (!known[0]) throw new Error('请先连接并核验本人个人空间，再登记执行资源。恢复包不用于建立公共历史信任。')
      const savedOwner = load(targetKey(personalSpaceId, active.entry), true)
      if (!savedOwner) throw new Error('请先连接本人个人项目，保存已核验的公共账户授权，再返回当前项目登记执行资源。')
      const ownerTarget = record(savedOwner), ownerGrantLeaf = ownerTarget.ownerGrantLeaf ?? ownerTarget.grantLeaf
      if (!ownerGrantLeaf) throw new Error('请先重新连接本人个人项目，核验原账户管理授权。')
      const ownerSelection = parseAuthorizationSelection({ ...ownerTarget, grantLeaf: ownerGrantLeaf })
      const ownerHandle = await readAuthentication(active, mergeAuthorizationTrust(known[0], known), ownerSelection, controller.signal)
      let owner: ReturnType<typeof verifyScopedAuthentication> | null = null
      let incomingOwner: VerifiedClientAuthorization | null = ownerHandle
      let prepared: ReturnType<typeof prepareClientExecutionServiceRegistration>
      try {
        owner = await withPolicyTrust(own, () => {
          check()
          const handle = incomingOwner
          if (!handle) throw changed()
          incomingOwner = null
          return verifyScopedAuthentication(handle, own, mergedRemotePolicyTrust(mergeAuthorizationTrust(known[0], known), own), nodePublicKey, ownerSelection)
        })
        await persistPolicyTrust(active, owner.trust, controller.signal, own)
        const privateMaterial = opened, ownerAuthorization = owner
        prepared = await withPolicyTrust(own, () => {
          check()
          ownerAuthorization.revalidate(mergedRemotePolicyTrust(mergeAuthorizationTrust(ownerAuthorization.trust, savedSpaces()), own))
          const source = active.authorization
          if (!source) throw changed()
          source.revalidate(mergedRemotePolicyTrust(mergeAuthorizationTrust(source.trust, savedSpaces()), own))
          return source.withNative(sourceHandle => {
            const management = executionManagement(sourceHandle, { space: material.spaceId, resource: connection.project.id }, nodePublicKey)
            try { return ownerAuthorization.withNative(ownerHandle => prepareClientExecutionServiceRegistration(sourceHandle, management.selection, ownerHandle, privateMaterial.material, nodePublicKey, input.expiresAt)) }
            finally { management.selection.free() }
          })
        })
      } finally { incomingOwner?.free(); owner?.release() }
      try { check() } catch (error) { prepared.free(); throw error }
      opened.release(); opened = null; sensitive.current = null
      transferred = true
      await submitGovernance(prepared, '执行资源登记', {
        method: 'execution_service_register', params: record(JSON.parse(prepared.requestJson())),
        verify(committed) {
          const verified = finishExecutionServiceRegistration(prepared, JSON.stringify(committed), Date.now())
          try {
            if (verified.spaceId() !== prepared.spaceId() || verified.resourceId() !== prepared.resourceId()) throw new Error('执行资源登记回执与原请求不同。')
            resourceId = verified.resourceId()
            const cleartext = new TextEncoder().encode(verified.recoveryMaterialJson())
            try { const next = recoverySeal(cleartext, password); if (!backupTransferred) backup?.fill(0); backup = next } finally { cleartext.fill(0) }
            const bytes = backup
            setExecutionServiceBackups(previous => {
              const next = new Map(previous), profile = storageProfile(own)
              next.get(profile)?.bytes.fill(0); next.set(profile, { bytes, accountId: connection.scope.account_id, own })
              return next
            }); backupTransferred = true
            const selection = verified.authorization()
            try { return { trust: parseAuthorizationTrust({ kind: 'policy', accountId: connection.scope.account_id, spaceId: verified.spaceId(), pins: JSON.parse(verified.pinsJson()) }), authorized: JSON.parse(verified.scopeJson()) !== null, grantLeaf: text(record(JSON.parse(selection.policyJson())).grant_leaf) } }
            finally { selection.free() }
          } finally { verified.free() }
        },
        async afterSaved(confirmed, resumed, governanceSignal) {
          if (!backup || !confirmed.grantLeaf) throw new Error('更新的账户恢复包或执行管理授权尚未准备完成，请保留原请求继续核对。')
          const grantLeaf = confirmed.grantLeaf
          const selection = parseAuthorizationSelection({ resourceId, audience: { device: deriveAuthorizationId('device', nodePublicKey), service: deriveAuthorizationId('service', nodePublicKey) }, grantLeaf: confirmed.grantLeaf })
          let incoming: VerifiedClientAuthorization | null = await readAuthentication(resumed, confirmed.trust, selection, governanceSignal)
          const candidate: { authorization: VerifiedClientAuthorization | null } = { authorization: null }
          try {
            await withPolicyTrust(own, () => {
              governanceSignal.throwIfAborted()
              if (session.current !== resumed || identityRef.current !== own || resumed.channel.publicKey !== nodePublicKey
                || resumed.authorization?.scope.space_id !== material.spaceId || !resumed.authorization.projects.some(project => project.id === connection.project.id) || !incoming) throw changed()
              candidate.authorization = revalidateClientPolicyAuthorization(incoming, JSON.stringify(mergedRemotePolicyTrust(mergeAuthorizationTrust(confirmed.trust, savedSpaces()), own).pins), Date.now())
              const next: ExecutionServiceContext = { spaceId: confirmed.trust.spaceId, sourceSpaceId: material.spaceId, sourceProjectId: connection.project.id, nodePublicKey, resourceId, trust: confirmed.trust, authorization: candidate.authorization, grantLeaf, own }
              saveExecutionServiceChoice(next)
              replaceExecutionAuthorizations([next]); candidate.authorization = null
              setExecutionServiceResource(next); setExecutionServiceResources([next])
            })
          } finally { incoming?.free(); incoming = null; candidate.authorization?.free() }
        },
        dispose() { password = ''; if (!backupTransferred) backup?.fill(0) },
      })
      if (!resourceId) throw new Error('执行资源登记尚未取得当前有效回执，请核对原请求。')
      return resourceId
    } finally {
      opened?.release(); input.password = ''
      if (!transferred) { password = ''; if (sensitive.current === reservation) sensitive.current = null }
    }
  }
  function replaceExecutionAuthorizations(available: ExecutionServiceContext[]) {
    const retained = new Set(available.map(item => item.authorization))
    for (const authorization of executionAuthorizations.current) if (!retained.has(authorization)) authorization.free()
    executionAuthorizations.current = retained
  }
  function matchesExecutionService(value: ExecutionServiceContext) {
    return !!ready?.isCurrent() && identityRef.current === value.own && value.sourceSpaceId === ready.scope.space_id
      && value.sourceProjectId === ready.project.id && value.nodePublicKey === ready.audiencePublicKey
  }
  function currentExecutionService() { return executionServiceResource && matchesExecutionService(executionServiceResource) ? executionServiceResource : null }
  function executionServiceChoiceKey(value: Pick<ExecutionServiceContext, 'own' | 'sourceSpaceId' | 'sourceProjectId' | 'nodePublicKey'>) {
    return `profile.${storageProfile(value.own)}.execution-service-choice.${JSON.stringify([value.sourceSpaceId, value.sourceProjectId, value.nodePublicKey])}`
  }
  function saveExecutionServiceChoice(value: ExecutionServiceContext) {
    const key = executionServiceChoiceKey(value)
    if (value.own.profileId === null) temporaryRecords.current.set(key, value.resourceId)
    else writePersistent(key, value.resourceId)
  }
  async function readExecutionServices(resourceId?: string) {
    governanceMaterial()
    const connection = ready, active = current(), own = identityRef.current, source = active.authorization
    if (!connection || !own || !source) throw changed()
    if (active.channel.publicKey !== connection.audiencePublicKey) throw new Error('请直接连接物理执行 Node，再读取已登记的执行资源。')
    const sourceProject = { space: connection.scope.space_id, resource: connection.project.id }
    const chosen = resourceId ? executionServiceResources.find(item => item.resourceId === resourceId && matchesExecutionService(item)) : null
    if (resourceId && !chosen) throw new Error('请先读取当前项目已登记的执行资源，再选择列表中的原资源。')
    const controller = new AbortController(), reservation: Sensitive = { cancel() { controller.abort() } }
    const check = () => { controller.signal.throwIfAborted(); if (!connection.isCurrent() || identityRef.current !== own || sensitive.current !== reservation) throw changed() }
    sensitive.current = reservation
    const available: ExecutionServiceContext[] = []
    let published = false
    try {
      const sourceGrant = await withPolicyTrust(own, () => {
        check(); source.revalidate(mergedRemotePolicyTrust(mergeAuthorizationTrust(source.trust, savedSpaces()), own))
        return source.withNative(handle => { const management = executionManagement(handle, sourceProject, connection.audiencePublicKey); try { return management.grant } finally { management.selection.free() } })
      })
      let cursor: string | null = null
      do {
        const reply = record(await callSession(active, 'identity', { method: 'execution_service_select', params: {
          account_id: connection.scope.account_id, device_public_key: own.publicKey, source_project: sourceProject, source_grant: sourceGrant,
          target_node_public_key: connection.audiencePublicKey, ...(chosen ? { execution_resource: { space: chosen.spaceId, resource: chosen.resourceId } } : { cursor }),
        } }))
        check()
        const selections = array(reply.selections)
        if (selections.length > 1) throw new Error('执行资源列表超过单页允许的候选数量。')
        for (const value of selections) {
          const selection = record(value), resource = record(selection.execution_resource)
          const executionResource = { space: text(resource.space), resource: text(resource.resource) }
          if (resourceId && (executionResource.resource !== resourceId || executionResource.space !== chosen?.spaceId)) throw new Error('Node 返回了其他执行资源，已拒绝切换。')
          const known = [...savedSpaces(), ...savedRemotePolicyTrusts(own)].filter(item => item.accountId === connection.scope.account_id && item.spaceId === executionResource.space)
          if (!known[0]) throw new Error('请先连接并核验本人个人空间，再读取已登记的执行资源。')
          const trust = mergedRemotePolicyTrust(mergeAuthorizationTrust(known[0], known), own)
          const requested = parseAuthorizationSelection({ resourceId: executionResource.resource,
            audience: { device: deriveAuthorizationId('device', connection.audiencePublicKey), service: deriveAuthorizationId('service', connection.audiencePublicKey) }, grantLeaf: selection.management_parent })
          let imported: VerifiedClientAuthorization | null = await readAuthentication(active, trust, requested, controller.signal)
          let verified: VerifiedClientAuthorization | null = null
          try {
            verified = await withPolicyTrust(own, () => {
              check()
              if (!imported) throw changed()
              const current = revalidateClientPolicyAuthorization(imported, JSON.stringify(mergedRemotePolicyTrust(mergeAuthorizationTrust(trust, savedSpaces()), own).pins), Date.now())
              try { return verifyExecutionServiceSelection(current, JSON.stringify(selection), JSON.stringify(sourceProject), JSON.stringify(executionResource), connection.audiencePublicKey, Date.now()) }
              finally { current.free() }
            })
            const policy = record(JSON.parse(verified.policyJson())), heads = array(policy.heads).map(value => record(value))
            const updated = parseAuthorizationTrust({ ...trust, pins: trust.pins.map(pin => ({ ...pin, minimum_head: heads.find(head => same(head.domain, pin.domain)) ?? pin.minimum_head })) })
            const saved = await persistPolicyTrust(active, updated, controller.signal, own)
            check()
            available.push({ spaceId: executionResource.space, sourceSpaceId: sourceProject.space, sourceProjectId: sourceProject.resource,
              nodePublicKey: connection.audiencePublicKey, resourceId: executionResource.resource, trust: saved, authorization: verified, grantLeaf: text(policy.grant_leaf), own })
            verified = null
          } finally { imported?.free(); imported = null; verified?.free() }
        }
        if (chosen || reply.next_cursor === null) break
        const next = text(reply.next_cursor)
        if (!/^res2_[a-f0-9]{64}$/.test(next) || cursor !== null && next <= cursor) throw new Error('执行资源分页游标无效。')
        cursor = next
      } while (cursor !== null)
      await withPolicyTrust(own, () => {
        check()
        if (new Set(available.map(item => item.resourceId)).size !== available.length) throw new Error('Node 返回了重复的执行资源。')
        const key = executionServiceChoiceKey({ own, sourceSpaceId: sourceProject.space, sourceProjectId: sourceProject.resource, nodePublicKey: connection.audiencePublicKey })
        const remembered = own.profileId === null ? temporaryRecords.current.get(key) : readPersistent(key)
        const selected = resourceId ? available.find(item => item.resourceId === resourceId) : available.find(item => item.resourceId === remembered) ?? (available.length === 1 ? available[0] : null)
        if (resourceId && !selected) throw new Error('原执行资源当前不可用，请核对它的授权和登记状态。')
        for (const value of available) {
          const current = revalidateClientPolicyAuthorization(value.authorization, JSON.stringify(mergedRemotePolicyTrust(mergeAuthorizationTrust(value.trust, savedSpaces()), own).pins), Date.now())
          value.authorization.free(); value.authorization = current
        }
        const next = resourceId ? [...executionServiceResources.filter(item => item.resourceId !== resourceId && matchesExecutionService(item)), ...available] : available
        if (selected) saveExecutionServiceChoice(selected)
        replaceExecutionAuthorizations(next)
        setExecutionServiceResources(next); setExecutionServiceResource(selected ?? null); setRemoteExecutionShare(null); published = true
      })
    } finally {
      if (!published) for (const value of available) value.authorization.free()
      if (sensitive.current === reservation) sensitive.current = null
    }
  }
  async function refreshExecutionServices() { await readExecutionServices() }
  async function selectExecutionService(resourceId: string) { await readExecutionServices(resourceId) }
  function downloadExecutionServiceBackup() {
    const backup = currentExecutionServiceBackup()
    if (!backup) throw new Error('请解锁原设备配置，再下载已核验的更新账户恢复包。')
    downloadPackage(backup.bytes, backup.accountId)
  }
  function currentExecutionServiceBackup() {
    const own = identityRef.current
    if (!own) return null
    const backup = executionServiceBackups.get(storageProfile(own))
    return backup?.own.publicKey === own.publicKey ? backup : null
  }
  function savedRemotePolicyTrusts(own: PublicIdentity) {
    const key = `profile.${storageProfile(own)}.remote-policy-trusts`
    return array((own.profileId === null ? temporaryRecords.current.get(key) : readPersistent(key, true)) ?? []).map(parseAuthorizationTrust)
  }
  function mergedRemotePolicyTrust(trust: AuthorizationTrust, own: PublicIdentity, requireSaved = false) {
    const profile = storageProfile(own), key = `profile.${profile}.policy-trusts`
    const ownTrusts = array((own.profileId === null ? temporaryRecords.current.get(key) : readPersistent(key, true)) ?? []).map(parseAuthorizationTrust)
    const known = [...savedRemotePolicyTrusts(own), ...ownTrusts].filter(item => item.accountId === trust.accountId)
    if (requireSaved && !known.some(item => item.spaceId === trust.spaceId)) throw new Error('请先独立核对并保存此执行 Node 的完整根信任。')
    return mergeAuthorizationTrust(parseAuthorizationTrust(trust), known)
  }
  async function persistRemotePolicyTrust(trust: AuthorizationTrust, own: PublicIdentity) {
    const profile = storageProfile(own), key = `profile.${profile}.remote-policy-trusts`, temporary = own.profileId === null
    if (!('locks' in navigator)) throw new Error('此浏览器需要 Web Locks 才能保存独立源信任。')
    const updated = await navigator.locks.request(`profile.${profile}.policy-trust-writer`, () => {
      if (temporary && identityRef.current !== own) throw changed()
      const all = savedRemotePolicyTrusts(own), sameAccount = all.filter(item => item.accountId === trust.accountId)
      const updated = mergedRemotePolicyTrust(trust, own)
      const next = [...all.filter(item => item.accountId !== trust.accountId), ...sameAccount.filter(item => item.spaceId !== trust.spaceId).map(item => mergeAuthorizationTrust(item, [updated])), updated]
      if (temporary) temporaryRecords.current.set(key, JSON.parse(JSON.stringify(next)))
      else writePersistent(key, next)
      return updated
    })
    if (identityRef.current !== own) throw changed()
    return mergedRemotePolicyTrust(updated, own, true)
  }
  function savedSourceTrustCards(own: PublicIdentity) {
    const key = `profile.${storageProfile(own)}.remote-source-cards`
    return array((own.profileId === null ? temporaryRecords.current.get(key) : readPersistent(key, true)) ?? []).map(parseRemoteExecutionSourceTrust)
  }
  async function verifiedRemoteExecutionRequest(input: Parameters<TeamControlsProps['inspectRemoteExecutionRequest']>[0]) {
    if (!ready?.isCurrent()) throw changed()
    const connection = ready, own = identityRef.current
    if (!own) throw changed()
    const signed = record(JSON.parse(input.requestJson)), intent = record(signed.intent), project = record(intent.project)
    let candidates = savedSourceTrustCards(own)
    if (input.sourceTrustJson) {
      if (!input.confirmedSourceRoots) throw new Error('请先通过独立可信渠道核对完整 B 根信任和 Node 公钥。')
      candidates = [parseRemoteExecutionSourceTrust(JSON.parse(input.sourceTrustJson))]
    }
    const card = candidates.find(candidate => candidate.nodePublicKey === intent.source_node_public_key && candidate.trust.accountId === intent.requester && candidate.trust.spaceId === project.space)
    if (!card) throw new Error('请导入通过独立可信渠道核对的完整 B 来源卡，包含原 Node 公钥及入口。')
    const controller = new AbortController(); independentReads.current.add(controller)
    const check = () => { controller.signal.throwIfAborted(); if (!connection.isCurrent() || identityRef.current !== own) throw changed() }
    let channel: DeviceChannel | null = null
    let verified: Awaited<ReturnType<typeof readRemoteSharePolicy>>['verified'] | null = null
    try {
      const trust = mergedRemotePolicyTrust(card.trust, own)
      channel = await connectDevice(card.entry, { signal: controller.signal })
      check()
      if (channel.publicKey !== card.nodePublicKey) throw new Error('B 来源连接与独立保存的 Node 公钥不同。')
      const sourceChannel = channel
      const imported = await readRemoteSharePolicy({ intentJson: input.requestJson, trust, nodePublicKey: card.nodePublicKey,
        call: (method, params) => sourceChannel.request('identity', { method, params }), check, signal: controller.signal })
      verified = imported.verified
      const metadata = record(JSON.parse(verified.summaryJson())), verifiedProject = record(metadata.project)
      if (metadata.requesterAccountId !== trust.accountId || verifiedProject.space !== trust.spaceId) throw new Error('已验证的来源申请属于其他账户或空间。')
      const savedTrust = await persistRemotePolicyTrust(imported.trust, own)
      check()
      const qualified = verified
      await withPolicyTrust(own, () => {
        check()
        const next = qualified.revalidate(JSON.stringify(mergedRemotePolicyTrust(savedTrust, own, true).pins), Date.now())
        qualified.free(); verified = next
        const key = `profile.${storageProfile(own)}.remote-source-cards`, saved = savedSourceTrustCards(own)
        const nextCards = [...saved.filter(value => value.nodePublicKey !== card.nodePublicKey || value.trust.accountId !== trust.accountId || value.trust.spaceId !== trust.spaceId), { ...card, trust: savedTrust }]
        const values = nextCards.map(value => ({ version: 2, kind: 'vibeface-source-trust', ...value }))
        if (own.profileId === null) temporaryRecords.current.set(key, values)
        else writePersistent(key, values)
      })
      const purpose = metadata.purpose
      if (purpose !== 'execution' && purpose !== 'resolution' && purpose !== 'rebind') throw new Error('来源申请没有明确的单一用途。')
      const summary: RemoteExecutionRequestSummary = { requestHash: text(metadata.intentHash), projectResourceId: text(verifiedProject.resource), projectLabel: '', taskId: text(metadata.taskId), purpose,
        executorKind: text(metadata.executorKind), servicePublicKey: card.nodePublicKey, expiresAt: integer(metadata.expiresAtMs) }
      const result = verified
      if (!result) throw changed()
      verified = null
      return { summary, trust: savedTrust, verified: result }
    } finally { verified?.free(); channel?.close(); independentReads.current.delete(controller) }
  }
  const inspectRemoteExecutionRequest: DeviceContextValue['inspectRemoteExecutionRequest'] = async input => {
    const source = await verifiedRemoteExecutionRequest(input)
    try { return source.summary } finally { source.verified.free() }
  }
  const createRemoteExecutionShare: DeviceContextValue['createRemoteExecutionShare'] = async rawInput => {
    governanceMaterial()
    const input = parseRemoteExecutionShareInput(rawInput, Date.now()), execution = currentExecutionService(), own = identityRef.current
    if (!execution || !own || input.executionResourceId !== execution.resourceId) throw new Error('先为当前项目登记并核验执行资源，再生成受限分享。')
    const source = await verifiedRemoteExecutionRequest({ requestJson: input.requestJson, confirmedSourceRoots: false })
    let prepared: ReturnType<typeof prepareClientRemoteExecutionShare>
    try {
      prepared = await withPolicyTrust(own, () => {
        governanceMaterial()
        if (currentExecutionService() !== execution || identityRef.current !== own) throw changed()
        const executionTrust = mergedRemotePolicyTrust(mergeAuthorizationTrust(execution.trust, savedSpaces()), own)
        const owner = revalidateClientPolicyAuthorization(execution.authorization, JSON.stringify(executionTrust.pins), Date.now())
        try {
          const request = source.verified.revalidate(JSON.stringify(mergedRemotePolicyTrust(source.trust, own, true).pins), Date.now())
          try { return prepareClientRemoteExecutionShare(JSON.stringify({ executionResourceId: input.executionResourceId, actions: input.actions, expiresAt: input.expiresAt }), owner, request) }
          finally { request.free() }
        } finally { owner.free() }
      })
    } finally { source.verified.free() }
    let verified: ReturnType<typeof finishRemoteExecutionShare> | null = null, result = ''
    await submitGovernance(prepared, '远端执行分享授权', {
      method: 'space_policy_commit', params: { request: JSON.parse(prepared.requestJson()) },
      verify(committed) {
        verified = finishRemoteExecutionShare(prepared, JSON.stringify(committed), Date.now())
        return { trust: parseAuthorizationTrust({ ...execution.trust, pins: JSON.parse(verified.pinsJson()) }), authorized: JSON.parse(verified.scopeJson()) !== null }
      },
      async afterSaved(confirmed, resumed, signal) {
        if (!verified) throw new Error('原分享授权回执尚未核验，请继续核对原请求。')
        result = verified.referenceJson()
        const share = parseRemoteExecutionShare(JSON.parse(result))
        if (share.selection.execution_resource.space !== execution.spaceId || share.selection.execution_resource.resource !== execution.resourceId || share.selection.target_public_key !== execution.nodePublicKey || share.selection.execution_grant !== prepared.grantHash() || share.purpose !== source.summary.purpose) throw new Error('原生分享引用与本次执行资源、用途或 Grant 不同。')
        const selection = { resourceId: execution.resourceId, audience: { device: deriveAuthorizationId('device', execution.nodePublicKey), service: deriveAuthorizationId('service', execution.nodePublicKey) }, grantLeaf: execution.grantLeaf }
        let imported: VerifiedClientAuthorization | null = await readAuthentication(resumed, confirmed.trust, selection, signal)
        const pending: { authorization: VerifiedClientAuthorization | null } = { authorization: null }
        try {
          await withPolicyTrust(own, () => {
            signal.throwIfAborted()
            if (session.current !== resumed || identityRef.current !== own || !imported) throw changed()
            pending.authorization = revalidateClientPolicyAuthorization(imported, JSON.stringify(mergedRemotePolicyTrust(mergeAuthorizationTrust(confirmed.trust, savedSpaces()), own).pins), Date.now())
            const next: ExecutionServiceContext = { ...execution, trust: confirmed.trust, authorization: pending.authorization }
            saveExecutionServiceChoice(next)
            const available = [...executionServiceResources.filter(value => value.resourceId !== next.resourceId && executionAuthorizations.current.has(value.authorization)), next]
            replaceExecutionAuthorizations(available); pending.authorization = null
            setExecutionServiceResource(next); setExecutionServiceResources(available); setRemoteExecutionShare({ value: result, execution: next })
          })
        } finally { imported?.free(); imported = null; pending.authorization?.free() }
      },
      dispose() { verified?.free(); verified = null },
    })
    if (!result) throw new Error('原分享授权尚未取得当前有效回执，请核对原请求。')
    return result
  }
  async function retryGovernance() {
    if (!sensitive.current?.governance) throw new Error('当前没有待核对的政策提交。')
    await sensitive.current.governance()
  }
  async function beginNodeEnrollment(input: { file: File; password: string; connectionCard: string; certificateExpiresAt: number }) {
    if (!Number.isSafeInteger(input.certificateExpiresAt) || input.certificateExpiresAt <= Date.now()) throw new Error('请选择晚于当前时刻的服务证书有效期。')
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    const own = identityRef.current
    if (!own) throw changed()
    if (!authorization) throw changed()
    const selection = { resourceId: ready.project.id, audience: ready.audience }
    const card = parseConnectionCard(readInvitation(input.connectionCard))
    if (card.kind !== 'claim') throw new Error('请在新 Node 本机运行 vibeface access，使用包含本机认领凭证的可信连接卡。')
    if (card.nodePublicKey === active.channel.publicKey) throw new Error('请选择尚未加入账户的另一台 Node。')
    cancelSensitive()
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof prepareNodeEnrollment>> | null = null
    let trust = authorization.trust
    const retained = resumableAuthority(active, own, () => prepared !== null && !controller.signal.aborted)
    const flow: Sensitive = {
      keepOnNetworkClose: retained.keepOnNetworkClose, retainForReconnect: retained.retainForReconnect,
      cancel() { controller.abort(); prepared?.release(); prepared = null },
      async nodeEnrollment() {
        if (!prepared || controller.signal.aborted) throw changed()
        const original = prepared, resumed = retained.resume()
        await original.prepare()
        controller.signal.throwIfAborted()
        setNodeEnrollmentReview(value => value ? { ...value, submitted: true } : null)
        await original.commit(async pins => { trust = parseAuthorizationTrust({ ...trust, pins }); await persistPolicyTrust(resumed, trust, undefined, own) })
        controller.signal.throwIfAborted()
        saveEntry(original.entry)
        const auth = await readAuthentication(resumed, trust, selection, controller.signal)
        await authenticate(auth, resumed, trust, selection.resourceId, selection, controller.signal)
        if (sensitive.current === flow) cancelSensitive()
      },
    }
    sensitive.current = flow
    try {
      prepared = await prepareNodeEnrollment({ ...input, accountId: ready.scope.account_id, card, signal: controller.signal,
        prepare: (material, claim, expiry) => withSessionAuthorization(active, own, handle => prepareClientAccountNodeEnrollment(material, handle, claim, expiry), controller.signal) })
      controller.signal.throwIfAborted()
      if (session.current !== active) throw changed()
      setNodeEnrollmentReview({ accountId: prepared.accountId, nodePublicKey: prepared.nodePublicKey, address: prepared.entry.address, submitted: false })
      await prepared.prepare()
    } catch (error) { if (!prepared || controller.signal.aborted) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }; throw error }
    finally { input.password = '' }
  }
  async function confirmNodeEnrollment() {
    if (!sensitive.current?.nodeEnrollment) throw new Error('当前没有待批准的 Node 入网请求。')
    await sensitive.current.nodeEnrollment()
  }
  async function createEnrollmentOffer(manageDevices: boolean, route?: ConnectionEntry) {
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    if (!authorization || !authorization.enrollmentRights.includes('enroll_device')) throw new Error('当前设备没有新增账户设备的权限。')
    if (ready.audiencePublicKey !== active.channel.publicKey) throw new Error('请连接账户所属 Node，再生成新设备邀请。')
    const selected = route ?? entries.find(entry => entry.node_id === active.entry.node_id && entry.relay && new URL(entry.address).protocol === 'https:') ?? active.entry
    if (selected.node_id !== active.entry.node_id) throw new Error('邀请路径必须指向当前账户节点。')
    if (selected.address !== active.entry.address || selected.relay !== active.entry.relay) {
      const probe = await connectDevice(selected)
      try {
        const info = record(await probe.request('info'))
        if (probe.publicKey !== active.channel.publicKey || info.node_id !== active.entry.node_id || info.public_key !== active.channel.publicKey) throw new Error('所选入口没有连接到当前节点。')
      } finally { probe.close() }
      if (session.current !== active) throw changed()
    }
    const material = authorization.enrollmentMaterial()
    if (material.expiresAt === null) throw new Error('当前设备没有有效的账户管理期限。')
    const certificateExpiry = material.expiresAt
    const rights = manageDevices ? authorization.enrollmentRights.filter(right => right === 'enroll_device' || right === 'revoke_device').sort() : []
    const own = identityRef.current
    if (!own) throw changed()
    return navigator.locks.request(profileKey('enrollment-offer-writer'), async () => {
      if (session.current !== active || identityRef.current !== own) throw changed()
      const previous = savedManagerEnrollment(active)
      let flow: ManagerEnrollment
      if (previous && !previous.published) {
        const checked = checkEnrollmentOffer(previous.offer, previous.reference)
        if (previous.reference.endpoint !== selected.address || previous.reference.relay !== selected.relay || !same(checked.rights, rights)) throw new Error('原邀请的发布结果尚未核对。请保持原入口和权限后重试。')
        flow = previous
      } else {
        const offer = await withSessionAuthorization(active, own, handle => createClientEnrollmentOffer(ready.scope.account_id, selected.address, active.channel.publicKey, selected.relay, JSON.stringify(rights), certificateExpiry, handle))
        const checked = verifyDeviceEnrollmentOffer(offer, Date.now())
        try {
          flow = { offer, published: false, reference: parseEnrollmentReference({ version: 2, kind: 'vibeface-device-enrollment', endpoint: selected.address, node_public_key: active.channel.publicKey, relay: selected.relay,
            account_id: checked.accountId(), offer_hash: checked.offerHash(), expires_at_ms: checked.expiresAtMs() }) }
        } finally { checked.free() }
        store(managerEnrollmentKey(active), flow)
      }
      const existing = record(await callSession(active, 'identity', { method: 'enrollment_offer_get', params: { account_id: flow.reference.account_id, offer_hash: flow.reference.offer_hash } }))
      if (existing.status === 'not_found') {
        if (existing.account_id !== flow.reference.account_id || existing.offer_hash !== flow.reference.offer_hash) throw new Error('节点未确认原邀请不存在，不会重新发布。')
        const reply = record(await callSession(active, 'identity', { method: 'enrollment_offer_publish', params: { offer: JSON.parse(flow.offer) } }))
        if (reply.offer_hash !== flow.reference.offer_hash || !same(parseEnrollmentReference(reply.reference), flow.reference)) throw new Error('节点返回的邀请引用与本地签名邀请不同。')
      } else if (!same(existing.offer, JSON.parse(flow.offer))) throw new Error('节点返回的邀请与原签名不同。')
      const published = { ...flow, published: true }
      store(managerEnrollmentKey(active), published)
      managerEnrollment.current = published; setEnrollmentRequests([]); setManagerEnrollmentActive(true)
      return enrollmentHref(location.href, published.reference)
    })
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
    const active = current(), own = identityRef.current
    if (!own) throw changed()
    const offer = verifyDeviceEnrollmentOffer(flow.offer, Date.now())
    try {
      const intent = JSON.parse(await withSessionAuthorization(active, own, handle => verifyDeviceEnrollmentIntentForManager(handle, offer, flow.reference.offer_hash, JSON.stringify(request.unsignedIntent), request.newDevicePublicKey, Date.now())))
      if (!same(intent, request.unsignedIntent)) throw new Error('设备申请与原邀请不同。')
    } finally { offer.free() }
    return { flow, request }
  }
  async function approveEnrollmentRequest(requestId: string) {
    const active = current(), { flow, request } = await managerRequest(requestId)
    if (request.status !== 'requested') throw new Error('申请已不处于等待批准状态，请重新核对。')
    const own = identityRef.current
    if (!own) throw changed()
    const signed = JSON.parse(await withSessionAuthorization(active, own, handle => approveClientEnrollmentIntent(JSON.stringify(request.unsignedIntent), handle)))
    await callSession(active, 'identity', { method: 'enrollment_approve', params: { account_id: flow.reference.account_id, offer_hash: flow.reference.offer_hash, request_id: requestId, intent: signed } })
    await refreshEnrollmentRequests()
  }
  async function applyDeviceChange(prepared: ReturnType<typeof prepareClientDeviceRevocation>, active: Session, requestId?: string, revokedDeviceId?: string) {
    let verified: ReturnType<typeof finishAccountDeviceChange> | null = null
    try {
      const connection = ready, own = identityRef.current
      if (!connection?.isCurrent() || !own) throw changed()
      const reply = record(await callSession(active, 'identity', { method: 'account_device_commit', params: { request: JSON.parse(prepared.requestJson()), ...(requestId ? { request_id: requestId } : {}) } }))
      verified = finishAccountDeviceChange(prepared, JSON.stringify(record(reply.committed)), Date.now())
      const trust = parseAuthorizationTrust({ kind: 'policy', accountId: connection.scope.account_id, spaceId: connection.scope.space_id, pins: JSON.parse(verified.pinsJson()) })
      await persistPolicyTrust(active, trust, undefined, own)
      if (revokedDeviceId === connection.scope.device_id) { lockVault(); return }
      const auth = await readAuthentication(active, trust, { resourceId: connection.project.id, audience: connection.audience })
      await authenticate(auth, active, trust, connection.project.id, { resourceId: connection.project.id, audience: connection.audience })
      managerEnrollment.current = null; setManagerEnrollmentActive(false); setEnrollmentRequests([])
      store(managerEnrollmentKey(active), null)
      await refreshDirectory()
    } finally { verified?.free(); prepared.free() }
  }
  async function completeEnrollmentRequest(requestId: string) {
    const active = current(), { request } = await managerRequest(requestId)
    if (request.status !== 'possessed' || !request.intent || !request.certificate || !same(request.intent.payload, request.unsignedIntent)) throw new Error('新设备尚未提供与原申请一致的密钥持有证明。')
    const own = identityRef.current
    if (!own) throw changed()
    const prepared = await withSessionAuthorization(active, own, handle => {
      const qualified = verifyAccountEnrollment(JSON.stringify(request.intent), handle, Date.now())
      try { return prepareClientDeviceEnrollment(qualified, JSON.stringify(request.certificate), handle) }
      finally { qualified.free() }
    })
    await applyDeviceChange(prepared, active, requestId)
  }

  async function revokeDevice(deviceId: string) {
    if (!ready?.isCurrent()) throw changed()
    const active = current(), authorization = active.authorization
    if (!authorization?.enrollmentRights.includes('revoke_device')) throw new Error('当前设备没有撤销账户设备的权限。')
    const own = identityRef.current
    if (!own) throw changed()
    const prepared = await withSessionAuthorization(active, own, handle => prepareClientDeviceRevocation(ready.scope.account_id, deviceId, handle))
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
    let running = true, released = false
    const release = () => {
      if (released) return
      released = true; opened?.release(); opened = null; registration?.release(); registration = null; prepared?.free(); prepared = null; intentJson = ''; completion = null; input.password = ''
    }
    const finish = async (receipt: Record<string, unknown>, resumed = active) => {
      if (!prepared) throw new Error('原 Passkey 注册材料已释放，请重新连接核对。')
      const verified = finishFirstAuthenticatorRegistration(prepared, JSON.stringify(array(receipt.journal)), Date.now())
      let trust: AuthorizationTrust
      try { trust = parseAuthorizationTrust({ kind: 'policy', accountId: connection.scope.account_id, spaceId: connection.scope.space_id, pins: JSON.parse(verified.pinsJson()) }) }
      finally { verified.free() }
      await persistPolicyTrust(resumed, trust, undefined, own)
      controller.signal.throwIfAborted()
      const auth = await readAuthentication(resumed, trust, { resourceId: connection.project.id, audience: connection.audience }, controller.signal)
      await authenticate(auth, resumed, trust, connection.project.id, { resourceId: connection.project.id, audience: connection.audience }, controller.signal)
      if (input.protectLocal) await saveLocalPasskeyProtection(registration, controller.signal)
      if (sensitive.current === flow) cancelSensitive()
    }
    const retained = resumableAuthority(active, own, () => prepared !== null && completion !== null && !released && !controller.signal.aborted)
    const flow: Sensitive = {
      keepOnNetworkClose: retained.keepOnNetworkClose, retainForReconnect: retained.retainForReconnect,
      cancel() { controller.abort(); opened?.release(); opened = null; registration?.release(); registration = null; if (!running) release() },
      async retryPasskeyRegistration() {
        if (running || released) throw new Error('原 Passkey 注册请求仍在等待结果或已关闭。')
        if (!prepared || !completion || !intentJson) throw new Error('当前没有需要核对的原注册提交。')
        controller.signal.throwIfAborted()
        const resumed = retained.resume(); running = true
        try {
          const pending = record(await callSession(resumed, 'identity', { method: 'passkey_prepare_registration', params: { intent: JSON.parse(intentJson), origin: location.origin } }))
          if (pending.status === 'completed') await finish(record(pending.receipt), resumed)
          else {
            controller.signal.throwIfAborted()
            if (pending.status !== 'pending') throw new Error('节点未确认原注册的待处理状态。')
            const receipt = record(await callSession(resumed, 'identity', { method: 'passkey_complete_registration', params: { completion, origin: location.origin } }))
            await finish(receipt, resumed)
          }
        } finally { running = false; if (controller.signal.aborted) release() }
      },
    }
    sensitive.current = flow
    try {
      opened = await openAccountPackage(input.file, input.password)
      input.password = ''; controller.signal.throwIfAborted()
      if (opened.scope.account_id !== connection.scope.account_id || opened.entry.nodePublicKey !== active.channel.publicKey) throw new Error('恢复包与当前账户或可信 Node 不同。')
      const privateMaterial = opened.material, credentialExpiry = material.expiresAt
      intentJson = await withSessionAuthorization(active, own, handle => prepareClientFirstAuthenticatorIntent(privateMaterial, handle, credentialExpiry), controller.signal)
      const pending = record(await callSession(active, 'identity', { method: 'passkey_prepare_registration', params: { intent: JSON.parse(intentJson), origin: location.origin } }))
      controller.signal.throwIfAborted()
      if (pending.status === 'completed') throw new Error('节点记录此注册已完成。请重新连接核对账户 Passkey，不会重复登记。')
      if (pending.status !== 'pending') throw new Error('节点未确认原 Passkey 注册请求。')
      registration = await registerNodePasskey({ pendingResponse: pending, signedIntent: JSON.parse(intentJson), nodePublicKey: active.channel.publicKey, signal: controller.signal, requestPrf: input.protectLocal })
      controller.signal.throwIfAborted()
      if (!opened) throw changed()
      const privateMaterialForRegistration = opened.material, verifiedRegistration = registration.registration
      prepared = await withSessionAuthorization(active, own, handle => prepareClientFirstAuthenticatorRegistration(privateMaterialForRegistration, handle, intentJson, verifiedRegistration), controller.signal)
      opened.release(); opened = null
      completion = { intent: JSON.parse(intentJson), pending_id: registration.pendingId, response: registration.response, journal: JSON.parse(prepared.completionJournalJson()) }
      setFirstPasskeyPending(true)
      const receipt = record(await callSession(active, 'identity', { method: 'passkey_complete_registration', params: { completion, origin: location.origin } }))
      await finish(receipt)
    } catch (error) {
      if (!prepared || controller.signal.aborted) { if (sensitive.current === flow) cancelSensitive(); else flow.cancel() }
      throw error
    } finally { running = false; input.password = ''; opened?.release(); opened = null; if (controller.signal.aborted) release() }
  }
  async function resumePasskeyRegistration() {
    if (!sensitive.current?.retryPasskeyRegistration) throw new Error('当前没有待核对的账户 Passkey 注册。')
    await sensitive.current.retryPasskeyRegistration()
  }
  async function beginEnrollment(reference: EnrollmentReference) {
    if (savedSpaces().some(trust => trust.accountId !== reference.account_id)) throw new Error('请为另一个账户创建独立设备配置。')
    const own = identityRef.current
    if (!own) throw new Error('请先创建或解锁新设备配置。')
    cancelSensitive()
    let active = await open({ address: reference.endpoint, node_id: nodeFingerprint(reference.node_public_key), relay: reference.relay }, reference.node_public_key)
    const requestKey = profileKey('enrollment-request')
    const controller = new AbortController()
    let pending: Awaited<ReturnType<typeof requestDeviceEnrollment>> | null = null
    let passkey: {
      intentJson: string; offer: VerifiedEnrollmentOffer; qualified: ReturnType<typeof verifyPasskeyEnrollmentRequestFromOffer>
      prepared: ReturnType<typeof prepareClientPasskeyDeviceEnrollment> | null; completion: Record<string, unknown> | null
      assertion: Awaited<ReturnType<typeof assertNodePasskey>> | null
    } | null = null
    const update = (status: EnrollmentStatus) => setIncomingEnrollment({ accountId: reference.account_id, newDevicePublicKey: own.publicKey, requestedRights: pending?.rights ?? [], status })
    const resume = async () => {
      if (identityRef.current !== own || controller.signal.aborted) throw changed()
      active = session.current ?? await open(active.entry, reference.node_public_key)
      if (active.channel.publicKey !== reference.node_public_key) throw changed()
    }
    const flow: Sensitive = {
      keepOnNetworkClose: () => pending !== null && !controller.signal.aborted,
      retainForReconnect: entry => pending !== null && identityRef.current === own && entry.node_id === nodeFingerprint(reference.node_public_key),
      cancel() {
        controller.abort(); pending?.release(); pending = null
        passkey?.prepared?.free(); passkey?.qualified.free()
        passkey?.assertion?.localPrf?.fill(0); passkey?.assertion?.prfSalt?.fill(0); passkey = null
      },
      async passkeyEnrollment(protectLocal) {
        if (!pending || controller.signal.aborted) throw changed()
        await resume()
        if (protectLocal && !own.profileId) throw new Error('本机 Passkey 保护需要独立口令设备配置。临时会话只验证账户身份。')
        const original = pending
        if (!passkey) {
          const offer = original.offer
          if (!offer) throw new Error('原设备接入已经完成，请检查加入结果。')
          try {
            const intentJson = signClientPasskeyEnrollmentIntent(JSON.stringify(original.unsignedIntent), offer)
            const qualified = verifyPasskeyEnrollmentRequestFromOffer(intentJson, offer, Date.now())
            passkey = { intentJson, offer, qualified, prepared: null, completion: null, assertion: null }
          } catch (error) { throw error }
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
            attempt.assertion = await assertNodePasskey({ pendingResponse: preparedResponse, signedIntent: JSON.parse(attempt.intentJson), nodePublicKey: reference.node_public_key,
              offer: attempt.offer,
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
        const auth = await readAuthentication(active, trust)
        controller.signal.throwIfAborted()
        await authenticate(auth, active, trust, undefined, undefined, controller.signal)
        if (protectLocal) await saveLocalPasskeyProtection(attempt.assertion, controller.signal)
        store(requestKey, null)
        if (sensitive.current === flow) cancelSensitive()
      },
      async refreshEnrollment() {
        if (!pending || controller.signal.aborted) throw changed()
        await resume()
        const original = pending
        const response = await callSession(active, 'identity', { method: 'enrollment_status', params: { account_id: reference.account_id, offer_hash: reference.offer_hash, request_id: original.requestId } })
        controller.signal.throwIfAborted()
        const status = verifyEnrollmentStatus(response, original)
        if (status.status === 'requested') { update(status.status); return }
        if (status.status === 'completed') {
          if (!status.completion) throw new Error('节点未确认账户设备变更已提交。')
          const auth = await readAuthentication(active, original.trust, undefined, controller.signal)
          controller.signal.throwIfAborted()
          await authenticate(auth, active, original.trust, undefined, undefined, controller.signal)
          store(requestKey, null)
          if (sensitive.current === flow) cancelSensitive()
          return
        }
        if (!status.intent) throw new Error('节点未提供原设备签署的批准意图。')
        const offer = original.offer
        if (!offer) throw new Error('无法恢复未完成的设备邀请。请保留原申请并核对。')
        let qualified: ReturnType<typeof verifyAccountEnrollmentFromOffer> | null = null
        try {
          qualified = verifyAccountEnrollmentFromOffer(JSON.stringify(status.intent), offer, Date.now())
          const certificate = JSON.parse(signClientEnrollmentPossession(qualified, original.nonce))
          if (status.status === 'approved') {
            await callSession(active, 'identity', { method: 'enrollment_possession', params: { account_id: reference.account_id, offer_hash: reference.offer_hash, request_id: original.requestId, certificate } })
            controller.signal.throwIfAborted(); update('possessed'); return
          }
          if (!status.certificate || !same(status.certificate, certificate)) throw new Error('接入记录中的持有证明与此设备原证明不同。')
          update(status.status)
        } finally { qualified?.free() }
      },
    }
    sensitive.current = flow
    try {
      if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能安全保存设备申请。')
      pending = await navigator.locks.request(profileKey('enrollment-request-writer'), () => requestDeviceEnrollment({
        reference, ownPublicKey: own.publicKey, call: (method, params) => callSession(active, 'identity', { method, params }), signal: controller.signal,
        saved: load(requestKey, true), save(request) {
          controller.signal.throwIfAborted()
          if (identityRef.current !== own || session.current !== active) throw changed()
          store(requestKey, request)
        },
      }))
      controller.signal.throwIfAborted()
      await persistPolicyTrust(active, pending.trust, controller.signal)
      saveEntry(active.entry); store(profileKey('last-entry'), active.entry); store(profileKey('last-space'), pending.trust.spaceId)
      controller.signal.throwIfAborted(); update(pending.status.status)
      if (pending.status.status === 'completed') await flow.refreshEnrollment?.()
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
      const auth = await readAuthentication(active, trust)
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
        const selection = ownerProjectSelection(created.authorization, created.serviceGrant.projectId, active.channel.publicKey)
        if (!('locks' in navigator)) throw new Error('此浏览器需要支持 Web Locks 才能保存账户信任。')
        await navigator.locks.request(profileKey('policy-trust-writer'), () => {
          if (controller.signal.aborted || session.current !== active) throw changed()
          const trust = mergeAuthorizationTrust(created.trust, savedSpaces())
          store(profileKey('policy-trusts'), [trust]); store(profileKey('last-space'), trust.spaceId); setSpaces([trust])
          saveEntry(active.entry); store(profileKey('last-entry'), active.entry)
          store(targetKey(created.trust.spaceId, active.entry), selection)
        })
        if (controller.signal.aborted || session.current !== active) throw changed()
        submitted = true
        try {
          await callSession(active, 'identity', { method: 'bootstrap', params: { pins: created.trust.pins, authorization: created.authorization } })
          if (controller.signal.aborted) throw new Error('初始化已提交，但本地确认已取消。请重新连接核对账户状态。')
          const auth = await readAuthentication(active, created.trust, selection, controller.signal)
          await authenticate(auth, active, created.trust, selection.resourceId, selection, controller.signal)
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
  function persistRecoveryLinks(own: PublicIdentity, sourceEntry: ConnectionEntry, targetEntry: ConnectionEntry, result: CrossNodeRecoveryResult, expiresAt: number) {
    const profile = storageProfile(own), temporary = own.profileId === null
    if (temporary && identityRef.current !== own) throw changed()
    const readOriginal = (key: string) => temporary ? temporaryRecords.current.get(key) : readPersistent(key, true)
    const writeOriginal = (key: string, value: unknown) => { if (temporary) temporaryRecords.current.set(key, JSON.parse(JSON.stringify(value))); else writePersistent(key, value) }
    const links = result.sourceReadSelections.map(item => ({ accountId: result.trust.accountId, spaceId: item.project.space, projectId: item.project.resource, entry: sourceEntry, nodePublicKey: item.targetPublicKey, expiresAt }))
    const entries = savedEntries(profile, readOriginal)
    const nextEntries = [...entries.filter(item => entryKey(item) !== entryKey(sourceEntry) && entryKey(item) !== entryKey(targetEntry)), sourceEntry, targetEntry]
    writeOriginal(`profile.${profile}.connections`, nextEntries)
    writeOriginal(`profile.${profile}.recovery-read-projects`, links)
    for (const item of result.sourceReadSelections) {
      const selection = { resourceId: item.project.resource, audience: { device: deriveAuthorizationId('device', item.targetPublicKey), service: deriveAuthorizationId('service', item.targetPublicKey) }, grantLeaf: item.grantLeaf }
      writeOriginal(`profile.${profile}.target.${JSON.stringify([item.project.space, entryKey(sourceEntry)])}`, selection)
    }
    if (identityRef.current === own) { setEntries(nextEntries); setRecoveryReadProjects(links) }
  }
  async function beginCrossRecovery(input: RecoveryInput) {
    if (savedSpaces().length) throw new Error('账户恢复需要新的独立设备配置。请先锁定并创建新配置。')
    const own = identityRef.current
    if (!own) throw new Error('请先解锁新的独立设备配置。')
    const target = parseConnectionCard(readInvitation(input.targetConnectionCard ?? ''))
    if (target.kind !== 'claim') throw new Error('跨 Node 恢复需要目标 Node 本机生成的认领卡。')
    const source = input.connectionCard.trim() ? parseConnectionCard(readInvitation(input.connectionCard)) : null
    if (source?.entry.relay) throw new Error('原 Node 需要直接入口，公钥将与加密恢复包核对。')
    const projects = input.sourceReadProjectIds ?? [], expiresAt = input.sourceReadExpiresAt
    if (!projects.length || !Number.isSafeInteger(expiresAt) || !expiresAt || expiresAt <= Date.now()) throw new Error('请明确选择原 Project，并设置晚于当前时刻的只读期限。')
    disconnect()
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof prepareCrossNodeRecovery>> | null = null
    installCrossRecoveryFlow(controller, own, () => prepared)
    const flow = sensitive.current
    try {
      prepared = await prepareCrossNodeRecovery({ file: input.file, password: input.password, newPassword: input.password, card: target,
        sourceAddress: source?.entry.address, sourceReadProjectIds: projects, sourceReadExpiresAt: expiresAt,
        certificateExpiresAt: Date.now() + 365 * 24 * 60 * 60 * 1000, signal: controller.signal })
      if (source && source.nodePublicKey !== prepared.sourceNodePublicKey) { prepared.release(); prepared = null; throw new Error('原 Node 连接卡的公钥与恢复包不一致。') }
      controller.signal.throwIfAborted()
      if (identityRef.current !== own || sensitive.current !== flow) throw changed()
      setCrossRecoveryPending(true)
      await flow?.crossPrepare?.()
    } catch (error) {
      if (!prepared || controller.signal.aborted) { if (sensitive.current === flow) cancelSensitive(); else flow?.cancel() }
      throw error
    } finally { input.password = ''; target.capability = ''; target.claimNonce = '' }
  }
  async function restorePendingCrossRecovery(input: { file: File; password: string }) {
    const own = identityRef.current
    if (!own) throw new Error('请先解锁原恢复设备配置。')
    disconnect()
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof restoreCrossNodeRecovery>> | null = null
    installCrossRecoveryFlow(controller, own, () => prepared)
    const flow = sensitive.current
    try {
      prepared = await restoreCrossNodeRecovery({ ...input, signal: controller.signal })
      controller.signal.throwIfAborted()
      if (identityRef.current !== own || sensitive.current !== flow) throw changed()
      const accountId = prepared.accountId
      if (savedSpaces().some(trust => trust.accountId !== accountId)) throw new Error('待恢复请求属于另一账户，请使用原恢复设备配置。')
      setCrossRecoveryPending(true)
      await flow?.crossPrepare?.()
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow?.cancel(); throw error }
    finally { input.password = '' }
  }
  function installCrossRecoveryFlow(controller: AbortController, own: PublicIdentity, pending: () => Awaited<ReturnType<typeof prepareCrossNodeRecovery>> | null) {
    let downloaded: Uint8Array | null = null
    let result: CrossNodeRecoveryResult | null = null
    const check = () => { controller.signal.throwIfAborted(); if (identityRef.current !== own) throw changed() }
    const update = () => {
      const prepared = pending(), review = prepared?.publicReview
      if (!prepared || !review || sensitive.current !== flow || controller.signal.aborted || identityRef.current !== own) return
      setReview({ kind: 'recovery', accountId: review.accountId, spaceId: review.spaceId, serviceGrant: review.serviceGrant, targetChanged: true,
        sourceRead: review.sourceRead, recoveryStage: prepared.phase === 'installed' ? 'installed' : prepared.committed ? 'committed' : prepared.submitted ? 'submitted' : 'prepared' })
    }
    const flow: Sensitive = {
      keepOnNetworkClose: () => !!pending() && !controller.signal.aborted,
      retainForReconnect: entry => !!pending() && identityRef.current === own && entry.node_id === pending()!.targetEntry.node_id,
      cancel() { controller.abort(); pending()?.release(); downloaded = null },
      download() { check(); const prepared = pending(); if (!prepared) throw changed(); const bytes = prepared.bytes; downloadPackage(bytes, prepared.accountId); downloaded = bytes },
      async crossPrepare() { check(); const prepared = pending(); if (!prepared) throw changed(); await prepared.prepare(); check(); update() },
      async recover(saved) {
        check(); const prepared = pending(), review = prepared?.publicReview
        if (!prepared || !review) throw new Error('请先完成原跨 Node 恢复准备。')
        if (!saved || downloaded !== prepared.bytes) throw new Error('请先下载当前恢复包，并确认已妥善保存。')
        if (prepared.phase !== 'installed') {
          try {
            result = await prepared.commit(true, async next => {
              result = next
              await persistPolicyTrust(null, next.trust, undefined, own)
              persistRecoveryLinks(own, prepared.sourceEntry, prepared.targetEntry, next, review.sourceRead.expiresAt)
            })
          } finally { update() }
          return
        }
        if (!result) throw new Error('请先核对原恢复回执和当前政策。')
        const active = await open(prepared.targetEntry, prepared.targetNodePublicKey)
        const authorization = await readAuthentication(active, result.trust, result.selection, controller.signal)
        await authenticate(authorization, active, result.trust, result.selection.resourceId, result.selection, controller.signal)
        if (sensitive.current === flow) cancelSensitive()
      },
    }
    sensitive.current = flow
  }
  async function resumeCrossRecoveryPreparation() {
    if (!sensitive.current?.crossPrepare) throw new Error('当前没有待核对的跨 Node 恢复准备。')
    await sensitive.current.crossPrepare()
  }
  function installAccountRecoveryFlow(controller: AbortController, own: PublicIdentity, pending: () => Awaited<ReturnType<typeof recoverAccount>> | null) {
    let downloaded: Uint8Array | null = null, running = false, installed = false
    const check = () => { controller.signal.throwIfAborted(); if (identityRef.current !== own || sensitive.current !== flow) throw changed() }
    const update = () => {
      const prepared = pending()
      if (!prepared || identityRef.current !== own || sensitive.current !== flow || controller.signal.aborted) return
      setReview({ kind: 'recovery', accountId: prepared.scope.account_id, spaceId: prepared.scope.space_id, serviceGrant: prepared.serviceGrant, targetChanged: false,
        recoveryStage: installed ? 'installed' : prepared.committed ? 'committed' : prepared.submitted ? 'submitted' : 'prepared' })
    }
    const flow: Sensitive = {
      keepOnNetworkClose: () => !!pending() && !controller.signal.aborted,
      retainForReconnect: entry => !!pending() && identityRef.current === own && entry.node_id === nodeFingerprint(pending()!.entry.nodePublicKey),
      cancel() { controller.abort(); pending()?.release(); downloaded = null },
      download() { check(); const prepared = pending(); if (!prepared) throw changed(); const bytes = prepared.bytes; downloadPackage(bytes, prepared.scope.account_id); downloaded = bytes },
      async recover(saved) {
        check()
        if (running) throw new Error('原账户恢复仍在核对结果，请等待当前请求。')
        const prepared = pending()
        if (!prepared || !saved || downloaded !== prepared.bytes) throw new Error('请先下载当前恢复包，并确认已妥善保存。')
        if (installed) { cancelSensitive(); return }
        running = true
        try {
          let active = session.current
          if (!active) active = await open({ address: prepared.entry.endpoint, node_id: nodeFingerprint(prepared.entry.nodePublicKey), relay: false }, prepared.entry.nodePublicKey)
          check()
          if (active.channel.publicKey !== prepared.entry.nodePublicKey) throw new Error('当前连接与原恢复 Node 的独立公钥不同。')
          try { await prepared.commit(true) } finally { update() }
          check()
          let candidate: VerifiedClientAuthorization | null = await readAuthentication(active, prepared.trust, prepared.selection, controller.signal)
          let confirmed: VerifiedClientAuthorization | null = null
          try {
            check(); confirmed = prepared.phase === 'confirmed' ? candidate : prepared.confirm(candidate)
            if (confirmed === candidate) candidate = null
            candidate?.free(); candidate = null
            const adopted = confirmed; confirmed = null
            await authenticate(adopted, active, prepared.trust, prepared.selection.resourceId, prepared.selection, controller.signal)
          } finally { candidate?.free(); confirmed?.free() }
          check(); installed = true
        } finally { running = false; update() }
      },
    }
    sensitive.current = flow
    return { update, call: async (method: string, params: Record<string, unknown>) => {
      check(); const prepared = pending(), active = current()
      if (prepared && active.channel.publicKey !== prepared.entry.nodePublicKey) throw new Error('请连接原恢复 Node 后核对同一请求。')
      return callSession(active, 'identity', { method, params })
    } }
  }
  async function restorePendingAccountRecovery(input: { file: File; password: string }) {
    const own = identityRef.current
    if (!own) throw new Error('请先解锁原恢复设备配置。')
    disconnect()
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof recoverAccount>> | null = null
    const installed = installAccountRecoveryFlow(controller, own, () => prepared), flow = sensitive.current
    try {
      prepared = await restoreAccountRecovery({ ...input, signal: controller.signal, call: installed.call })
      controller.signal.throwIfAborted()
      if (savedSpaces().some(trust => trust.accountId !== prepared!.trust.accountId)) throw new Error('待恢复请求属于另一账户，请使用原恢复设备配置。')
      await open({ address: prepared.entry.endpoint, node_id: nodeFingerprint(prepared.entry.nodePublicKey), relay: false }, prepared.entry.nodePublicKey)
      installed.update()
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow?.cancel(); throw error }
    finally { input.password = '' }
  }
  async function beginRecovery(input: RecoveryInput) {
    if (input.targetConnectionCard?.trim()) return beginCrossRecovery(input)
    if (savedSpaces().length) throw new Error('账户恢复需要新的独立设备配置。请先锁定并创建新配置。')
    const own = identityRef.current
    if (!own) throw new Error('请先解锁新的独立设备配置。')
    const version = generation.current
    let password = input.password; input.password = ''
    const opened = await openAccountPackage(input.file, password).catch(error => { password = ''; throw error })
    if (version !== generation.current) { opened.release(); password = ''; throw changed() }
    let card: ConnectionCard | null = null, active: Session
    try {
      card = input.connectionCard.trim() ? parseConnectionCard(readInvitation(input.connectionCard)) : null
      const publicKey = card?.nodePublicKey ?? opened.entry.nodePublicKey
      if (publicKey !== opened.entry.nodePublicKey) throw new Error('请选择跨 Node 恢复，并明确选择原项目只读范围。')
      const target = card?.entry ?? { address: opened.entry.endpoint, node_id: nodeFingerprint(publicKey), relay: false }
      active = await open(target, publicKey)
    } catch (error) { opened.release(); password = ''; throw error }
    const controller = new AbortController()
    let prepared: Awaited<ReturnType<typeof recoverAccount>> | null = null
    const installed = installAccountRecoveryFlow(controller, own, () => prepared), flow = sensitive.current
    try {
      if (!active.info.initialized) throw new Error('原 Node 尚未初始化。请使用跨 Node 恢复和目标 Node 的可信认领卡。')
      const requestJson = prepareClientRecoveryContext(opened.material, opened.entry.nodePublicKey)
      const request = record(JSON.parse(requestJson))
      const response = record(await callSession(active, 'identity', { method: 'recovery_context', params: { request } }))
      controller.signal.throwIfAborted()
      prepared = await recoverAccount({ material: opened.material, requestJson, snapshot: response.snapshot,
        nodePublicKey: opened.entry.nodePublicKey, endpoint: active.entry.address, password, signal: controller.signal, call: installed.call })
      controller.signal.throwIfAborted(); installed.update()
    } catch (error) { if (sensitive.current === flow) cancelSensitive(); else flow?.cancel(); throw error }
    finally { opened.release(); password = ''; if (card?.kind === 'claim') { card.capability = ''; card.claimNonce = '' } }
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
    setEntries(savedEntries(storageProfile(own), load)); setSpaces(savedSpaces()); setRecoveryReadProjects(readRecoveryReadProjects(load(profileKey('recovery-read-projects'))))
    const pendingEnrollment = load(profileKey('enrollment-request'), true)
    if (pendingEnrollment) { await beginEnrollment(parseSavedEnrollmentRequest(pendingEnrollment).reference); return }
    if (!initialInvitation) {
      const lastEntry = load(profileKey('last-entry'), true)
      if (lastEntry) await connect(parseEntry(lastEntry))
    }
  }
  function lockVault() {
    disconnect(); lockClient(); temporaryRecords.current.clear(); identityRef.current = null; setIdentity(null); setEntries([]); setSpaces([]); setRecoveryReadProjects([])
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
    const invitation = fragment.get('enroll') || fragment.get('bootstrap') || fragment.get('connect') || fragment.get('invite') || ''
    if (location.hash) { const clean = new URL(location.href); clean.hash = ''; history.replaceState(null, '', clean) }
    if (invitation) setInitialInvitation(invitation)
    clientVaultStatus().then(status => {
      if (mounted) { setVaults(status.profiles); setPhase(status.status === 'locked' ? 'vault_locked' : status.status === 'key_lost' ? 'key_lost' : status.status === 'unavailable' ? 'vault_unavailable' : 'vault_empty'); if (status.status === 'unavailable') setError(status.error) }
    }).catch(cause => { if (mounted) { setPhase('blocked'); setError(messageOf(cause)) } })
    return () => { mounted = false; generation.current += 1; lockClient(); temporaryRecords.current.clear(); sensitive.current?.cancel(); sensitive.current = null; connecting.current?.abort(); for (const controller of independentReads.current) controller.abort(); independentReads.current.clear(); const previous = session.current; session.current = null; releaseSession(previous); if (previous?.businessRoute.kind === 'gateway') previous.businessRoute.channel?.close(); previous?.channel.close(); listeners.current.clear(); for (const authorization of executionAuthorizations.current) authorization.free(); executionAuthorizations.current.clear() }
  }, [])
  const value: DeviceContextValue = {
    identity, vaults, spaces, ready, entries, phase, error, entry, info, directory, review, initialInvitation, ownerGatewayRoute,
    exportGatewayRequest, approveGatewayRoute, retryGatewayRoute, connectGatewayCard, disconnectGateway, connect, selectSpace, selectProject, disconnect, saveEntry, removeEntry, initializeVault, lockVault, backupVault, useConnectionCard, refreshDirectory, createEnrollmentOffer, beginRecovery, restorePendingAccountRecovery, reencrypt,
    incomingEnrollment, enrollmentRequests, managerEnrollmentActive, activeEnrollmentLink: managerEnrollment.current?.published ? enrollmentHref(location.href, managerEnrollment.current.reference) : '', localProtectionNotice, governancePending, retryGovernance, createTeam, restoreTeamCreation, changeMembership, grantProject, revokeProjectGrant, teamCreationReview, confirmTeamCreation, nodeEnrollmentReview, beginNodeEnrollment, confirmNodeEnrollment, refreshIncomingEnrollment, refreshEnrollmentRequests, approveEnrollmentRequest, completeEnrollmentRequest, revokeDevice, joinWithPasskey,
    firstPasskeyPending, registerAccountPasskey, resumePasskeyRegistration, crossRecoveryPending, recoveryReadProjects, resumeCrossRecoveryPreparation, restorePendingCrossRecovery,
    registerExecutionService, latestExecutionServiceResource: currentExecutionService()?.resourceId ?? '', downloadExecutionServiceBackup, executionServiceBackupAvailable: currentExecutionServiceBackup() !== null,
    executionServiceOptions: executionServiceResources.filter(matchesExecutionService).map(item => ({ spaceId: item.spaceId, resourceId: item.resourceId })), refreshExecutionServices, selectExecutionService,
    createRemoteExecutionShare, inspectRemoteExecutionRequest, latestRemoteExecutionShare: remoteExecutionShare && currentExecutionService()?.resourceId === remoteExecutionShare.execution.resourceId ? remoteExecutionShare.value : '',
    downloadBackup() { if (!sensitive.current?.download) throw new Error('当前没有待下载的恢复包。'); sensitive.current.download() },
    async confirmBootstrap(saved) { if (!sensitive.current?.bootstrap) throw new Error('初始化确认已失效。'); await sensitive.current.bootstrap(saved) },
    async confirmRecovery(saved) { if (!sensitive.current?.recover) throw new Error('恢复确认已失效。'); await sensitive.current.recover(saved) },
    cancelSensitive,
  }
  return <DeviceContext.Provider value={value}>{children}</DeviceContext.Provider>
}
export function useDevice() { const value = useContext(DeviceContext); if (!value) throw new Error('DeviceProvider 缺失。'); return value }
