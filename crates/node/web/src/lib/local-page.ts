import { parseLocalSetup } from '@vibeface/client/lib/node.ts'
import type { LocalSetup } from '@vibeface/client/lib/node.ts'
import type { LocalPage } from '@vibeface/client/lib/platform.ts'
import { localBrowserEntry } from './local-access'

/** Only a page on localhost may set up, launch or join the computer serving it. */
export const localPage: LocalPage = {
  get origin() { return location.origin },
  takeLaunch() {
    const fragment = new URLSearchParams(location.hash.slice(1))
    const invitation = location.pathname === '/join' && location.hash.startsWith('#vibeface1') ? location.href : ''
    const localToken = fragment.get('local')
    const localLaunch = localToken ? { token: localToken, deviceId: fragment.get('node') ?? '', rejoin: fragment.get('rejoin') === '1' } : null
    if (location.hash) { const clean = new URL(location.href); clean.hash = ''; history.replaceState(null, '', clean) }
    return { invitation, localLaunch }
  },
  async discoverLocalSetup(signal): Promise<LocalSetup | null> {
    const url = new URL(location.href)
    if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) return null
    let response: Response
    // The Gateway serving this origin may be down while the shell comes from the service worker.
    try { response = await fetch('/api/local/setup', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}', cache: 'no-store', credentials: 'omit', signal }) }
    catch (cause) { if (signal.aborted) throw cause; return null }
    if (response.status === 404) return null
    if (!response.ok) throw new Error('无法读取这台电脑的本机设置。请检查本机地址并刷新页面。')
    return parseLocalSetup(await response.json(), url.origin)
  },
  localEntry: localBrowserEntry,
  open(href) {
    const url = new URL(href)
    location.assign(url.href)
    // Same page with only a new hash does not reload, and the launch token is read on mount.
    if (url.origin === location.origin && url.pathname === location.pathname) location.reload()
  },
}
