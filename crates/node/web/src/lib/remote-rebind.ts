import { deriveAuthorizationBodyHash, deriveAuthorizationId, nodeFingerprint } from '../../generated/vibeface_secure.js'
import { record, same, strings, text } from './node'
import type { AuthorizedRequest } from './node'
import { parseTrustedRemoteExecution, remoteExecutionProposal } from './remote-execution'
import type { TrustedRemoteExecution } from './remote-execution'
import { prepareRpc } from './rpc-action'

type Project = { space: string; resource: string }
export type RemoteRebindPlan = {
  id: string
  project: Project
  projectPublicKey: string
  binding: Record<string, unknown>
  executionReceipt: Record<string, unknown>
  oldAuthorization: TrustedRemoteExecution
  nextAuthorization: TrustedRemoteExecution
  nextOperationId: string
  cwd: string
  basisHeads: string[]
  expectedRevisionJson: string
}
type SealAttempt = { proof: AuthorizedRequest; invocationHash: string }
export type PendingRebind = { plan: RemoteRebindPlan } & (
  | { phase: 'seal'; body: Record<string, unknown>; attempt: SealAttempt | null }
  | { phase: 'rebind'; body: Record<string, unknown>; seal: Record<string, unknown>; sealAttempt: SealAttempt; attempt: AuthorizedRequest | null }
  | { phase: 'open'; body: Record<string, unknown>; seal: Record<string, unknown>; sealAttempt: SealAttempt; rebindAttempt: AuthorizedRequest; binding: Record<string, unknown>; basisHeads: string[]; attempt: AuthorizedRequest | null }
)

function bounded(body: Record<string, unknown>) {
  if (new TextEncoder().encode(JSON.stringify(body)).length > 65_536) throw new Error('重绑证明超过大小限制，请保留原请求并核对。')
  return structuredClone(body)
}
function invocationHash(value: unknown) {
  const result = text(value)
  if (!/^h2_[a-f0-9]{64}$/.test(result)) throw new Error('原封存签名哈希无效，请保留原请求。')
  return result
}
function originalSession(plan: RemoteRebindPlan) {
  const payload = record(plan.executionReceipt.payload), session = record(payload.session)
  const binding = record(plan.binding.binding), originalProject = record(session.original_project)
  const old = remoteExecutionProposal(plan.oldAuthorization), next = remoteExecutionProposal(plan.nextAuthorization)
  const revision = session.revision
  if (typeof revision !== 'number' || !Number.isSafeInteger(revision) || revision < 0 || plan.expectedRevisionJson !== JSON.stringify(revision)) throw new Error('原执行版本必须精确且在此客户端支持的整数范围内。')
  if (old.purpose !== 'rebind' || next.purpose !== 'execution') throw new Error('原执行端需要独立 Rebind 授权，新执行端需要独立执行授权。')
  const resource = plan.oldAuthorization.share.selection.execution_resource
  if (plan.executionReceipt.issuer !== plan.oldAuthorization.share.selection.target_public_key
    || originalProject.space !== plan.project.space || originalProject.resource !== plan.project.resource
    || payload.original_space_id !== plan.project.space || payload.execution_space_id !== resource.space
    || !same(session.execution_resource, resource)
    || binding.space_id !== plan.project.space || binding.authority_node !== nodeFingerprint(plan.projectPublicKey)
    || binding.target_node !== nodeFingerprint(plan.oldAuthorization.share.selection.target_public_key)
    || old.targetDevice !== deriveAuthorizationId('device', plan.oldAuthorization.share.selection.target_public_key)
    || old.targetDevice !== session.target || payload.target !== session.target
    || old.sessionId !== session.session_id || payload.session_id !== session.session_id || binding.session_id !== session.session_id
    || old.taskRef !== session.task || binding.task_ref !== session.task || old.conversationId !== binding.conversation_id
    || old.executorKind !== session.executor || record(binding.open_spec).executor !== session.executor
    || record(binding.open_spec).cwd !== session.cwd || binding.generation !== session.binding_generation
    || payload.binding_generation !== session.binding_generation
    || next.taskRef !== old.taskRef || next.conversationId !== old.conversationId
    || next.sessionId === old.sessionId && plan.nextAuthorization.share.selection.target_public_key === plan.oldAuthorization.share.selection.target_public_key) throw new Error('重绑的原项目、回执、执行会话或新目标与授权不同。')
  return { payload, session, binding, old, next, resource }
}

