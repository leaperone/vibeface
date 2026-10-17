import type { VerifiedClientAuthorization } from './generated/vibeface_secure.js'

export type DeviceChannel = {
  nodeId: string
  publicKey: string
  request(method: string, params?: Record<string, unknown>, invocation?: unknown): Promise<unknown>
  close(): void
}
export type ClientVaultOptions = { mode: 'unlock'; profileId: string; password: string } | { mode: 'create'; name: string; password: string } | { mode: 'temporary' } | { mode: 'restore'; password: string; vault: unknown }
export type VaultProfile = { id: string; name: string }
export function initClient(options: ClientVaultOptions): Promise<{ nodeId: string; publicKey: string; profileId: string | null }>
export function clientVaultStatus(): Promise<{ status: 'locked' | 'empty' | 'key_lost'; profiles: VaultProfile[] } | { status: 'unavailable'; profiles: VaultProfile[]; error: string }>
export function lockClient(): void
export function exportDeviceVault(): Promise<string>
export function sealDeviceVault(secret: string, password: string): Promise<unknown>
export function openDeviceVault(value: unknown, password: string): Promise<string>
export function connectDevice(entry: { address: string; node_id: string; relay: boolean }, options?: {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
}): Promise<DeviceChannel>
export function validTarget(value: unknown): string
export function validEndpoint(value: string): string
export function readInvitation(value: string): unknown
export function signClient(domain: string, payload: string): string

export function signAuthorizedClientInvocation(authorization: VerifiedClientAuthorization, templateJson: string, bodyJson: string, expectedAction: string, operationId: string): string
