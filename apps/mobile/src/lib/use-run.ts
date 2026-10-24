import { useEffect, useRef, useState } from 'react'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf } from '@vibeface/client/lib/node.ts'

/**
 * Busy and error state bound to the connection generation, as on the web: a press runs once, and a result from an
 * older connection never unlocks or reports on a newer one. `busy` names the action in flight.
 */
export function useRun() {
  const device = useDevice()
  const generation = device.connectionGeneration()
  const [active, setActive] = useState<{ generation: number; key: string } | null>(null)
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  useEffect(() => { setError(''); setStatus('') }, [generation])
  async function run(key: string, action: () => Promise<void>, success = '') {
    if (pending.current === device.connectionGeneration()) return
    let owner = device.connectionGeneration()
    setError(''); setStatus('')
    try {
      const work = action()
      owner = device.connectionGeneration(); pending.current = owner; setActive({ generation: owner, key })
      await work
      if (owner === device.connectionGeneration()) setStatus(success)
    } catch (cause) { if (owner === device.connectionGeneration()) setError(messageOf(cause)) }
    finally { if (pending.current === owner) pending.current = null; setActive((current) => current?.generation === owner ? null : current) }
  }
  return { busy: active?.generation === generation ? active.key : null, error, status, run }
}
