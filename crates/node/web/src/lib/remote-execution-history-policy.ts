import { buildClientRemoteExecutionHistoryPolicyReadIntent } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { beginRemoteExecutionHistoryPolicyImport } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization, VerifiedRemoteExecutionHistoryPolicy } from '../../generated/vibeface_secure.js'
import type { AuthorizationTrust } from './authorization-trust'
import type { AuthorizedRequest } from './node'
import { record } from './node'

export async function readRemoteExecutionHistoryPolicy(input: {
  withSource<T>(work: (source: VerifiedClientAuthorization) => T): Promise<T>
  actual: Record<string, unknown>
  original: AuthorizedRequest
  binding?: Record<string, unknown>
  independentPins: AuthorizationTrust['pins']
  latestIndependentPins(): AuthorizationTrust['pins']
  bChannel: DeviceChannel
  cChannel: DeviceChannel
  sourceNodePublicKey: string
  executionNodePublicKey: string
  check(): void
  signal?: AbortSignal
}): Promise<VerifiedRemoteExecutionHistoryPolicy> {
  const check = () => {
    input.signal?.throwIfAborted()
    input.check()
    if (input.bChannel.publicKey !== input.sourceNodePublicKey || input.cChannel.publicKey !== input.executionNodePublicKey) throw new Error('历史政策读取连接与独立保存的来源或执行端公钥不同。')
  }
  const call = async (channel: DeviceChannel, method: string, params: Record<string, unknown>) => {
    check()
    const response = await channel.request(method, params)
    check()
    return record(response)
  }
  check()
  const pinsJson = JSON.stringify(input.independentPins)
  const intentInputJson = JSON.stringify({ actual_json: JSON.stringify(input.actual), body_json: JSON.stringify(input.original.body),
    invocation_json: JSON.stringify(input.original.invocation), ...(input.binding === undefined ? {} : { binding_json: JSON.stringify(input.binding) }) })
  const intentJson = await input.withSource(source => buildClientRemoteExecutionHistoryPolicyReadIntent(source, intentInputJson, pinsJson, Date.now() + 120_000))
  check()
  const intent = record(JSON.parse(intentJson))
  const delegated = await call(input.bChannel, 'remote_execution_policy_delegate', intent)
  const request = { intent, delegation: record(delegated.delegation) }
  const requestJson = JSON.stringify(request)
  const begun = await call(input.cChannel, 'remote_execution_policy_prepare', request)
  let importer: ReturnType<typeof beginRemoteExecutionHistoryPolicyImport> | null = null
  let policy: VerifiedRemoteExecutionHistoryPolicy | null = null
  try {
    importer = await input.withSource(source => {
      const created = beginRemoteExecutionHistoryPolicyImport(source, requestJson, JSON.stringify(record(begun.snapshot)), pinsJson, input.executionNodePublicKey, Date.now())
      importer = created
      return created
    })
    check()
    for (;;) {
      const paramsJson = importer.pageRequestJson()
      const response = await call(input.cChannel, 'remote_execution_policy_page', record(JSON.parse(paramsJson)))
      const page = record(response.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      if (record(page.payload).complete === true) break
      if (importer.pageRequestJson() === paramsJson) throw new Error('历史政策分页没有继续推进。')
    }
    const params = record(JSON.parse(importer.pageRequestJson()))
    const response = await call(input.cChannel, 'remote_execution_policy_finish', { ...params, cursor: null })
    const witnessJson = JSON.stringify(record(response.witness))
    const completed = importer; importer = null
    policy = completed.finish(witnessJson, Date.now())
    check()
    const current = policy.revalidatePins(JSON.stringify(input.latestIndependentPins()), Date.now())
    policy.free(); policy = current
    check()
    const result = policy; policy = null
    return result
  } finally {
    importer?.free()
    policy?.free()
  }
}