export function prepareRemoteRebind(plan: RemoteRebindPlan): PendingRebind {
  const { payload, resource } = originalSession(plan)
  if (!plan.cwd.trim() || plan.cwd.length > 4096 || !plan.cwd.startsWith('/')) throw new Error('请输入新执行端允许的绝对工作目录。')
  const prepared = prepareRpc({ method: 'seal_remote_execution', spaceId: resource.space, authorizationId: `seal:${plan.id}`, params: {
    resource_id: resource.resource, original_space_id: plan.project.space, operation_id: text(payload.operation_id),
    binding: plan.binding, execution_receipt: plan.executionReceipt,
  } })
  return { plan: structuredClone(plan), phase: 'seal', body: bounded(prepared.body), attempt: null }
}

function exactAttempt(body: Record<string, unknown>, value: unknown): AuthorizedRequest {
  const input = record(value), proof = { body: record(input.body), invocation: record(input.invocation) }
  if (!same(proof.body, body) || record(proof.invocation.payload).body_hash !== deriveAuthorizationBodyHash(JSON.stringify(body))) throw new Error('重绑的完整签名请求与原正文不同。')
  return structuredClone(proof)
}

export function recordRebindAttempt(pending: PendingRebind, proof: AuthorizedRequest, signedInvocationHash?: string): PendingRebind {
  const exact = exactAttempt(pending.body, proof)
  if (pending.phase === 'seal') {
    const attempt = { proof: exact, invocationHash: invocationHash(signedInvocationHash) }
    if (pending.attempt && !same(pending.attempt, attempt)) throw new Error('原封存签名已保存，不能替换签名或 nonce。')
    return { ...pending, attempt }
  }
  if (pending.attempt && !same(pending.attempt, exact)) throw new Error('原重绑签名已保存，不能替换签名或 nonce。')
  return { ...pending, attempt: exact }
}

export function remoteSealStatusBody(pending: PendingRebind) {
  if (pending.phase !== 'seal' || !pending.attempt) throw new Error('请先保存原封存签名，再核对同一次封存。')
  const { method: _method, space_id: spaceId, authorization_id: _authorizationId, ...params } = pending.body
  return bounded(prepareRpc({ method: 'remote_execution_seal_status', spaceId: text(spaceId), authorizationId: `seal-status:${pending.plan.id}`,
    params: { ...params, seal_invocation_hash: pending.attempt.invocationHash } }).body)
}

export function acceptRebindSeal(pending: PendingRebind, verifiedSeal: Record<string, unknown>): PendingRebind {
  if (pending.phase !== 'seal' || !pending.attempt) throw new Error('没有与此封存回执对应的原签名请求。')
  const { binding, old, next } = originalSession(pending.plan), seal = record(verifiedSeal.seal)
  if (seal.space_id !== pending.plan.project.space || seal.target_node !== binding.target_node || JSON.stringify(seal.final_revision) !== pending.plan.expectedRevisionJson) throw new Error('封存回执与原执行范围或最终版本不同。')
  const prepared = prepareRpc({ method: 'rebind_execution', spaceId: pending.plan.project.space, authorizationId: `rebind:${pending.plan.id}`, params: {
    resource_id: pending.plan.project.resource, conversation_id: old.conversationId, task_id: text(binding.task_id), task_ref: old.taskRef,
    binding: pending.plan.binding, previous_seal: verifiedSeal, previous_target_public_key: pending.plan.oldAuthorization.share.selection.target_public_key,
    target_node: nodeFingerprint(pending.plan.nextAuthorization.share.selection.target_public_key), session_id: next.sessionId,
    executor: next.executorKind, cwd: pending.plan.cwd, ...pending.plan.nextAuthorization.share.selection,
  } })
  return { plan: pending.plan, phase: 'rebind', body: bounded(prepared.body), seal: structuredClone(verifiedSeal), sealAttempt: pending.attempt, attempt: null }
}

