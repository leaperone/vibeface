import { boolean, entryKey, parseEntry, record, text } from './node'
import type { ConnectionEntry, PublicIdentity } from './node'

type LocalLaunch = { token: string; peerId: string }

async function localResponse(response: Response): Promise<unknown> {
  if (!response.ok) throw new Error('本机入口已过期或已关闭。请返回节点原来的本机设置页刷新入口，或从本机启动器重新打开。')
  return response.json()
}

export async function localBrowserEntry(own: PublicIdentity, launch: LocalLaunch | null): Promise<ConnectionEntry | null> {
  let response: Response
  try { response = await fetch('/api/local/info', { credentials: 'same-origin', cache: 'no-store', redirect: 'error' }) }
  catch {
    if (launch) throw new Error('无法连接本机启动入口。请确认这台电脑的 Node 正在运行。')
    return null
  }
  if (!response.ok && !launch) return null
  const info = record(await localResponse(response))
  const entry = parseEntry(info.entry)
  if (entry.local !== true || entry.relay || entry.address !== location.origin) throw new Error('本机入口与当前网页不一致。')
  if (!launch) return boolean(info.enabled) && boolean(info.has_session) && info.subject_id === own.peerId ? entry : null
  if (!/^[a-f0-9]{64}$/.test(launch.token) || entry.peer_id !== launch.peerId) throw new Error('本机启动信息与目标设备不一致。')
  const exchanged = record(await localResponse(await fetch('/api/local/exchange', {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', redirect: 'error',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token: launch.token, peer_id: launch.peerId, subject_id: own.peerId, public_key: own.publicKey }),
  })))
  const authorized = parseEntry(exchanged.entry)
  if (entryKey(authorized) !== entryKey(entry) || text(exchanged.network_id) !== info.network_id) throw new Error('兑换后的本机会话与原设备或网络不一致。')
  return authorized
}
