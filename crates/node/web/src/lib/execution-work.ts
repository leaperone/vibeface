import { integer, record, text } from './node.ts'

type ExecutionSession = { target: string; session_id: string }
type ConversationOpen = { operation_id: string; invocation_hash: string; admission_hash: string }
export type ExecutionWork =
  | { kind: 'task_session'; task: string; binding_hash: string; binding_generation: number; execution_session: ExecutionSession }
  | { kind: 'conversation_session'; conversation: string; execution_session: ExecutionSession; executor: string; canonical_cwd: string; open: ConversationOpen }
  | { kind: 'command'; request_hash: string; canonical_cwd: string }
export type ExecutionWorkIntent =
  | Omit<Extract<ExecutionWork, { kind: 'task_session' }>, 'binding_hash'>
  | Omit<Extract<ExecutionWork, { kind: 'conversation_session' }>, 'open'> & { open: { kind: 'existing'; open: ConversationOpen } | { kind: 'opening'; operation_id: string } }
  | Extract<ExecutionWork, { kind: 'command' }>
export type ProjectExecutionProof = { dispatch: Record<string, unknown>; origin: Record<string, unknown>; result?: Record<string, unknown> }
export type VerifiedProjectWork = { work: ExecutionWork; context: Record<string, unknown>; request: Record<string, unknown> }

export function parseExecutionWork(value: unknown): ExecutionWork {
  const work = record(value)
  if (work.kind === 'command') return { kind: 'command', request_hash: text(work.request_hash), canonical_cwd: text(work.canonical_cwd) }
  const session = record(work.execution_session)
  const execution_session = { target: text(session.target), session_id: text(session.session_id) }
  if (work.kind === 'task_session') return { kind: 'task_session', task: text(work.task), binding_hash: text(work.binding_hash), binding_generation: integer(work.binding_generation), execution_session }
  if (work.kind !== 'conversation_session') throw new Error('执行证明包含未知的工作类型。')
  const open = record(work.open)
  return { kind: 'conversation_session', conversation: text(work.conversation), execution_session, executor: text(work.executor), canonical_cwd: text(work.canonical_cwd),
    open: { operation_id: text(open.operation_id), invocation_hash: text(open.invocation_hash), admission_hash: text(open.admission_hash) } }
}

export function parseVerifiedProjectWork(value: unknown): VerifiedProjectWork {
  const verified = record(value)
  return { work: parseExecutionWork(verified.work), context: record(verified.context), request: record(verified.request) }
}

export function parseExecutionWorkIntent(value: unknown): ExecutionWorkIntent {
  const work = record(value)
  if (work.kind === 'command') return { kind: 'command', request_hash: text(work.request_hash), canonical_cwd: text(work.canonical_cwd) }
  const session = record(work.execution_session), execution_session = { target: text(session.target), session_id: text(session.session_id) }
  if (work.kind === 'task_session') return { kind: 'task_session', task: text(work.task), binding_generation: integer(work.binding_generation), execution_session }
  if (work.kind !== 'conversation_session') throw new Error('执行授权包含未知的工作类型。')
  const intent = record(work.open)
  let open: Extract<ExecutionWorkIntent, { kind: 'conversation_session' }>['open']
  if (intent.kind === 'opening') open = { kind: 'opening', operation_id: text(intent.operation_id) }
  else if (intent.kind === 'existing') {
    const original = record(intent.open)
    open = { kind: 'existing', open: { operation_id: text(original.operation_id), invocation_hash: text(original.invocation_hash), admission_hash: text(original.admission_hash) } }
  } else throw new Error('普通会话授权缺少原打开来源。')
  return { kind: 'conversation_session', conversation: text(work.conversation), execution_session, executor: text(work.executor), canonical_cwd: text(work.canonical_cwd), open }
}

export function executionWorkIntent(work: ExecutionWork): ExecutionWorkIntent {
  if (work.kind === 'command') return work
  if (work.kind === 'task_session') {
    const { binding_hash: _hash, ...intent } = work
    return intent
  }
  return { ...work, open: { kind: 'existing', open: work.open } }
}
