import { buildClientRemoteSharePolicyReadRequest } from '../../transport.js'
import { beginRemoteSharePolicyImport } from '../../generated/vibeface_secure.js'
import { array, integer, record, same } from './node'
import { parseAuthorizationTrust } from './authorization'
import type { AuthorizationTrust } from './authorization'

export async function readRemoteSharePolicy(input: {
  intentJson: string
  trust: AuthorizationTrust
  nodePublicKey: string
  call(method: string, params: Record<string, unknown>): Promise<unknown>
  check(): void
  signal?: AbortSignal
}) {
  const check = () => { input.signal?.throwIfAborted(); input.check() }
  const call = async (method: string, params: Record<string, unknown>) => {
    check()
    const reply = await input.call(method, params)
    check()
    return record(reply)
  }
  check()
  const intent = record(record(JSON.parse(input.intentJson)).intent)
  const expiresAt = Math.min(Date.now() + 120_000, integer(intent.expires_at_ms))
  const pinsJson = JSON.stringify(input.trust.pins)
  const requestJson = buildClientRemoteSharePolicyReadRequest(input.intentJson, pinsJson, input.nodePublicKey, expiresAt)
  const begun = await call('execution_share_policy_prepare', record(JSON.parse(requestJson)))
  const snapshotJson = JSON.stringify(begun.snapshot)
  let importer: ReturnType<typeof beginRemoteSharePolicyImport> | null = null
  try {
    importer = beginRemoteSharePolicyImport(requestJson, snapshotJson, pinsJson, input.nodePublicKey, Date.now())
    for (;;) {
      const paged = await call('execution_share_policy_page', record(JSON.parse(importer.pageRequestJson())))
      const page = record(paged.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      if (record(page.payload).complete === true) break
    }
    const params = record(JSON.parse(importer.pageRequestJson()))
    const finished = await call('execution_share_policy_finish', { ...params, cursor: null })
    const witnessJson = JSON.stringify(record(finished.witness))
    const completed = importer; importer = null
    const verified = completed.finish(witnessJson, Date.now())
    try {
      check()
      const heads = array(record(record(JSON.parse(snapshotJson)).payload).target_heads).map(value => record(value))
      const trust = parseAuthorizationTrust({ ...input.trust, pins: input.trust.pins.map(pin => ({ ...pin, minimum_head: heads.find(head => same(head.domain, pin.domain)) ?? pin.minimum_head })) })
      return { verified, trust }
    }
    catch (error) { verified.free(); throw error }
  } finally { importer?.free() }
}
