import { counter, typedId } from './authorization-trust'
import { record } from './node'

export type RemoteExecutionPolicyUse = { kind: 'current' } | { kind: 'candidate'; binding: Record<string, unknown> | null }
export type PolicyReadCall = (method: string, params: Record<string, unknown>) => Promise<Record<string, unknown>>

function importProgress(value: Record<string, unknown>) {
  if (typeof value.ready_for_finish !== 'boolean') throw new Error('来源节点没有确认执行政策导入进度。')
  return { importRef: typedId(value.import_ref, 'h2_'), nextPageIndex: counter(value.next_page_index), readyForFinish: value.ready_for_finish }
}

export async function beginForeignExecutionPolicyImport(input: {
  call: PolicyReadCall
  request: Record<string, unknown>
  snapshot: Record<string, unknown>
  approvalRef?: string
  purpose: string
}) {
  let progress = importProgress(await input.call('foreign_execution_policy_begin', { request: input.request, snapshot: input.snapshot,
    ...(input.approvalRef === undefined ? {} : { approval_ref: input.approvalRef }) }))
  const importRef = progress.importRef
  return {
    async append(page: Record<string, unknown>) {
      const next = importProgress(await input.call('foreign_execution_policy_page', { import_ref: importRef, page }))
      if (next.importRef !== importRef || next.nextPageIndex < progress.nextPageIndex || next.nextPageIndex <= counter(record(page.payload).page_index)) throw new Error('来源节点返回了其他执行政策导入或没有确认原分页。')
      progress = next
    },
    async finish(witness: Record<string, unknown>) {
      if (!progress.readyForFinish) throw new Error('来源节点尚未收到完整执行政策，已暂停业务请求。')
      const completed = await input.call('foreign_execution_policy_finish', { import_ref: importRef, witness })
      typedId(completed.policy_ref, 'h2_')
      const approvalRef = typedId(completed.approval_ref, 'h2_')
      if (completed.purpose !== input.purpose || input.approvalRef !== undefined && approvalRef !== input.approvalRef) throw new Error('来源节点完成了其他用途或批准的执行政策。')
    },
  }
}

export async function lookupForeignExecutionPolicy(call: PolicyReadCall, intent: Record<string, unknown>) {
  const selected = await call('foreign_execution_policy_selection', intent)
  if (selected.policy_ref !== null) typedId(selected.policy_ref, 'h2_')
  return { metadata: selected, approvalRef: selected.approval_ref === null ? undefined : typedId(selected.approval_ref, 'h2_') }
}
