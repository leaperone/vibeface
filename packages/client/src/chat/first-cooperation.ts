import { useEffect, useRef, useState } from 'react'
import type { KeyValue } from '../lib/platform.ts'
import { networkKey, storageOf } from '../workspace/storage.ts'

export const cooperationExamples = [
  { label: '需要选择时再问我', text: '能继续的先做，需要我选择、补充信息或确认结果时告诉我。' },
  { label: '先给结论和下一步', text: '先给结论，再说下一步，需要时附上出处。' },
] as const

/** Remember only that this device has shown the optional guide, never a permission or a sent message. */
export function useFirstCooperation(storage: KeyValue, scope: { networkId: string; deviceId: string; botId: string }, visible: boolean) {
  const key = networkKey('first-cooperation.' + scope.deviceId + '.' + scope.botId, scope.networkId)
  const [visit, setVisit] = useState<{ key: string; show: boolean } | null>(null)
  const displayed = useRef<{ key: string; show: boolean } | null>(null)
  useEffect(() => {
    if (!visible || !scope.networkId || !scope.deviceId || !scope.botId) { displayed.current = null; setVisit(null); return }
    const local = storageOf(storage), show = displayed.current?.key === key ? displayed.current.show : local.readStorage(key, false) !== true
    displayed.current = { key, show }; setVisit({ key, show })
    if (show) local.writeStorage(key, true)
  }, [key, visible, storage])
  return {
    show: visible && visit?.key === key && visit.show,
    dismiss: () => { displayed.current = { key, show: false }; setVisit({ key, show: false }) },
  }
}
