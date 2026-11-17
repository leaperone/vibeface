import { useEffect, useRef } from 'react'
import { selectionOwner, type DisposableSelection } from '@vibeface/client/lib/selection.ts'

/** An effect lifetime owns selections. Replay and network replacement always get a fresh owner. */
export function useSelectionOwner<File extends DisposableSelection>(scope: string | undefined, reset: () => void, onError: (error: unknown) => void) {
  // Losing a ready connection during reconnect does not abandon a still-visible draft.
  const network = useRef(scope)
  if (scope !== undefined) network.current = scope
  const ownedScope = network.current
  const latest = useRef({ reset, onError })
  latest.current = { reset, onError }
  const selection = useRef<ReturnType<typeof selectionOwner<File>> | null>(null)
  useEffect(() => {
    let active = true
    const owner = selectionOwner<File>((error) => {
      if (active) latest.current.onError(error)
      else console.error('选择副本清理失败，副本保留待清除。', error)
    })
    selection.current = owner
    latest.current.reset()
    return () => { active = false; if (selection.current === owner) selection.current = null; owner.close() }
  }, [ownedScope])
  return selection
}
