import type { ReadyConnection } from '@vibeface/client/lib/node.ts'
import type { DeliveryDraft, DeliveryOrder, DeliveryStatus } from '@vibeface/client/delivery/types.ts'

// The independent-worker CLIENT-18 baseline's terms, bytes and full fingerprint.
export const source = '11'.repeat(32), receiver = '22'.repeat(32), provider = '33'.repeat(32)
export const oldId = '00000000-0000-4000-8000-000000000001'
export const content = new TextEncoder().encode('controlled CLIENT18 bytes')
export const wireBytes = 'Y29udHJvbGxlZCBDTElFTlQxOCBieXRlcw=='
export const fingerprint = '378d14d7b0568e3cfd0ef72df668721feb7e49e383852086bdc86efb7a397821'
export const intentKey = `vibeface.delivery.network.client.${source}.intent.${fingerprint}`
export const file = { name: 'client18.txt', size: content.length, arrayBuffer: async () => content.slice().buffer }
export const draft: DeliveryDraft = { receiver_peer: receiver, provider_peer: provider, mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0', budget_id: null }
export const status: DeliveryStatus = { enabled: true, charity_enabled: true, paid_enabled: false, node_peer: source, wallet: { state: 'absent', address: null }, max_file_bytes: 130816, receiver_key: null, session_relay_enabled: false, session_route: null }
export const responseLost = 'controlled response lost; no Core or network invoked'
export function connection(request: ReadyConnection['request'], network = 'network'): ReadyConnection {
  return { generation: 1, entry: { kind: 'gateway', address: 'http://127.0.0.1:4790', device_id: source }, principal: { source: 'membership', can_manage: true, network_id: network, device_id: 'client', epoch: 1 }, info: { device_id: source, name: 'own Node', version: 'test', initialized: true, network_id: network }, request, rpc: async () => { throw new Error('private RPC forbidden') }, subscribe: () => () => {}, isCurrent: () => true }
}
export function completed(digest: string): DeliveryOrder {
  return {
    order_id: '44'.repeat(32), intent_id: oldId, role: 'sender', mode: 'charity', acceptance: 'recoverable', source_peer: source, receiver_peer: receiver, provider_peer: provider,
    name: file.name, bytes: content.length, sha256: digest, phase: 'recoverable', evidence: 'charity_receipt',
    payment: { state: 'free', chain_id: null, asset: null, payer: null, provider_wallet: null, delivery_fee: '0', reservation_fee: '0', priority_fee: '0', total: '0', gas_cap: '0', earned: '0', withdrawn: '0', refunded: '0', tx_hash: null, budget_id: null, settlement: null },
    deadlines: null, windows: [{ index: 0, bytes: content.length + 128, committed_bytes: 0, state: 'held', decryption_proven: false }], created_at_ms: 1, updated_at_ms: 1, error: null,
  }
}
