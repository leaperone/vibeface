import { buildClientRemoteExecutionHistoryPolicyReadIntent } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { beginRemoteExecutionHistoryPolicyImport } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization, VerifiedRemoteExecutionHistoryPolicy } from '../../generated/vibeface_secure.js'
import type { AuthorizationTrust } from './authorization-trust'
import type { AuthorizedRequest } from './node'
import { record } from './node'
import { beginForeignExecutionPolicyImport, lookupForeignExecutionPolicy } from './remote-execution-policy-ingress'

export async function readRemoteExecutionHistoryPolicy(input: {
  withSource<T>(work: (source: VerifiedClientAuthorization) => T): Promise<T>
  actual: Record<string, unknown>
  original: AuthorizedRequest
  binding?: Record<string, unknown> | null
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
    const response = await channel.request('identity', { method, params })
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
  const callSource = (method: string, params: Record<string, unknown>) => call(input.bChannel, method, params)
  const selected = await lookupForeignExecutionPolicy(callSource, intent)
  if (selected.approvalRef === undefined) throw new Error('来源节点缺少原任务已批准的执行政策，请先同步并核对历史。')
  const delegated = await call(input.bChannel, 'remote_execution_policy_delegate', intent)
  const request = { intent, delegation: record(delegated.delegation) }
  const requestJson = JSON.stringify(request)
  const begun = await call(input.cChannel, 'remote_execution_policy_prepare', request)
  let importer: ReturnType<typeof beginRemoteExecutionHistoryPolicyImport> | null = null
  let policy: VerifiedRemoteExecutionHistoryPolicy | null = null
  try {
    const snapshot = record(begun.snapshot)
    importer = await input.withSource(source => {
      const created = beginRemoteExecutionHistoryPolicyImport(source, requestJson, JSON.stringify(snapshot), pinsJson, input.executionNodePublicKey, Date.now())
      importer = created
      return created
    })
    check()
    const ingress = await beginForeignExecutionPolicyImport({ call: callSource, request, snapshot, approvalRef: selected.approvalRef, purpose: 'history' })
    for (;;) {
      const paramsJson = importer.pageRequestJson()
      const response = await call(input.cChannel, 'remote_execution_policy_page', record(JSON.parse(paramsJson)))
      const page = record(response.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      await ingress.append(page)
      if (record(page.payload).complete === true) break
      if (importer.pageRequestJson() === paramsJson) throw new Error('历史政策分页没有继续推进。')
    }
    const params = record(JSON.parse(importer.pageRequestJson()))
    const response = await call(input.cChannel, 'remote_execution_policy_finish', { ...params, cursor: null })
    const witness = record(response.witness), witnessJson = JSON.stringify(witness)
    const completed = importer; importer = null
    policy = completed.finish(witnessJson, Date.now())
    check()
    const current = policy.revalidatePins(JSON.stringify(input.latestIndependentPins()), Date.now())
    policy.free(); policy = current
    check()
    await ingress.finish(witness)
    check()
    const result = policy; policy = null
    return result
  } finally {
    importer?.free()
    policy?.free()
  }
}
