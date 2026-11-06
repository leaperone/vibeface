import type { CoreLifecycle } from './lifecycle.js'
import { parseLocalListen } from './access-url.js'

export type DesktopConnectionMode = 'none' | 'attach' | 'owned'

export class AppState {
  profileDir: string
  dataDir: string | null = null
  businessOrigin: string | null = null
  deviceId: string | null = null

  constructor(profileDir: string) {
    this.profileDir = profileDir
  }

  connectionMode(lifecycle: CoreLifecycle): DesktopConnectionMode {
    const s = lifecycle.getState().kind
    if (s === 'attached') return 'attach'
    if (s === 'owned_child') return 'owned'
    return 'none'
  }

  setBusinessFromInspect(inspect: { local_listen?: string; device_id?: string }) {
    if (typeof inspect.device_id === 'string') this.deviceId = inspect.device_id
    const ll = inspect.local_listen
    if (typeof ll !== 'string') return
    const parsed = parseLocalListen(ll)
    if (!parsed) return
    const isV6 = parsed.host.includes(':')
    const host = isV6 ? `[${parsed.host}]` : parsed.host
    this.businessOrigin = `http://${host}:${parsed.port}`
  }
}
