export function shouldAutoConnect(input: { hasInvitation: boolean; savedCount: number }): boolean {
  return !input.hasInvitation && input.savedCount === 1
}

export function primaryLane(input: { hasInvitation: boolean; savedCount: number; hasLocalSetup: boolean; rejoined?: boolean }): 'local' | 'invite' | 'saved' {
  if (input.hasInvitation || input.rejoined) return 'invite'
  if (input.hasLocalSetup) return 'local'
  if (input.savedCount === 0) return 'invite'
  return 'saved'
}
