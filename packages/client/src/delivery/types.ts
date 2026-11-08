export type DeliveryMode = 'charity' | 'paid'
export type Acceptance = 'recoverable' | 'strict'
export type DeliveryPhase = 'prepared' | 'funding_pending' | 'proving' | 'funded' | 'transferring' | 'cipher_held' | 'key_published' | 'recoverable' | 'paid' | 'partial' | 'refundable' | 'refunded' | 'reconciling' | 'key_unavailable' | 'failed'
export type DeliveryEvidence = 'none' | 'charity_receipt' | 'ciphertext_held' | 'recoverable' | 'decryption_proven'
export type PaymentState = 'free' | 'prepared' | 'submitted' | 'included' | 'confirmed' | 'earned' | 'withdrawn' | 'refunded' | 'unknown' | 'failed'
export type DeliverySettings = { enabled: boolean; charity_enabled: boolean; paid_enabled: boolean }
export type DeliveryStatus = DeliverySettings & {
  node_peer: string
  wallet: { state: 'absent'; address: null } | { state: 'locked' | 'unlocked'; address: string } | { state: 'unavailable'; address: null; reason: string }
  max_file_bytes: number
  receiver_key: { key_id: string; serial: number; public_key: string } | null
}
export type DeliveryPayment = {
  state: PaymentState
  chain_id: string | null
  asset: string | null
  payer: string | null
  provider_wallet: string | null
  delivery_fee: string
  reservation_fee: string
  priority_fee: string
  total: string
  gas_cap: string
  earned: string
  withdrawn: string
  refunded: string
  tx_hash: string | null
  budget_id: string | null
  settlement: { earned: string; withdrawn: string; refunded: string; publication: { state: 'private_ready' | 'disclosed_outstanding' | 'final_earned' | 'expired_unrevealed'; total_amount: string; economic_cutoff: number; availability_risk: string | null } | null } | null
}
export type DeliveryBudget = {
  budget_id: string
  payer: string
  payee: string
  provider_peer: string
  asset: string
  deposit: string
  free: string
  locked: string
  earned: string
  withdrawn: string
  refunded: string
  state: 'open' | 'closing' | 'closed'
  gas_cap: string
  ready_windows: Array<{ order_id: string; index: number }>
}
export type DeliveryWindow = {
  index: number
  bytes: number
  committed_bytes: number
  state: 'pending' | 'stored' | 'held' | 'key_published' | 'earned' | 'refunded'
  decryption_proven: boolean
}
export type DeliveryDeadlines = {
  quote_expires_at: number
  hold_deadline: number
  key_deadline: number
  open_deadline: number
  retain_until: number
  priority_deadline: number
}
export type DeliveryOrder = {
  order_id: string
  intent_id: string
  role: 'sender' | 'receiver' | 'provider'
  mode: DeliveryMode
  acceptance: Acceptance
  source_peer: string
  receiver_peer: string
  provider_peer: string
  name: string | null
  bytes: number
  sha256: string | null
  phase: DeliveryPhase
  evidence: DeliveryEvidence
  payment: DeliveryPayment
  deadlines: DeliveryDeadlines | null
  windows: DeliveryWindow[]
  created_at_ms: number
  updated_at_ms: number
  error: string | null
}
export type DeliveryDraft = {
  receiver_peer: string
  provider_peer: string
  mode: DeliveryMode
  acceptance: Acceptance
  max_service_amount: string
  max_gas_amount: string
  budget_id: string | null
}
export type TransferFile = { name: string; size: number; arrayBuffer(): Promise<ArrayBuffer> }
export type ReceivedFile = { name: string; content_base64: string; sha256: string; bytes: number }
export type PendingIntent = { intent_id: string; fingerprint: string }
