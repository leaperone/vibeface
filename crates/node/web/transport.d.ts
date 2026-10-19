export type DeviceChannel = {
  peerId: string
  publicKey: string
  request(method: string, params?: Record<string, unknown>): Promise<unknown>
  close(): void
}
export function initClient(options?: { replaceLostKey?: boolean }): Promise<{ peerId: string; publicKey: string }>
type ChannelOptions = {
  onEvent?: (event: unknown) => void
  onClose?: (error: unknown) => void
  signal?: AbortSignal
}
export function connectDevice(entry: { address: string; peer_id: string; local?: boolean }, options?: ChannelOptions): Promise<DeviceChannel>
export type ClusterPath = { address: string; mode: 'direct' | 'relay' }
export function connectClusterPath(path: ClusterPath & { peer_id: string }, options?: ChannelOptions): Promise<DeviceChannel>
export type ClusterCandidate = {
  peerId: string
  publicKey: string
  expiresAt: number
  paths: ClusterPath[]
}
export function discoverCluster(target: { clusterId: string; directories: string[] }, options?: { signal?: AbortSignal; trustedPeers?: string[]; attempt?: number }): Promise<ClusterCandidate[]>
export function validTarget(value: unknown): string
export function validEndpoint(value: string): string
export function readInvitation(value: string): unknown
export function signClient(domain: string, payload: string): string
