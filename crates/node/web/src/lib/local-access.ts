import { boolean, parseConnectionCard, record, text } from './node'
import { validTarget } from '../../transport.js'

export type LocalAccessStatus = { enabled: boolean; authenticated: boolean; nodeId: string }
export type LocalAccessSession = { enabled: boolean; nodeId: string; unlockKey: string | null; connectionCard: string }

async function localRequest(path: string, body: Record<string, unknown>): Promise<Record<string, unknown>> {
  const response = await fetch(`/api/local/${path}`, { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body), cache: 'no-store', redirect: 'error' })
  if (!response.ok) throw new Error('本机访问凭据已失效。请在本机运行 vibeface open。')
  return record(await response.json(), '本机访问响应')
}

export async function readLocalAccessStatus(): Promise<LocalAccessStatus | null> {
  const response = await fetch('/api/local/status', { credentials: 'same-origin', cache: 'no-store', redirect: 'error' })
  if (response.status === 404) return null
  if (!response.ok) throw new Error('无法读取本机访问设置。')
  const value = record(await response.json())
  return { enabled: boolean(value.enabled), authenticated: boolean(value.authenticated), nodeId: validTarget(value.node_id) }
}

export async function exchangeLocalAccess(token: string): Promise<void> {
  await localRequest('exchange', { token: validTarget(token) })
}

export async function readLocalAccessSession(): Promise<LocalAccessSession> {
  const value = await localRequest('session', {})
  const enabled = boolean(value.enabled)
  const nodeId = validTarget(value.node_id)
  const card = parseConnectionCard(value.connection_card)
  if (card.entry.node_id !== nodeId || card.entry.address !== location.origin || card.entry.relay) throw new Error('本机连接卡不属于当前节点。')
  return { enabled, nodeId, unlockKey: enabled ? validTarget(text(value.unlock_key)) : null, connectionCard: JSON.stringify(value.connection_card) }
}

export async function setLocalAccessEnabled(enabled: boolean): Promise<void> {
  await localRequest('settings', { enabled })
}
