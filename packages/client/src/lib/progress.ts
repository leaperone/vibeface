type ProgressUpdate =
  | { type: 'text_snapshot' | 'text_delta'; item_id?: string; text: string }
  | { type: 'status' | 'tool'; item_id?: string; text?: string; status?: string; name?: string }

type ProgressFields = { conversation_id: string; sequence: number; progress: ProgressUpdate }
type ManagerProgressEvent = ProgressFields & { type: 'manager_progress'; trigger: string; source_node: string; invocation_id?: string }
type NativeProgressEvent = ProgressFields & { type: 'native_progress'; work_id: string; target_node: string }
/**
 * `records`: what one change touched in a conversation, merged in place; `value` is parsed by the workspace.
 * `invalidate`: reload; with a conversation, only what that conversation shows.
 */
export type ChangedEvent =
  | { type: 'invalidate'; conversation_id?: string }
  | { type: 'records'; conversation_id: string; value: Record<string, unknown> }
  | ManagerProgressEvent | NativeProgressEvent
type LiveProgressFields = {
  key: string; conversation_id: string
  sequence: number; text: string; status?: string; items: { item_id: string; text: string }[]; needsRefresh: boolean
}
export type LiveProgress = LiveProgressFields & (
  | { kind: 'manager'; trigger: string; source_node: string; invocation_id?: string }
  | { kind: 'native'; work_id: string; target_node: string }
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
    if (!isRecord(value)) return { type: 'invalidate' }
    if (value.type === 'records') return { type: 'records', conversation_id: requiredText(value.conversation_id), value }
    if (value.type === 'changed' && typeof value.conversation_id === 'string') return { type: 'invalidate', conversation_id: value.conversation_id }
    if (value.type !== 'manager_progress' && value.type !== 'native_progress') return { type: 'invalidate' }
    const conversation_id = requiredText(value.conversation_id), progress = parseProgress(value.progress)
    const sequence = value.sequence
    if (typeof sequence !== 'number' || !Number.isSafeInteger(sequence) || sequence < 1) return { type: 'invalidate' }
    if (value.type === 'manager_progress') return { type: value.type, conversation_id, progress, sequence, trigger: requiredText(value.trigger), source_node: requiredText(value.source_node), invocation_id: optionalText(value.invocation_id) }
    return { type: value.type, conversation_id, progress, sequence, work_id: requiredText(value.work_id), target_node: requiredText(value.target_node) }
  } catch { return { type: 'invalidate' } }
}

export function mergeProgress(previous: LiveProgress[], event: ChangedEvent): LiveProgress[] {
  if (event.type === 'invalidate' || event.type === 'records') return previous
  const key = event.type === 'manager_progress'
    ? JSON.stringify(['manager', event.conversation_id, event.source_node, event.trigger])
    : JSON.stringify(['native', event.conversation_id, event.target_node, event.work_id])
  const current = previous.find(item => item.key === key)
  const existing = event.type === 'manager_progress' && current?.kind === 'manager' && current.invocation_id !== event.invocation_id ? undefined : current
  if (existing && event.sequence <= existing.sequence) return previous
  const fields: LiveProgressFields = {
    key, conversation_id: event.conversation_id,
    sequence: event.sequence, text: existing?.text ?? '', items: existing?.items ?? [],
    status: existing?.status,
    needsRefresh: Boolean(existing?.needsRefresh || event.sequence > (existing?.sequence ?? 0) + 1),
  }
  const next: LiveProgress = event.type === 'manager_progress'
    ? { ...fields, kind: 'manager', trigger: event.trigger, source_node: event.source_node, invocation_id: event.invocation_id }
    : { ...fields, kind: 'native', work_id: event.work_id, target_node: event.target_node }
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
  return current ? previous.map(item => item.key === key ? next : item) : [...previous, next]
}
