import type { DesktopNodeApi } from '../../../desktop/src/shared/contract.ts'

export function desktopNodeApi(): DesktopNodeApi | null {
  return typeof window !== 'undefined' ? window.vibefaceClient?.desktop ?? null : null
}

export function isDesktopHost(): boolean {
  return desktopNodeApi() !== null
}
