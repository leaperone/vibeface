export type DeliveryMode = 'charity' | 'paid'
export type Acceptance = 'recoverable' | 'strict'
export type DeliveryPhase = 'prepared' | 'funding_pending' | 'proving' | 'funded' | 'transferring' | 'cipher_held' | 'key_published' | 'recoverable' | 'paid' | 'partial' | 'refunded' | 'reconciling' | 'key_unavailable' | 'failed'
export type DeliveryEvidence = 'none' | 'charity_receipt' | 'ciphertext_held' | 'recoverable' | 'decryption_proven'
export type PaymentState = 'free' | 'prepared' | 'confirmed' | 'earned' | 'withdrawn' | 'refunded' | 'unknown' | 'failed'
export type DeliverySettings = { enabled: boolean; charity_enabled: boolean; paid_enabled: boolean }
/** What a paid provider asks, as it announces it; `price` is per window in the asset's smallest unit. */
/** Where an offer settles: the delivery chain's ID, escrow and asset. */
export type Settlement = { chain_id: number; escrow: string; asset: string }
export type DeliveryOffer = { settlement: Settlement; price: string }
/** A device outside this one that announces it relays deliveries; `offer` only with paid delivery on. */
export type DeliveryProvider = { device_id: string; paid: boolean; offer: DeliveryOffer | null; last_seen_ms: number }
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
}
export type TransferFile = {
  name: string
  size: number
  arrayBuffer(): Promise<ArrayBuffer>
  /** Releases only an app-owned selection copy, never the person's original file. */
  dispose?(): void
}
export type ReceivedFile = { name: string; content_base64: string; sha256: string; bytes: number }
export type PendingIntent = { intent_id: string; fingerprint: string }

/** A selection owns its private copy; an operation holds it until its promise settles. */
export function transferSelection() {
  let selected: TransferFile | null = null
  let closed = false
  const uses = new Map<TransferFile, number>()
  const retired = new Set<TransferFile>()
  const release = (file: TransferFile) => {
    if ((uses.get(file) ?? 0) > 0) { retired.add(file); return }
    retired.delete(file)
    file.dispose?.()
  }
  return {
    replace(file: TransferFile | null) {
      if (closed) { file?.dispose?.(); return false }
      const previous = selected
      selected = file
      if (previous && previous !== file) release(previous)
      return true
    },
    async use<T>(operation: (file: TransferFile) => Promise<T>): Promise<T> {
      const file = selected
      if (closed || !file) throw new Error('请先选择文件。')
      uses.set(file, (uses.get(file) ?? 0) + 1)
      try { return await operation(file) }
      finally {
        const count = (uses.get(file) ?? 1) - 1
        if (count) uses.set(file, count)
        else { uses.delete(file); if (retired.has(file)) release(file) }
      }
    },
    close() {
      closed = true
      const previous = selected
      selected = null
      if (previous) release(previous)
    },
  }
}
