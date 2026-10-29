export function shouldAutoConnect(input: { hasInvitation: boolean; savedCount: number }): boolean {
  return !input.hasInvitation && input.savedCount === 1
}

/**
 * `local`: what this computer's own page offers. `open` sets it up or reopens it for a browser already in; `join` is
 * a computer in use that this browser is not part of, so it joins with a link like anywhere else.
 * `removed`: this browser was removed, so its saved addresses only refuse it again and the local page only opens a
 * session that cannot send; a new join link brings it back.
 */
export function primaryLane(input: { hasInvitation: boolean; savedCount: number; local: 'none' | 'open' | 'join'; rejoined?: boolean; removed?: boolean }): 'local' | 'invite' | 'saved' {
  if (input.hasInvitation || input.rejoined || input.removed || input.local === 'join') return 'invite'
  if (input.local === 'open') return 'local'
  if (input.savedCount === 0) return 'invite'
  return 'saved'
}
