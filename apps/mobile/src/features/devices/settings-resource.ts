import { useEffect, useRef, useState } from 'react'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf, parseNodeSettings, type NodeSettings, type NodeSettingsResponse, type ProviderChange } from '@vibeface/client/lib/node.ts'
import { parseTriageSettings } from '@vibeface/client/workspace/model.ts'
import type { TriageSettings } from '@vibeface/client/workspace/types.ts'

export type TriageChange = Pick<TriageSettings, 'default_outlet' | 'projects' | 'scan_codex_sessions' | 'codex_sessions'> & {
  webhook_url: TriageSettings['webhook_url'] | null
  webhook_secret?: string | null
}
type NodeOperations = {
  node_settings: undefined
  configure_provider: ProviderChange
  configure_node: NodeSettings
  provider_probe: Pick<ProviderChange, 'provider'>
}
type TriageOperations = {
  triage_settings: undefined
  configure_triage: TriageChange
  test_webhook: undefined
  create_decision_token: Pick<TriageSettings['decision_tokens'][number], 'label'>
  revoke_decision_token: Pick<TriageSettings['decision_tokens'][number], 'token_id'>
}
type Operations = NodeOperations & TriageOperations
type Method = keyof Operations
type Strategy = 'returnsSnapshot' | 'reloadAfterSuccess'
type Resource<Snapshot, Operation extends Method> = {
  read: Extract<Operation, 'node_settings' | 'triage_settings'>
  parse(value: unknown): Snapshot
  strategies: { [Name in Operation]: Strategy }
}
type Arguments<Name extends Method> = Operations[Name] extends undefined
  ? [params?: undefined, at?: string]
  : [params: Operations[Name], at?: string]

export const nodeSettingsResource: Resource<NodeSettingsResponse, keyof NodeOperations> = {
  read: 'node_settings',
  parse: parseNodeSettings,
  strategies: {
    node_settings: 'returnsSnapshot',
    configure_provider: 'returnsSnapshot',
    configure_node: 'returnsSnapshot',
    provider_probe: 'reloadAfterSuccess',
  },
}
export const triageSettingsResource: Resource<TriageSettings, keyof TriageOperations> = {
  read: 'triage_settings',
  parse: parseTriageSettings,
  strategies: {
    triage_settings: 'returnsSnapshot',
    configure_triage: 'returnsSnapshot',
    test_webhook: 'reloadAfterSuccess',
    create_decision_token: 'reloadAfterSuccess',
    revoke_decision_token: 'reloadAfterSuccess',
  },
}

/** A newer call or connection makes an older settings response stale. */
export function useSettingsResource<Snapshot, Operation extends Method>(resource: Resource<Snapshot, Operation>) {
  const { ready } = useDevice()
  const [value, setValue] = useState<Snapshot | null>(null)
  const [busy, setBusy] = useState('')
  const [error, setError] = useState<{ at: string; text: string } | null>(null)
  const sequence = useRef(0)
  const invoke = async (key: string, method: Operation, params?: Record<string, unknown>, at = '') => {
    if (!ready?.isCurrent()) return null
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const result = await connection.request(method, params)
      if (owner !== sequence.current || !connection.isCurrent()) return null
      const latest = resource.strategies[method] === 'returnsSnapshot' ? result : await connection.request(resource.read)
      if (owner !== sequence.current || !connection.isCurrent()) return null
      setValue(resource.parse(latest))
      return result
    } catch (cause) {
      if (owner === sequence.current) setError({ at, text: messageOf(cause) })
      return null
    } finally { if (owner === sequence.current) setBusy('') }
  }
  const call = <Name extends Operation>(key: string, method: Name, ...[params, at]: Arguments<Name>) => invoke(key, method, params, at)
  const load = () => invoke('load', resource.read)
  useEffect(() => { setValue(null); void load(); return () => { sequence.current += 1 } }, [ready])
  return { value, busy, error, call, load, setError }
}
