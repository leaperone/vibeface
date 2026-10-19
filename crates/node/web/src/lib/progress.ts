export type ProgressUpdate =
  | { type: 'text_snapshot' | 'text_delta'; item_id?: string; text: string }
  | { type: 'status' | 'tool'; item_id?: string; text?: string; status?: string; name?: string }

type ProgressFields = { conversation_id: string; sequence?: number; progress: ProgressUpdate }
export type ManagerProgressEvent = ProgressFields & { type: 'manager_progress'; trigger: string; source_node?: string }
export type NativeProgressEvent = ProgressFields & { type: 'native_progress'; work_id: string; thread_id: string; turn_id?: string; task_ref?: string; target_node: string }
export type ChangedEvent = { type: 'invalidate' } | ManagerProgressEvent | NativeProgressEvent
type LiveProgressFields = {
  key: string; conversation_id: string
  sequence?: number; text: string; status?: string; items: { item_id: string; text: string }[]; needsRefresh: boolean
}
export type LiveProgress = LiveProgressFields & (
  | { kind: 'manager'; trigger: string; source_node?: string }
  | { kind: 'native'; work_id: string; thread_id: string; turn_id?: string; task_ref?: string; target_node: string }
)

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
    if (!isRecord(value) || value.type !== 'manager_progress' && value.type !== 'native_progress') return { type: 'invalidate' }
    const conversation_id = requiredText(value.conversation_id), progress = parseProgress(value.progress)
    const sequence = value.sequence === undefined || value.sequence === null ? undefined : value.sequence
    if (sequence !== undefined && (typeof sequence !== 'number' || !Number.isSafeInteger(sequence) || sequence < 0)) return { type: 'invalidate' }
    if (value.type === 'manager_progress') return { type: value.type, conversation_id, progress, sequence, trigger: requiredText(value.trigger), source_node: optionalText(value.source_node) }
    return { type: value.type, conversation_id, progress, sequence, work_id: requiredText(value.work_id), thread_id: requiredText(value.thread_id), turn_id: optionalText(value.turn_id), task_ref: optionalText(value.task_ref), target_node: requiredText(value.target_node) }
  } catch { return { type: 'invalidate' } }
}

export function mergeProgress(previous: LiveProgress[], event: ChangedEvent): LiveProgress[] {
  if (event.type === 'invalidate') return previous
  const key = event.type === 'manager_progress'
    ? JSON.stringify(['manager', event.conversation_id, event.source_node ?? '', event.trigger])
    : JSON.stringify(['native', event.conversation_id, event.target_node, event.work_id, event.thread_id, event.turn_id ?? ''])
  const existing = previous.find(item => item.key === key)
  if (existing?.sequence !== undefined && event.sequence !== undefined && event.sequence <= existing.sequence) return previous
  const fields: LiveProgressFields = {
    key, conversation_id: event.conversation_id,
    sequence: event.sequence ?? existing?.sequence, text: existing?.text ?? '', items: existing?.items ?? [],
    status: existing?.status,
    needsRefresh: Boolean(existing?.needsRefresh || event.sequence !== undefined && event.sequence > (existing?.sequence ?? 0) + 1),
  }
  const next: LiveProgress = event.type === 'manager_progress'
    ? { ...fields, kind: 'manager', trigger: event.trigger, source_node: event.source_node }
    : { ...fields, kind: 'native', work_id: event.work_id, thread_id: event.thread_id, turn_id: event.turn_id, task_ref: event.task_ref, target_node: event.target_node }
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
