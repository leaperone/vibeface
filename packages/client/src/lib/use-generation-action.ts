import { useEffect, useRef, useState, type SetStateAction } from 'react'
import { createActionOwner, type ActionClaim, type ActionTransition } from './action-owner.ts'
import { messageOf } from './node.ts'

export type GenerationAction = {
  current(): boolean
  /** Called at an action's own transition, with the generations immediately before and after. */
  rebind(transition: ActionTransition<number>): void
}

/** A keyed action and its feedback follow the connection that owns them. */
export function useGenerationAction(getGeneration: () => number) {
  const generation = getGeneration()
  const [owner] = useState(() => createActionOwner<number>())
  const [active, setActive] = useState<{ generation: number; key: string; claim: ActionClaim<number> } | null>(null)
  const [feedback, setFeedback] = useState({ generation, error: '', status: '' })
  const mounted = useRef(true)
  useEffect(() => {
    mounted.current = true
    return () => { mounted.current = false; owner.cancel() }
  }, [owner])

  function setError(value: SetStateAction<string>) {
    const generation = getGeneration()
    setFeedback(previous => {
      const current = previous.generation === generation ? previous : { generation, error: '', status: '' }
      return { ...current, error: typeof value === 'function' ? value(current.error) : value }
    })
  }

  async function run(key: string, action: (claim: GenerationAction) => Promise<unknown>, success = '') {
    let generation = getGeneration()
    const claim = owner.claim(generation, value => mounted.current && value === getGeneration())
    if (!claim) return
    setActive({ generation, key, claim })
    setFeedback({ generation, error: '', status: '' })
    try {
      await action({
        current: claim.current,
        rebind(transition) {
          if (claim.rebind(transition)) {
            generation = transition.to
            setActive({ generation, key, claim })
            setFeedback({ generation, error: '', status: '' })
          }
        },
      })
      if (claim.current()) setFeedback(previous => ({ generation, error: previous.generation === generation ? previous.error : '', status: success }))
    } catch (cause) {
      if (claim.current()) setError(messageOf(cause))
    } finally {
      if (claim.finish()) setActive(current => current?.claim === claim ? null : current)
    }
  }

  function cancel() {
    owner.cancel()
    setActive(null)
  }

  return {
    busy: active?.generation === generation && active.claim.current() ? active.key : null,
    error: feedback.generation === generation ? feedback.error : '',
    status: feedback.generation === generation ? feedback.status : '',
    run, cancel, setError, clear: () => setError(''),
  }
}
