import { buildClientRemoteExecutionPolicyReadIntent } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { beginRemoteExecutionPolicyImport } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization, VerifiedRemoteExecutionPolicy } from '../../generated/vibeface_secure.js'
import type { AuthorizationTrust } from './authorization-trust'
import { counter, parseAuthorizationTrust, typedId } from './authorization-trust'
import type { RemoteExecutionSelection } from './remote-execution'
import type { ExecutionWorkIntent } from './execution-work'
import type { prepareContext } from './rpc-prepare'
import { beginForeignExecutionPolicyImport, lookupForeignExecutionPolicy } from './remote-execution-policy-ingress'
import type { RemoteExecutionPolicyUse } from './remote-execution-policy-ingress'
import { record, same } from './node'

export type RemoteExecutionPolicyPurpose = 'execution' | 'observation_read' | 'resolver'

export async function readRemoteExecutionPolicy(input: {
  withSource<T>(work: (source: VerifiedClientAuthorization) => T): Promise<T>
  selection: RemoteExecutionSelection
  context: ReturnType<typeof prepareContext>
  work: ExecutionWorkIntent
  action: string
  purpose: RemoteExecutionPolicyPurpose
  use?: RemoteExecutionPolicyUse
  executionTrust: AuthorizationTrust
  latestExecutionTrust(): AuthorizationTrust
  bChannel: DeviceChannel
  cChannel: DeviceChannel
  sourceNodePublicKey: string
  executionNodePublicKey: string
  check(): void
  signal?: AbortSignal
}): Promise<VerifiedRemoteExecutionPolicy> {
  const check = () => {
    input.signal?.throwIfAborted()
    input.check()
    if (input.bChannel.publicKey !== input.sourceNodePublicKey || input.cChannel.publicKey !== input.executionNodePublicKey
      || input.selection.target_public_key !== input.executionNodePublicKey) throw new Error('政策读取连接与独立保存的来源或执行端公钥不同。')
  }
  const call = async (channel: DeviceChannel, method: string, params: Record<string, unknown>) => {
    check()
    const response = await channel.request('identity', { method, params })
    check()
    return record(response)
  }
  check()
  if (input.executionTrust.spaceId !== input.selection.execution_resource.space) throw new Error('执行政策信任与选定的执行空间不同。')
  const pinsJson = JSON.stringify(input.executionTrust.pins)
  let context = input.context, action = input.action, work = input.work
  const buildIntent = async () => {
    const intentInputJson = JSON.stringify({ selection: input.selection, context, work, expected_action: action,
      purpose: input.purpose, execution_pins: input.executionTrust.pins })
    const intentJson = await input.withSource(source => buildClientRemoteExecutionPolicyReadIntent(source, intentInputJson, Date.now() + 120_000))
    check()
    return record(JSON.parse(intentJson))
  }
  const callSource = (method: string, params: Record<string, unknown>) => call(input.bChannel, method, params)
  let intent = await buildIntent()
  const selected = await lookupForeignExecutionPolicy(callSource, intent)
  const binding = selected.metadata.binding === null ? null : record(record(selected.metadata.binding).binding)
  let approvalRef = selected.approvalRef
  if (input.use?.kind === 'candidate') {
    if (!same(selected.metadata.binding, input.use.binding)) throw new Error('来源节点的当前任务绑定已变化，请先核对原任务。')
    if (binding !== null && input.purpose === 'execution') {
      const approved = parseAuthorizationTrust({ ...input.executionTrust, pins: selected.metadata.approved_pins })
      const roots = (trust: AuthorizationTrust) => trust.pins.map(pin => ({ domain: pin.domain, genesis_root: pin.genesis_root }))
        .sort((left, right) => JSON.stringify(left.domain).localeCompare(JSON.stringify(right.domain)))
      if (!same(record(selected.metadata.selection), input.selection) || !same(roots(approved), roots(input.executionTrust))) {
        const generation = counter(binding.generation) + 1
        if (!Number.isSafeInteger(generation)) throw new Error('新绑定代次超过此客户端支持范围。')
        if (work.kind !== 'task_session') throw new Error('只有任务会话支持绑定代次。')
        action = 'execution_rebind'; context = { ...context, binding_generation: generation }; work = { ...work, binding_generation: generation }
        intent = await buildIntent()
      }
    }
  } else if (input.purpose === 'execution' && (binding === null && action === 'execution_open' && context.binding_generation === 1
    || binding !== null && action === 'execution_rebind' && context.binding_generation === counter(binding.generation) + 1)) {
    const approved = await callSource('foreign_execution_policy_approve', intent)
    approvalRef = typedId(approved.approval_ref, 'h2_')
    typedId(approved.choice_ref, 'h2_')
  } else if (approvalRef === undefined) throw new Error('来源节点缺少此任务已批准的执行政策，请先同步并核对原选择。')
  const delegated = await call(input.bChannel, 'remote_execution_policy_delegate', intent)
  const request = { intent, delegation: record(delegated.delegation) }
  const requestJson = JSON.stringify(request)
  const begun = await call(input.cChannel, 'remote_execution_policy_prepare', request)
  let importer: ReturnType<typeof beginRemoteExecutionPolicyImport> | null = null
  let policy: VerifiedRemoteExecutionPolicy | null = null
  try {
    const snapshot = record(begun.snapshot)
    importer = await input.withSource(source => {
      const created = beginRemoteExecutionPolicyImport(source, requestJson, JSON.stringify(snapshot), pinsJson, input.executionNodePublicKey, Date.now())
      importer = created
      return created
    })
    check()
    const ingress = input.use?.kind === 'candidate' ? null : await beginForeignExecutionPolicyImport({ call: callSource, request, snapshot, approvalRef, purpose: input.purpose })
    for (;;) {
      const paramsJson = importer.pageRequestJson()
      const response = await call(input.cChannel, 'remote_execution_policy_page', record(JSON.parse(paramsJson)))
      const page = record(response.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      await ingress?.append(page)
      if (record(page.payload).complete === true) break
      if (importer.pageRequestJson() === paramsJson) throw new Error('执行政策分页没有继续推进。')
    }
    const params = record(JSON.parse(importer.pageRequestJson()))
    const response = await call(input.cChannel, 'remote_execution_policy_finish', { ...params, cursor: null })
    const witness = record(response.witness), witnessJson = JSON.stringify(witness)
    const completed = importer; importer = null
    policy = completed.finish(witnessJson, Date.now())
    check()
    const latestTrust = input.latestExecutionTrust()
    if (latestTrust.accountId !== input.executionTrust.accountId || latestTrust.spaceId !== input.executionTrust.spaceId) throw new Error('最新执行政策信任属于其他账户或空间。')
    const current = policy.revalidatePins(JSON.stringify(latestTrust.pins), Date.now())
    policy.free(); policy = current
    check()
    await ingress?.finish(witness)
    check()
    const result = policy; policy = null
    return result
  } finally {
    importer?.free()
    policy?.free()
  }
}
