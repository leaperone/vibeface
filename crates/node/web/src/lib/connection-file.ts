import { connectDevice, readInvitation } from '../../transport.js'
import { checkEnrollmentOffer } from './enrollment'
import { parseEnrollmentReference } from './enrollment-reference'
import { parseEntry, record, text } from './node'
import type { ConnectionCard, ConnectionEntry, GatewayConnectionCard } from './node'
import { nodeFingerprint, parseNodeConnectionCard } from '../../generated/vibeface_secure.js'

const maxBytes = 64 * 1024

type ConnectionPreview = { content: string; entry: ConnectionEntry; nodePublicKey: string }
export type ConnectionPackage = ConnectionPreview & (
  | { kind: 'connection' | 'claim' }
  | { kind: 'enrollment'; accountId: string; rights: string[]; expiresAt: number }
)

export async function readConnectionFile(file: File): Promise<string> {
  if (!file.size || file.size > maxBytes) throw new Error('请选择不超过 64 KiB 的连接或邀请文件。')
  return file.text()
}

export async function inspectConnectionPackage(content: string, signal?: AbortSignal): Promise<ConnectionPackage> {
  signal?.throwIfAborted()
  if (new TextEncoder().encode(content).length > maxBytes) throw new Error('连接或邀请内容超过 64 KiB。')
  const value = readInvitation(content)
  if (record(value).kind !== 'vibeface-device-enrollment') {
    const card = parseConnectionCard(value)
    return { content: JSON.stringify(value), kind: card.kind, entry: card.entry, nodePublicKey: card.nodePublicKey }
  }
  const reference = parseEnrollmentReference(value)
  const entry = { address: reference.endpoint, node_id: nodeFingerprint(reference.node_public_key), relay: reference.relay }
  const channel = await connectDevice(entry, { signal })
  try {
    if (channel.publicKey !== reference.node_public_key) throw new Error('邀请目标与实际节点公钥不同。')
    const response = record(await channel.request('identity', { method: 'enrollment_offer_get', params: { account_id: reference.account_id, offer_hash: reference.offer_hash } }))
    signal?.throwIfAborted()
    const checked = checkEnrollmentOffer(JSON.stringify(response.offer), reference)
    return { content: JSON.stringify(reference), kind: 'enrollment', entry, nodePublicKey: reference.node_public_key,
      accountId: reference.account_id, rights: checked.rights, expiresAt: reference.expires_at_ms }
  } finally { channel.close() }
}

export function downloadConnectionFile(content: string, filename: string): void {
  const bytes = new TextEncoder().encode(content)
  if (bytes.length > maxBytes) throw new Error('连接或邀请内容超过 64 KiB。')
  const url = URL.createObjectURL(new Blob([bytes], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = filename
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}

export function parseConnectionCard(value: unknown): ConnectionCard {
  const card = record(JSON.parse(parseNodeConnectionCard(JSON.stringify(value))), '连接卡')
  const entry = parseEntry({ address: card.url, node_id: card.node_id, relay: card.relay })
  const nodePublicKey = text(card.node_public_key)
  if (card.capability === undefined) return { kind: 'connection', entry, nodePublicKey }
  return { kind: 'claim', entry, nodePublicKey, capability: text(card.capability), claimNonce: text(card.claim_nonce) }
}

export function parseGatewayConnectionCard(value: unknown): GatewayConnectionCard {
  const card = record(value, '网关连接卡')
  if (card.kind !== 'vibeface-gateway' || card.version !== 2 || Object.keys(card).some(key => !['kind', 'version', 'gateway', 'approval'].includes(key))) throw new Error('请使用包含已批准路由的网关连接卡。')
  const gateway = parseConnectionCard(card.gateway)
  if (gateway.kind !== 'connection') throw new Error('网关连接卡不能包含设备认领凭证。')
  return { kind: 'vibeface-gateway', version: 2, gateway, approval: record(card.approval, '网关路由批准') }
}
