import { useEffect,useRef,useState } from "react"

type Feedback = { status: "idle" } | { status: "pending" | "copied" | "failed"; key: string }
type Owner = { scope: unknown; sequence: number; live: boolean; timer: ReturnType<typeof setTimeout> | null }
type Options = { duration?: number | null; isCurrent?(): boolean; onCopied?(): void; onFailed?(): void }

/** Feedback belongs to the displayed source and the latest attempt, including before effect cleanup. */
export function useClipboard(scope: unknown, { duration = 2000, isCurrent, onCopied, onFailed }: Options = {}) {
  const active = useRef<Owner>({ scope, sequence: 0, live: true, timer: null })
  if (!Object.is(active.current.scope, scope)) {
    active.current = { scope, sequence: 0, live: true, timer: null }
  }
  const owner = active.current
  const [feedback, setFeedback] = useState<{ owner: typeof owner; value: Feedback; copiedKey: string | null } | null>(null)
  const clearTimer = () => { if (owner.timer !== null) clearTimeout(owner.timer); owner.timer = null }
  const reset = () => { owner.sequence++; clearTimer(); if (active.current === owner && owner.live) setFeedback(null) }
  useEffect(() => {
    owner.live = true
    return () => { owner.live = false; owner.sequence++; clearTimer() }
  }, [owner])
  const copy = async (value: string, key = "default") => {
    if (active.current !== owner || !owner.live || isCurrent?.() === false) return
    const sequence = ++owner.sequence
    const current = () => active.current === owner && owner.live && owner.sequence === sequence && isCurrent?.() !== false
    clearTimer()
    setFeedback((previous) => ({ owner, value: { status: "pending", key }, copiedKey: duration === null && previous?.owner === owner ? previous.copiedKey : null }))
    try {
      // Start the write in the gesture stack, before the first await.
      await navigator.clipboard.writeText(value)
    } catch {
      if (current()) { setFeedback((previous) => ({ owner, value: { status: "failed", key }, copiedKey: duration === null && previous?.owner === owner ? previous.copiedKey : null })); onFailed?.() }
      return
    }
    if (!current()) return
    setFeedback({ owner, value: { status: "copied", key }, copiedKey: key })
    if (duration !== null) owner.timer = setTimeout(() => {
      if (current()) { owner.timer = null; setFeedback(null) }
    }, duration)
    onCopied?.()
  }
  const state: Feedback = feedback?.owner === owner ? feedback.value : { status: "idle" }
  return { state, copied: state.status === "copied" || duration === null && feedback?.owner === owner && feedback.copiedKey !== null, failed: state.status === "failed", copy, reset }
}
