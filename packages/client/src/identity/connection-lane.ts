export function shouldAutoConnect(input: { hasInvitation: boolean; savedCount: number }): boolean {
  return !input.hasInvitation && input.savedCount === 1
}

/** `removed`: this browser was removed, so its saved addresses only refuse it again; a new join link brings it back. */
export function primaryLane(input: { hasInvitation: boolean; savedCount: number; hasLocalSetup: boolean; rejoined?: boolean; removed?: boolean }): 'local' | 'invite' | 'saved' {
  if (input.hasInvitation || input.rejoined) return 'invite'
  if (input.hasLocalSetup) return 'local'
  if (input.savedCount === 0 || input.removed) return 'invite'
  return 'saved'
}