export function acceptReboundBinding(pending: PendingRebind, verifiedBinding: Record<string, unknown>, basisHeads: string[]): PendingRebind {
  if (pending.phase !== 'rebind' || !pending.attempt) throw new Error('没有与此新绑定对应的原重绑签名请求。')
  const { binding: original, next } = originalSession(pending.plan), binding = record(verifiedBinding.binding)
  if (typeof original.generation !== 'number' || !Number.isSafeInteger(original.generation) || original.generation < 1 || original.generation >= Number.MAX_SAFE_INTEGER
    || binding.space_id !== original.space_id || binding.conversation_id !== original.conversation_id || binding.task_id !== original.task_id
    || binding.task_ref !== original.task_ref || binding.authority_node !== original.authority_node || binding.generation !== original.generation + 1
    || binding.target_node !== nodeFingerprint(pending.plan.nextAuthorization.share.selection.target_public_key) || binding.session_id !== next.sessionId
    || binding.previous_binding_hash !== record(pending.seal.seal).binding_hash
    || !same(binding.previous_seal, pending.seal) || !same(binding.open_spec, { type: 'open', executor: next.executorKind, cwd: pending.plan.cwd })) throw new Error('新绑定与原任务、封存回执或新执行范围不同。')
  const request = {
    operation_id: pending.plan.nextOperationId, space_id: pending.plan.project.space, conversation_id: text(original.conversation_id),
    task_id: text(original.task_id), binding: verifiedBinding, target_node: text(binding.target_node), session_id: next.sessionId,
    expected_revision: 0, basis_heads: basisHeads, resources: [pending.plan.cwd],
    action: { type: 'open', executor: next.executorKind, cwd: pending.plan.cwd },
  }
  const remote = pending.plan.nextAuthorization.share.selection
  const prepared = prepareRpc({ method: 'operate', spaceId: pending.plan.project.space, authorizationId: request.operation_id,
    params: { resource_id: pending.plan.project.resource, task_ref: next.taskRef, request, ...remote } })
  return { plan: pending.plan, phase: 'open', body: bounded(prepared.body), seal: pending.seal, sealAttempt: pending.sealAttempt, rebindAttempt: pending.attempt, binding: structuredClone(verifiedBinding), basisHeads: [...basisHeads], attempt: null }
}

export function parsePendingRebinds(value: unknown, spaceId: string, projectId: string): PendingRebind[] {
  if (!Array.isArray(value)) throw new Error('已保存的重绑请求格式无效，请保留浏览器数据。')
  const parsed = value.map(value => {
    const input = record(value), raw = record(input.plan), project = record(raw.project)
    const plan: RemoteRebindPlan = {
      id: text(raw.id), project: { space: text(project.space), resource: text(project.resource) }, projectPublicKey: text(raw.projectPublicKey),
      binding: record(raw.binding), executionReceipt: record(raw.executionReceipt), oldAuthorization: parseTrustedRemoteExecution(raw.oldAuthorization),
      nextAuthorization: parseTrustedRemoteExecution(raw.nextAuthorization), nextOperationId: text(raw.nextOperationId), cwd: text(raw.cwd),
      basisHeads: strings(raw.basisHeads), expectedRevisionJson: text(raw.expectedRevisionJson),
    }
    if (plan.project.space !== spaceId || plan.project.resource !== projectId) throw new Error('已保存的重绑请求属于其他项目。')
    let pending = prepareRemoteRebind(plan)
    if (input.phase !== 'seal') {
      const sealAttempt = record(input.sealAttempt)
      pending = recordRebindAttempt(pending, exactAttempt(pending.body, sealAttempt.proof), invocationHash(sealAttempt.invocationHash))
      pending = acceptRebindSeal(pending, record(input.seal))
      if (input.phase === 'open') {
        pending = recordRebindAttempt(pending, exactAttempt(pending.body, input.rebindAttempt))
        pending = acceptReboundBinding(pending, record(input.binding), strings(input.basisHeads))
      }
    }
    if (input.phase !== pending.phase || !same(input.body, pending.body)) throw new Error('已保存的重绑阶段或完整正文不同。')
    if (input.attempt === null) return pending
    if (pending.phase === 'seal') {
      const attempt = record(input.attempt)
      return recordRebindAttempt(pending, exactAttempt(pending.body, attempt.proof), invocationHash(attempt.invocationHash))
    }
    return recordRebindAttempt(pending, exactAttempt(pending.body, input.attempt))
  })
  if (new Set(parsed.map(item => item.plan.id)).size !== parsed.length) throw new Error('同一重绑存在多份请求，请保留原记录。')
  return parsed
}
