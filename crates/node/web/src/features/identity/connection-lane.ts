export function shouldAutoConnect(input: { hasInvitation: boolean; savedCount: number }): boolean {
  return !input.hasInvitation && input.savedCount === 1
}

export function primaryLane(input: { hasInvitation: boolean; savedCount: number }): 'invite' | 'saved' {
  if (input.hasInvitation || input.savedCount === 0) return 'invite'
  return 'saved'
}
