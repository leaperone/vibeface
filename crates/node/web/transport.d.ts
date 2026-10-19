export type DeviceChannel = {
  peerId: string
  publicKey: string
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  close(): void
}
export function initClient(options?: { replaceLostKey?: boolean }): Promise<{ peerId: string; publicKey: string }>
export function connectDevice(entry: { address: string; peer_id: string }, options?: {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
}): Promise<DeviceChannel>
export function validTarget(value: unknown): string
export function validEndpoint(value: string): string
export function readInvitation(value: string): unknown
export function signClient(domain: string, payload: string): string
