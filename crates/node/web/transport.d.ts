import type { Identity, PreparedAccountCreation, PreparedAccountRecovery, PreparedAccountDeviceChange, PreparedFirstAuthenticatorRegistration, VerifiedAccountEnrollment, VerifiedClientAuthorization, VerifiedEnrollmentOffer, VerifiedNodeClaim, VerifiedPasskeyEnrollmentRequest, VerifiedPasskeyRegistration, VerifiedRemoteExecution, prepareAccountNodeEnrollment, prepareTeamCreation, prepareSpaceMembershipChange, prepareDirectionalGrant, prepareSpaceGrantRevocation, buildCrossNodeRecoveryRequest, prepareCrossNodeAccountRecovery, restorePreparedCrossNodeRecoveryCommit, prepareExecutionServiceRegistration, prepareRemoteExecutionShare } from './generated/vibeface_secure.js'

export type DeviceChannel = {
  nodeId: string
  publicKey: string
  request(method: string, params?: Record<string, unknown>, invocation?: unknown): Promise<unknown>
  close(): void
}
export type ClientVaultOptions = { mode: 'unlock'; profileId: string; password: string } | { mode: 'unlock_prf'; profileId: string; prf: Uint8Array } | { mode: 'create'; name: string; password: string } | { mode: 'temporary' } | { mode: 'restore'; password: string; vault: unknown }
export type VaultProfile = { id: string; name: string; passkey: boolean }
export function initClient(options: ClientVaultOptions): Promise<{ nodeId: string; publicKey: string; profileId: string | null }>
export function clientVaultStatus(): Promise<{ status: 'locked' | 'empty' | 'key_lost'; profiles: VaultProfile[] } | { status: 'unavailable'; profiles: VaultProfile[]; error: string }>
export function lockClient(): void
export function exportDeviceVault(): Promise<string>
export function sealDeviceVault(secret: string, password: string): Promise<unknown>
export function openDeviceVault(value: unknown, password: string): Promise<string>
export function protectClientVaultWithPrf(options: { credentialId: string; salt: Uint8Array; prf: Uint8Array }, signal?: AbortSignal): Promise<void>
export function passkeyVaultUnlockOptions(profileId: string): Promise<{ credentialId: string; options: unknown; salt: Uint8Array<ArrayBuffer> }>
export function connectDevice(entry: { address: string; node_id: string; relay: boolean }, options?: {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
}): Promise<DeviceChannel>
export function validTarget(value: unknown): string
export function validEndpoint(value: string): string
export function readInvitation(value: string): unknown
export function signAuthorizedClientInvocation(authorization: VerifiedClientAuthorization, templateJson: string, bodyJson: string, expectedAction: string, operationId: string, gatewayPublicKey?: string): string
export function signAuthorizedClientPrepare(authorization: VerifiedClientAuthorization, bodyJson: string, contextJson: string, expectedAction: string, operationId: string, gatewayPublicKey: string): string
export function prepareRemoteClientExecution(remote: VerifiedRemoteExecution, authorization: VerifiedClientAuthorization, bodyJson: string, contextJson: string, expectedAction: string, operationId: string, gatewayPublicKey: string): string
export function signRemoteClientExecution(remote: VerifiedRemoteExecution, authorization: VerifiedClientAuthorization, templateJson: string, bodyJson: string, expectedAction: string, operationId: string, gatewayPublicKey: string): string
export function buildClientCrossNodeRecoveryRequest(materialJson: string, nextRoot: Identity, claim: VerifiedNodeClaim, resourceJson: string, readExpiresAt: number, requestExpiresAt: number): ReturnType<typeof buildCrossNodeRecoveryRequest>
export function prepareClientCrossNodeAccountRecovery(materialJson: string, context: Parameters<typeof prepareCrossNodeAccountRecovery>[1], nextRoot: Identity, certificateExpiresAt: number): ReturnType<typeof prepareCrossNodeAccountRecovery>
export function prepareClientAccountCreation(root: Identity, recovery: Identity, claim: VerifiedNodeClaim, certificateExpiresAt: number): PreparedAccountCreation
export function prepareClientAccountRecovery(materialJson: string, context: Parameters<typeof import('./generated/vibeface_secure.js').prepareAccountRecovery>[1], nextRoot: Identity, certificateExpiresAt: number): PreparedAccountRecovery
export function buildClientAccountRecoveryPending(prepared: PreparedAccountRecovery, created: Parameters<typeof import('./generated/vibeface_secure.js').buildAccountRecoveryPending>[1], nextRoot: Identity, previousMaterialJson: string, endpoint: string): ReturnType<typeof import('./generated/vibeface_secure.js').buildAccountRecoveryPending>
export function restoreClientAccountRecovery(pendingJson: string): ReturnType<typeof import('./generated/vibeface_secure.js').restoreAccountRecoveryPending>
export function prepareClientRecoveryContext(materialJson: string, expectedNodePublicKey: string): string
export function createClientEnrollmentOffer(accountId: string, endpoint: string, nodePublicKey: string, requestedRightsJson: string, certificateExpiresAt: number, authorization: VerifiedClientAuthorization): string
export function createClientEnrollmentIntent(offer: VerifiedEnrollmentOffer): string
export function approveClientEnrollmentIntent(intentJson: string, authorization: VerifiedClientAuthorization): string
export function signClientEnrollmentPossession(qualified: VerifiedAccountEnrollment, pendingNonce: string): string
export function prepareClientDeviceEnrollment(qualified: VerifiedAccountEnrollment, possessionJson: string, authorization: VerifiedClientAuthorization): PreparedAccountDeviceChange
export function prepareClientDeviceRevocation(accountId: string, deviceId: string, authorization: VerifiedClientAuthorization): PreparedAccountDeviceChange
export function signClientPasskeyEnrollmentIntent(intentJson: string, offer: VerifiedEnrollmentOffer): string
export function signClientPasskeyEnrollmentPossession(qualified: VerifiedPasskeyEnrollmentRequest, pendingNonce: string): string
export function prepareClientPasskeyDeviceEnrollment(intentJson: string, certificateJson: string, proofJson: string, offer: VerifiedEnrollmentOffer): PreparedAccountDeviceChange
export function prepareClientFirstAuthenticatorIntent(materialJson: string, authorization: VerifiedClientAuthorization, credentialExpiresAt: number): string
export function prepareClientFirstAuthenticatorRegistration(materialJson: string, authorization: VerifiedClientAuthorization, intentJson: string, registration: VerifiedPasskeyRegistration): PreparedFirstAuthenticatorRegistration

