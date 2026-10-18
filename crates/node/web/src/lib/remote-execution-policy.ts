import { buildClientRemoteExecutionPolicyReadIntent } from '../../transport.js'
import type { DeviceChannel } from '../../transport.js'
import { beginRemoteExecutionPolicyImport } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization, VerifiedRemoteExecutionPolicy } from '../../generated/vibeface_secure.js'
import type { AuthorizationTrust } from './authorization-trust'
import type { RemoteExecutionSelection } from './remote-execution'
import type { prepareContext } from './rpc-prepare'
import { record } from './node'

export type RemoteExecutionPolicyPurpose = 'execution' | 'observation_read' | 'resolver'

export async function readRemoteExecutionPolicy(input: {
  withSource<T>(work: (source: VerifiedClientAuthorization) => T): Promise<T>
  selection: RemoteExecutionSelection
  context: ReturnType<typeof prepareContext>
  action: string
  purpose: RemoteExecutionPolicyPurpose
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
    const response = await channel.request(method, params)
    check()
    return record(response)
  }
  check()
  if (input.executionTrust.spaceId !== input.selection.execution_resource.space) throw new Error('执行政策信任与选定的执行空间不同。')
  const pinsJson = JSON.stringify(input.executionTrust.pins)
  const intentInputJson = JSON.stringify({ selection: input.selection, context: input.context, expected_action: input.action,
    purpose: input.purpose, execution_pins: input.executionTrust.pins })
  const intentJson = await input.withSource(source => buildClientRemoteExecutionPolicyReadIntent(source, intentInputJson, Date.now() + 120_000))
  check()
  const intent = record(JSON.parse(intentJson))
  const delegated = await call(input.bChannel, 'remote_execution_policy_delegate', intent)
  const request = { intent, delegation: record(delegated.delegation) }
  const requestJson = JSON.stringify(request)
  const begun = await call(input.cChannel, 'remote_execution_policy_prepare', request)
  let importer: ReturnType<typeof beginRemoteExecutionPolicyImport> | null = null
  let policy: VerifiedRemoteExecutionPolicy | null = null
  try {
    importer = await input.withSource(source => {
      const created = beginRemoteExecutionPolicyImport(source, requestJson, JSON.stringify(record(begun.snapshot)), pinsJson, input.executionNodePublicKey, Date.now())
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
      if (importer.pageRequestJson() === paramsJson) throw new Error('执行政策分页没有继续推进。')
    }
    const params = record(JSON.parse(importer.pageRequestJson()))
    const response = await call(input.cChannel, 'remote_execution_policy_finish', { ...params, cursor: null })
    const witnessJson = JSON.stringify(record(response.witness))
    const completed = importer; importer = null
    policy = completed.finish(witnessJson, Date.now())
    check()
    const latestTrust = input.latestExecutionTrust()
    if (latestTrust.accountId !== input.executionTrust.accountId || latestTrust.spaceId !== input.executionTrust.spaceId) throw new Error('最新执行政策信任属于其他账户或空间。')
    const current = policy.revalidatePins(JSON.stringify(latestTrust.pins), Date.now())
    policy.free(); policy = current
    check()
    const result = policy; policy = null
    return result
  } finally {
    importer?.free()
    policy?.free()
  }
}
