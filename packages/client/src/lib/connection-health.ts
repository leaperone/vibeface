export type ConnectionEvidence = { disconnects: number; readStartedAt: number | null; now: number; timedOut: boolean }

/** Surface only repeated failures or a read that has taken long enough to matter. */
export function connectionHealth(evidence: ConnectionEvidence): 'quiet' | 'recovering' {
  if (evidence.timedOut || evidence.disconnects >= 2) return 'recovering'
  if (evidence.readStartedAt !== null && evidence.now - evidence.readStartedAt >= 8000) return 'recovering'
  return 'quiet'
}