export function prepareClientAccountNodeEnrollment(materialJson: string, authorization: VerifiedClientAuthorization, claim: VerifiedNodeClaim, certificateExpiresAt: number): ReturnType<typeof prepareAccountNodeEnrollment>
export function prepareClientTeamCreation(root: Identity, ownerAccountId: string, targetNodePublicKey: string, recoveryPublicKey: string, sourceProjectResourceId: string, authorization: VerifiedClientAuthorization, certificateExpiresAt: number): ReturnType<typeof prepareTeamCreation>
export function prepareClientMembershipChange(intentJson: string, authorization: VerifiedClientAuthorization): ReturnType<typeof prepareSpaceMembershipChange>
export function prepareClientDirectionalGrant(intentJson: string, authorization: VerifiedClientAuthorization): ReturnType<typeof prepareDirectionalGrant>
export function prepareClientGrantRevocation(spaceId: string, grantHash: string, authorization: VerifiedClientAuthorization): ReturnType<typeof prepareSpaceGrantRevocation>
export function buildClientPolicyReadRequest(account: string, space: string, nodePublicKey: string, pinsJson: string, selectionJson: string, expiresAt: number): string
export function beginClientPolicyImport(pinsJson: string, requestJson: string, snapshotJson: string, nodePublicKey: string): ReturnType<typeof import('./generated/vibeface_secure.js').beginClientPolicyImport>
export function verifyClientPolicySelections(pinsJson: string, requestJson: string, pageJson: string, nodePublicKey: string): string
export function buildClientPolicyAuthConfirmRequest(authorization: VerifiedClientAuthorization, expiresAt: number): string
export function prepareClientExecutionServiceRegistration(source: Parameters<typeof prepareExecutionServiceRegistration>[0], management: Parameters<typeof prepareExecutionServiceRegistration>[1], owner: Parameters<typeof prepareExecutionServiceRegistration>[2], ownerMaterialJson: string, targetNodePublicKey: string, expiresAt: number): ReturnType<typeof prepareExecutionServiceRegistration>

export function restoreClientTeamCreation(materialJson: string, authorization: VerifiedClientAuthorization): ReturnType<typeof prepareTeamCreation>

export function restoreClientCrossNodeRecovery(materialJson: string): ReturnType<typeof restorePreparedCrossNodeRecoveryCommit>

export function buildClientRemoteShareIntent(intentJson: string, source: VerifiedClientAuthorization): string
export function buildClientRemoteSharePolicyReadRequest(intentJson: string, pinsJson: string, nodePublicKey: string, expiresAt: number): string
export function prepareClientRemoteExecutionShare(intentJson: string, authorization: VerifiedClientAuthorization, source: Parameters<typeof prepareRemoteExecutionShare>[3]): ReturnType<typeof prepareRemoteExecutionShare>
export function signRemoteExecutionObservation(remote: VerifiedRemoteExecution, originalJson: string, authorizationId: string, pinnedGatewayPublicKey: string): string

export function prepareClientAccountNodeEnrollmentStatus(prepared: Parameters<typeof import('./generated/vibeface_secure.js').prepareAccountNodeEnrollmentStatus>[0], expiresAt: number): ReturnType<typeof import('./generated/vibeface_secure.js').prepareAccountNodeEnrollmentStatus>
export function buildClientEnrollmentPolicyReadRequest(offer: Parameters<typeof import('./generated/vibeface_secure.js').buildEnrollmentOfferPolicyReadRequest>[0], expiresAt: number): string
export function beginClientEnrollmentOfferImport(offer: Parameters<typeof import('./generated/vibeface_secure.js').beginEnrollmentOfferImport>[0], requestJson: string, snapshotJson: string, nodePublicKey: string): ReturnType<typeof import('./generated/vibeface_secure.js').beginEnrollmentOfferImport>

export function buildClientRemoteExecutionPolicyReadIntent(source: VerifiedClientAuthorization, inputJson: string, expiresAt: number): string

export function buildClientRemoteExecutionHistoryPolicyReadIntent(source: VerifiedClientAuthorization, inputJson: string, independentPinsJson: string, expiresAt: number): string
