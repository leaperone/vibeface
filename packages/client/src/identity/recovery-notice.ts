/**
 * Recoveries a manager answered with 是我在找回 on this device: hidden until the app or page opens again, never stored,
 * so a wrong tap costs at most this one visit of a reminder while the recovery runs toward taking effect.
 */
const hidden = new Set<string>()

export const hideRecovery = (networkId: string, checkpoint: string) => { hidden.add(`${networkId}:${checkpoint}`) }
export const recoveryHidden = (networkId: string, checkpoint: string) => hidden.has(`${networkId}:${checkpoint}`)
