import type { LocalPage } from '@vibeface/client/lib/platform.ts'
import type { LanStream, NativeLocalGatewayCarrier } from '@vibeface/client/lib/transport.ts'
import { joinTicket } from '@vibeface/client/lib/transport.ts'
import type { ClientPreloadApi } from '../../../desktop/src/shared/client-contract.ts'

const invitationIn = () => {
  if (location.pathname !== '/join') return ''
  const invitation = new URLSearchParams(location.hash.slice(1)).get('invitation') ?? ''
  return joinTicket(invitation) ? invitation : ''
}
const DEVICE_ID = /^[a-f0-9]{64}$/

/** The native broker decides whether a Gateway belongs to the selected local Node. */
export function desktopGatewayCarrier(
  native: Pick<ClientPreloadApi['carrier'], 'openGateway'>,
  stream: (id: string) => LanStream,
): NativeLocalGatewayCarrier {
  return async (entry, target, timeoutMs) => {
    const id = await native.openGateway(target, entry.address, timeoutMs)
    return id === null ? null : stream(id)
  }
}

export function desktopLocalPage(native: ClientPreloadApi, rememberLocalOrigin: (origin: string) => void = () => {}): LocalPage {
  return {
    get origin() { return location.origin },
    takeLaunch() {
      const fragment = new URLSearchParams(location.hash.slice(1))
      const invitation = invitationIn()
      const token = fragment.get('local')
      const deviceId = fragment.get('node')
      const localLaunch = token && deviceId && DEVICE_ID.test(deviceId)
        ? { token, deviceId, rejoin: fragment.get('rejoin') === '1' }
        : null
      if (location.hash) {
        const clean = new URL(location.href)
        clean.hash = ''
        history.replaceState(null, '', clean)
      }
      return { invitation, localLaunch }
    },
    watchInvitation(listener) {
      const changed = () => { if (invitationIn()) listener(desktopLocalPage(native).takeLaunch().invitation) }
      addEventListener('hashchange', changed)
      return () => removeEventListener('hashchange', changed)
    },
    async discoverLocalSetup(signal) {
      if (signal.aborted) throw new DOMException('Aborted', 'AbortError')
      const pending = native.local.discoverLocalSetup()
      const setup = await new Promise<Awaited<typeof pending>>((resolve, reject) => {
        const abort = () => reject(new DOMException('Aborted', 'AbortError'))
        signal.addEventListener('abort', abort, { once: true })
        void pending.then(resolve, reject).finally(() => signal.removeEventListener('abort', abort))
      })
      if (setup) rememberLocalOrigin(setup.entry.address)
      return setup
    },
    localEntry: (identity, launch) => native.local.localEntry(identity.deviceId, launch),
    open(raw) {
      const local = new URL(raw)
      if (local.protocol !== 'http:' || !['localhost', '127.0.0.1', '[::1]'].includes(local.hostname)
          || local.username || local.password || local.pathname !== '/' || local.search) throw new Error('Invalid local launch')
      const params = new URLSearchParams(local.hash.slice(1))
      if (!params.has('local') || !DEVICE_ID.test(params.get('node') ?? '')) throw new Error('Invalid local launch')
      const destination = new URL(location.href)
      destination.hash = local.hash
      location.assign(destination.href)
      if (destination.origin === location.origin && destination.pathname === location.pathname) location.reload()
    },
  }
}
