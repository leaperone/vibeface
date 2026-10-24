import { array, record } from '../lib/node.ts'

/** Per network in this browser: the recovery key it last accepted, and the key it is itself handing recovery to. */
export type RecoveryMarks = { seen: string | null; own: string | null }

/**
 * What a verified network state means for this browser. `notice`: since the key it accepted, a recovery file took the
 * network over (a checkpoint replaced the device list). A first sight, a change this browser made and a plain new
 * recovery password are adopted quietly.
 */
export function recoveryCheck(state: unknown, networkId: string, current: string, marks: RecoveryMarks): 'same' | 'adopt' | 'notice' {
  if (marks.seen === current) return 'same'
  if (!marks.seen || marks.own === current) return 'adopt'
  const checkpoints = array(record(state).checkpoints).map(item => record(item))
  const keys = [networkId, ...checkpoints.map(item => item.recovery_key)]
  const from = Math.max(keys.lastIndexOf(marks.seen), marks.own ? keys.lastIndexOf(marks.own) : -1)
  if (from < 0) return 'adopt'
  return checkpoints.slice(from).some(item => item.directory != null) ? 'notice' : 'adopt'
}
