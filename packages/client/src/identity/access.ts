/**
 * What the workspace routes may show for this browser. `resuming` holds saved credentials that are still
 * connecting, so the page stays put; only `auth` sends the browser to the connect page.
 */
export type Access =
  | { kind: 'member' }
  | { kind: 'loading' }
  | { kind: 'resuming'; retryInSeconds: number | null }
  | { kind: 'auth' }

type DeviceState = {
  ready: boolean
  phase: string
  joining: boolean
  savedEntries: number
  invitation: boolean
  offlineRetryMs: number | null
}

export function accessOf(device: DeviceState): Access {
  if (device.ready) return { kind: 'member' }
  if (device.phase === 'reconnecting' && !device.joining) return { kind: 'resuming', retryInSeconds: device.offlineRetryMs === null ? null : Math.ceil(device.offlineRetryMs / 1000) }
  if (device.phase === 'initializing' || device.phase === 'connecting' && device.savedEntries > 0 && !device.invitation) return { kind: 'loading' }
  return { kind: 'auth' }
}

/** A same-origin path to come back to after connecting; anything else falls back to the home page. */
export function returnPath(value: unknown): string {
  return typeof value === 'string' && value.startsWith('/') && !value.startsWith('//') && !value.startsWith('/\\') && !value.startsWith('/connect') ? value : '/'
}
