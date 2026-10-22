import { boolean, entryKey, parseEntry, record, text } from './node'
import type { ConnectionEntry, PublicIdentity } from './node'

type LocalLaunch = { token: string; deviceId: string }

async function localResponse(response: Response): Promise<unknown> {
  if (!response.ok) throw new Error('本机链接已过期。请从这台电脑上重新打开 Vibeface。')
  return response.json()
}

export async function localBrowserEntry(own: PublicIdentity, launch: LocalLaunch | null): Promise<ConnectionEntry | null> {
  let response: Response
  try { response = await fetch('/api/local/info', { credentials: 'same-origin', cache: 'no-store', redirect: 'error' }) }
  catch {
    if (launch) throw new Error('连不上这台电脑。请确认 Vibeface 正在运行。')
    return null
  }
  // Only the launcher origin serves this; elsewhere the caller sends a launch on to it.
  if (response.status === 404 || !response.ok && !launch) return null
  const info = record(await localResponse(response))
  const entry = parseEntry(info.entry)
  if (entry.local !== true || entry.address !== location.origin) throw new Error('本机链接和当前网页对不上。')
  if (!launch) return boolean(info.enabled) && boolean(info.has_session) && info.device_id === own.deviceId ? entry : null
  if (!/^[a-f0-9]{64}$/.test(launch.token) || entry.device_id !== launch.deviceId) throw new Error('本机启动信息与目标设备不一致。')
  const exchanged = record(await localResponse(await fetch('/api/local/exchange', {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', redirect: 'error',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token: launch.token, node_id: launch.deviceId, device_id: own.deviceId }),
  })))
  const authorized = parseEntry(exchanged.entry)
  if (entryKey(authorized) !== entryKey(entry) || text(exchanged.network_id) !== info.network_id) throw new Error('兑换后的本机会话与原设备或网络不一致。')
  return authorized
}
