/** Purpose-specific client window contract shared with its isolated preload. */
export const CLIENT_SCHEME = 'vibeface-app'
export const CLIENT_HOST = 'app'
export const CLIENT_ORIGIN = `${CLIENT_SCHEME}://${CLIENT_HOST}`
export const CLIENT_URL = `${CLIENT_ORIGIN}/`

export const CLIENT_IPC = {
  localSetup: 'client:local-setup',
  localEntry: 'client:local-entry',
  carrierOpen: 'client:carrier-open',
  gatewayOpen: 'client:gateway-open',
  lanOpen: 'client:lan-open',
  carrierSend: 'client:carrier-send',
  carrierReceive: 'client:carrier-receive',
  carrierClose: 'client:carrier-close',
  showNodePanel: 'client:show-node-panel',
} as const

export type ClientLocalLaunch = { token: string; deviceId: string; rejoin: boolean }
export type ClientLocalSetup =
  | { kind: 'uninitialized'; entry: { kind: 'gateway'; address: string; device_id: string }; capability: string }
  | { kind: 'initialized'; entry: { kind: 'gateway'; address: string; device_id: string }; removed: boolean; local_launch?: { url: string; device_id: string; token: string } }
export type ClientLocalEntry = { kind: 'local'; address: string; device_id: string }
export type ClientFrame = { text: boolean; data: Uint8Array }
export type ClientPreloadApi = {
  local: {
    discoverLocalSetup(): Promise<ClientLocalSetup | null>
    localEntry(deviceId: string, launch: ClientLocalLaunch | null): Promise<ClientLocalEntry | null>
  }
  carrier: {
    open(deviceId: string, timeoutMs: number): Promise<string>
    openGateway(deviceId: string, address: string, timeoutMs: number): Promise<string | null>
    openLan(deviceId: string, addrs: string[], timeoutMs: number): Promise<string>
    send(id: string, bytes: Uint8Array): Promise<void>
    receive(id: string): Promise<ClientFrame | null>
    close(id: string): void
  }
  showNodePanel(): Promise<void>
}
