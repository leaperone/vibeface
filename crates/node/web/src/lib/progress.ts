export type ProgressUpdate =
  | { type: 'text_snapshot' | 'text_delta'; item_id?: string; text: string }
  | { type: 'status' | 'tool'; item_id?: string; text?: string; status?: string; name?: string }

type ProgressFields = { conversation_id: string; sequence?: number; progress: ProgressUpdate }
export type ManagerProgressEvent = ProgressFields & { type: 'manager_progress'; trigger: string; source_node?: string }
export type ExecutionProgressEvent = ProgressFields & { type: 'execution_progress'; session_id: string; operation_id?: string; task_id?: string; target_node?: string }
export type ChangedEvent = { type: 'invalidate' } | ManagerProgressEvent | ExecutionProgressEvent
export type LiveProgress = {
  kind: 'manager' | 'execution'; key: string; conversation_id: string
  trigger?: string; source_node?: string; session_id?: string; operation_id?: string; task_id?: string; target_node?: string
  sequence?: number; text: string; status?: string; items: { item_id: string; text: string }[]; needsRefresh: boolean
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}
function requiredText(value: unknown): string {
  if (typeof value !== 'string' || !value) throw new Error('无效的实时进度编号。')
  return value
}
function optionalText(value: unknown): string | undefined {
  return value === undefined || value === null ? undefined : requiredText(value)
}
function parseProgress(value: unknown): ProgressUpdate {
  if (!isRecord(value)) throw new Error('无效的实时进度。')
  const item_id = optionalText(value.item_id)
  if (value.type === 'text_snapshot' || value.type === 'text_delta') {
    if (typeof value.text !== 'string') throw new Error('无效的实时文本。')
    return { type: value.type, item_id, text: value.text }
  }
  if (value.type === 'status' || value.type === 'tool') {
    const text = optionalText(value.text), status = optionalText(value.status), name = optionalText(value.name)
    if (!text && !status && !name) throw new Error('实时进度缺少描述。')
    return { type: value.type, item_id, text, status, name }
  }
  throw new Error('不支持的实时进度。')
}

export function parseChanged(value: unknown): ChangedEvent {
  try {
    if (!isRecord(value) || value.type !== 'manager_progress' && value.type !== 'execution_progress') return { type: 'invalidate' }
    const conversation_id = requiredText(value.conversation_id), progress = parseProgress(value.progress)
    const sequence = value.sequence === undefined || value.sequence === null ? undefined : value.sequence
    if (sequence !== undefined && (typeof sequence !== 'number' || !Number.isSafeInteger(sequence) || sequence < 0)) return { type: 'invalidate' }
    if (value.type === 'manager_progress') return { type: value.type, conversation_id, progress, sequence, trigger: requiredText(value.trigger), source_node: optionalText(value.source_node) }
    return { type: value.type, conversation_id, progress, sequence, session_id: requiredText(value.session_id), operation_id: optionalText(value.operation_id), task_id: optionalText(value.task_id), target_node: optionalText(value.target_node) }
  } catch { return { type: 'invalidate' } }
}

export function mergeProgress(previous: LiveProgress[], event: ChangedEvent): LiveProgress[] {
  if (event.type === 'invalidate') return previous
  const key = event.type === 'manager_progress'
    ? JSON.stringify(['manager', event.conversation_id, event.source_node ?? '', event.trigger])
    : JSON.stringify(['execution', event.conversation_id, event.target_node ?? '', event.session_id, event.operation_id ?? ''])
  const existing = previous.find(item => item.key === key)
  if (existing?.sequence !== undefined && event.sequence !== undefined && event.sequence <= existing.sequence) return previous
  const next: LiveProgress = {
    ...existing, kind: event.type === 'manager_progress' ? 'manager' : 'execution', key, conversation_id: event.conversation_id,
    ...(event.type === 'manager_progress' ? { trigger: event.trigger, source_node: event.source_node } : { session_id: event.session_id, operation_id: event.operation_id, task_id: event.task_id, target_node: event.target_node }),
    sequence: event.sequence ?? existing?.sequence, text: existing?.text ?? '', items: existing?.items ?? [],
    needsRefresh: Boolean(existing?.needsRefresh || event.sequence !== undefined && event.sequence > (existing?.sequence ?? 0) + 1),
  }
  const progress = event.progress
  if (progress.type === 'text_snapshot' || progress.type === 'text_delta') {
    const item_id = progress.item_id ?? 'message'
    const item = next.items.find(item => item.item_id === item_id)
    const replacement = { item_id, text: progress.type === 'text_snapshot' ? progress.text : (item?.text ?? '') + progress.text }
    next.items = item ? next.items.map(item => item.item_id === item_id ? replacement : item) : [...next.items, replacement]
    next.text = next.items.map(item => item.text).join('\n\n')
  } else if (progress.type === 'status' || progress.type === 'tool') {
    next.status = progress.text ?? progress.status ?? progress.name
  }
  return existing ? previous.map(item => item.key === key ? next : item) : [...previous, next]
}
