export type DeviceChannel = {
  deviceId: string
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  close(): void
}
export type JoinLink = { origin: string; entryId: string; inviteId: string; inviteHash: string; secret: string }
export function initClient(options?: { replaceLostKey?: boolean }): Promise<{ deviceId: string }>
export function connectDevice(entry: { address: string; device_id: string; local?: boolean }, options?: {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
  /** Connect and handshake must finish within this; entrance switching uses 5 seconds. */
  timeoutMs?: number
}): Promise<DeviceChannel>
export function validTarget(value: unknown): string
export function validEndpoint(value: string): string
export function parseJoinLink(value: string): Promise<JoinLink>
export function signClient(domain: string, payload: string | Uint8Array): Promise<string>
