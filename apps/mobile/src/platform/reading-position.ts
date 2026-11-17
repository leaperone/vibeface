import type { KeyValue } from '@vibeface/client/lib/platform.ts'

export type ReadingPosition = { kind: 'latest' } | { kind: 'message'; messageId: string }
type SavedPosition = { conversationId: string; position: ReadingPosition }

function position(value: unknown): ReadingPosition | null {
  if (!value || typeof value !== 'object' || !('kind' in value)) return null
  if (value.kind === 'latest') return { kind: 'latest' }
  if (value.kind === 'message' && 'messageId' in value && typeof value.messageId === 'string' && value.messageId.length > 0 && value.messageId.length <= 512) return { kind: 'message', messageId: value.messageId }
  return null
}

export function readingPositions(storage: KeyValue, identity: { networkId: string; deviceId: string }) {
  const key = `vibeface.reading-positions.${JSON.stringify([identity.networkId, identity.deviceId])}`
  const read = (): SavedPosition[] => {
    try {
      const raw = storage.getItem(key)
      if (!raw || raw.length > 128 * 1024) return []
      const value: unknown = JSON.parse(raw)
      if (!Array.isArray(value)) return []
      return value.flatMap((item: unknown): SavedPosition[] => {
        if (!item || typeof item !== 'object' || !('conversationId' in item) || !('position' in item) || typeof item.conversationId !== 'string') return []
        const parsed = position(item.position)
        return parsed && item.conversationId.length <= 512 ? [{ conversationId: item.conversationId, position: parsed }] : []
      }).slice(-64)
    } catch { return [] }
  }
  return {
    get(conversationId: string): ReadingPosition | null { return read().findLast((item) => item.conversationId === conversationId)?.position ?? null },
    save(conversationId: string, value: ReadingPosition) {
      if (!identity.networkId || !identity.deviceId || !conversationId || conversationId.length > 512 || !position(value)) return
      const records = [...read().filter((item) => item.conversationId !== conversationId), { conversationId, position: value }].slice(-64)
      try { storage.setItem(key, JSON.stringify(records)) } catch { /* Reading remains available when optional local persistence fails. */ }
    },
    clear() { storage.setItem(key, '[]') },
  }
}
