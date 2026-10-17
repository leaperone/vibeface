import type { Identity, PreparedAccountCreation, PreparedAccountRecovery, PreparedAccountDeviceChange, PreparedFirstAuthenticatorRegistration, VerifiedAccountEnrollment, VerifiedClientAuthorization, VerifiedEnrollmentOffer, VerifiedNodeClaim, VerifiedPasskeyEnrollmentRequest, VerifiedPasskeyRegistration } from './generated/vibeface_secure.js'

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
export function prepareClientAccountCreation(root: Identity, recovery: Identity, claim: VerifiedNodeClaim, certificateExpiresAt: number): PreparedAccountCreation
export function prepareClientAccountRecovery(materialJson: string, latestBundleJson: string, nextRoot: Identity, claim: VerifiedNodeClaim, certificateExpiresAt: number): PreparedAccountRecovery
export function prepareClientRecoveryContext(materialJson: string, expectedNodePublicKey: string): string
export function createClientEnrollmentOffer(accountId: string, endpoint: string, nodePublicKey: string, requestedRightsJson: string, certificateExpiresAt: number, pinsJson: string, bundleJson: string): string
export function createClientEnrollmentIntent(offer: VerifiedEnrollmentOffer): string
export function approveClientEnrollmentIntent(intentJson: string, pinsJson: string, bundleJson: string): string
export function signClientEnrollmentPossession(qualified: VerifiedAccountEnrollment, pendingNonce: string): string
export function prepareClientDeviceEnrollment(qualified: VerifiedAccountEnrollment, possessionJson: string, pinsJson: string, bundleJson: string): PreparedAccountDeviceChange
export function prepareClientDeviceRevocation(accountId: string, deviceId: string, pinsJson: string, bundleJson: string): PreparedAccountDeviceChange
export function signClientPasskeyEnrollmentIntent(intentJson: string, offer: VerifiedEnrollmentOffer): string
export function signClientPasskeyEnrollmentPossession(qualified: VerifiedPasskeyEnrollmentRequest, pendingNonce: string): string
export function prepareClientPasskeyDeviceEnrollment(intentJson: string, certificateJson: string, proofJson: string, offer: VerifiedEnrollmentOffer): PreparedAccountDeviceChange
export function prepareClientFirstAuthenticatorIntent(materialJson: string, pinsJson: string, bundleJson: string, credentialExpiresAt: number): string
export function prepareClientFirstAuthenticatorRegistration(materialJson: string, pinsJson: string, bundleJson: string, intentJson: string, registration: VerifiedPasskeyRegistration): PreparedFirstAuthenticatorRegistration
