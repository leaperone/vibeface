import { array, boolean, record, text } from '../lib/node.ts'
import { enqueue, type OutboxItem, type OutboxStore } from '../lib/outbox.ts'
import type { MemoryBase, MemoryControlResult, MemoryEntry, MemoryMetadata, MemoryState, MemoryStateResult, MemorySyncConflicts } from './types.ts'

export const DATA_EPOCH = 11
export type MemoryControls = {
  conversationId: string; writable: boolean
  controlMemory(conversationId: string, memoryId: string, base: MemoryBase, after: string | null): Promise<MemoryControlResult>
}

function counter(value: unknown): number {
  if (typeof value !== 'number' || !Number.isInteger(value) || value < 0 || value > 0xffff_ffff) throw new Error('记忆版本编号无效。')
  return value
}
function hash(value: unknown): string {
  const digest = text(value)
  if (!/^[a-f0-9]{64}$/.test(digest)) throw new Error('记忆版本格式无效。')
  return digest
}
function epoch(value: unknown): number {
  if (value !== DATA_EPOCH) throw new Error('记忆数据版本不一致，请更新设备后再试。')
  return value
}
export function parseMemoryBase(value: unknown): MemoryBase {
  const data = record(value), version = data.version === null ? null : record(data.version)
  return { epoch: epoch(data.epoch), version: version && { revision: hash(version.revision), control_revision: counter(version.control_revision), source_event_hash: hash(version.source_event_hash) } }
}
function metadata(value: unknown): MemoryMetadata {
  const data = record(value), origin = data.origin
  if (origin !== 'agent' && origin !== 'user' && origin !== 'review') throw new Error('未知的记忆来源。')
  return {
    epoch: epoch(data.epoch), revision: hash(data.revision), control_revision: counter(data.control_revision),
    source_event_hash: hash(data.source_event_hash), origin, user_controlled: boolean(data.user_controlled), essential: boolean(data.essential),
  }
}
export function memoryBase(value: MemoryMetadata | null): MemoryBase {
  return value ? { epoch: value.epoch, version: { revision: value.revision, control_revision: value.control_revision, source_event_hash: value.source_event_hash } } : { epoch: DATA_EPOCH, version: null }
}
function memoryState(value: unknown, memoryId?: string): MemoryState {
  const data = record(value), memory_id = text(data.memory_id)
  if (memoryId !== undefined && memory_id !== memoryId) throw new Error('设备返回了另一条记忆，未确认操作。')
  return { memory_id, text: data.text === null ? null : text(data.text), metadata: metadata(data.metadata) }
}
export function parseMemoryEntry(value: unknown): MemoryEntry {
  const entry = memoryState(value)
  if (entry.text === null) throw new Error('已保存记忆列表包含删除记录。')
  return { ...entry, text: entry.text }
}
export function parseMemoryControlResult(value: unknown, memoryId: string, operationId: string): MemoryControlResult {
  const data = record(value), state = data.state, operation_id = hash(data.operation_id)
  if (operation_id !== operationId) throw new Error('记忆回执与原操作不一致，未确认结果。')
  if (state !== 'applied' && state !== 'already_recorded' && state !== 'superseded' && state !== 'conflict') throw new Error('未知的记忆操作结果。')
  const current = data.current === null ? null : memoryState(data.current, memoryId)
  if ((state === 'applied' || state === 'already_recorded') && current?.metadata.revision !== operationId || state === 'superseded' && current?.metadata.revision === operationId) throw new Error('记忆回执与当前版本不一致，未确认结果。')
  return { state, operation_id, current }
}
export function parseMemoryStateResult(value: unknown, memoryId: string, operationId?: string): MemoryStateResult {
  const data = record(value), receipt = data.receipt
  if (receipt !== null && receipt !== 'recorded' && receipt !== 'superseded' && receipt !== 'not_seen_here') throw new Error('未知的记忆查询回执。')
  if (operationId === undefined ? receipt !== null : receipt !== 'recorded' && receipt !== 'superseded' && receipt !== 'not_seen_here') throw new Error('记忆查询缺少有效回执。')
  const current = data.current === null ? null : memoryState(data.current, memoryId)
  if (receipt === 'recorded' && current?.metadata.revision !== operationId || (receipt === 'superseded' || receipt === 'not_seen_here') && current?.metadata.revision === operationId) throw new Error('记忆回执与当前版本不一致，未确认结果。')
  return { current, receipt }
}
export function parseMemorySyncConflicts(value: unknown): MemorySyncConflicts {
  const data = record(value), items = array(data.items)
  if (items.length > 50) throw new Error('记忆同步冲突列表超出范围。')
  return { has_more: boolean(data.has_more), items: items.map((value) => {
    const item = record(value), reason = item.reason
    if (reason !== 'user_protected' && reason !== 'concurrent_automatic' && reason !== 'concurrent_user') throw new Error('未知的记忆同步冲突。')
    return { memory_id: text(item.memory_id), losing_revision: hash(item.losing_revision), winning_revision: hash(item.winning_revision), losing_event_hash: hash(item.losing_event_hash), winning_event_hash: hash(item.winning_event_hash), reason }
  }) }
}
export const memorySyncConflictLabel = (reason: MemorySyncConflicts['items'][number]['reason']) => reason === 'user_protected' ? '用户维护的记忆受保护，自动修改未覆盖' : reason === 'concurrent_user' ? '人工并发修改发生冲突，当前版本按共享规则选定' : 'Bot 并发更新发生冲突，当前版本按共享规则选定'
export const memorySaved = (result: MemoryControlResult) => result.state === 'applied' || result.state === 'already_recorded'
export const memoryResultLabel = (result: MemoryControlResult) => memorySaved(result) ? result.current?.text === null ? '记忆已删除。' : '记忆已保存。' : result.state === 'superseded' ? '这次修改已记录，但已被其他修改替代。草稿已保留。' : '当前版本与草稿基准不同，草稿已保留。未确认的原操作需等待设备同步确认。'

function afterText(value: unknown): string | null {
  if (value === null) return null
  const after = text(value, '记忆内容')
  if (!after.trim() || [...after].length > 500) throw new Error('记忆须为 1 至 500 个字符。')
  return after
}
function sameBase(base: MemoryBase, current: MemoryState | null): boolean {
  return base.epoch === DATA_EPOCH && (base.version === null ? current === null : current !== null && base.version.revision === current.metadata.revision && base.version.control_revision === current.metadata.control_revision)
}
function request(item: OutboxItem) {
  if (item.method !== 'control_memory') throw new Error('待确认操作不是记忆修改。')
  const data = item.params, attestation = record(data.attestation)
  return {
    conversation_id: text(data.conversation_id), memory_id: text(data.memory_id), base: parseMemoryBase(data.base), after: afterText(data.after),
    operation_id: hash(data.operation_id), attestation: { device_id: text(attestation.device_id), signature: text(attestation.signature) },
  }
}

export function memoryController(options: {
  outbox: OutboxStore
  key(conversationId: string, memoryId: string): string
  rpc(method: string, params: Record<string, unknown>): Promise<unknown>
  sign(conversationId: string, body: Record<string, unknown>): Promise<{ operation_id: string; attestation: { device_id: string; signature: string } }>
}) {
  const submit = async (item: OutboxItem, unknown: boolean): Promise<MemoryControlResult> => {
    const original = request(item)
    let response: unknown
    try { response = await options.rpc(item.method, item.params) }
    catch (caught) {
      if (!unknown && caught instanceof Error && 'responded' in caught && caught.responded === true) await options.outbox.delete(item.key)
      throw caught
    }
    const result = parseMemoryControlResult(response, original.memory_id, original.operation_id)
    if (!unknown || result.state !== 'conflict') await options.outbox.delete(item.key)
    return result
  }
  const reconcile = async (item: OutboxItem): Promise<MemoryControlResult> => {
    const original = request(item)
    const state = parseMemoryStateResult(await options.rpc('memory_state', { conversation_id: original.conversation_id, memory_id: original.memory_id, operation_id: original.operation_id }), original.memory_id, original.operation_id)
    if (state.receipt === 'recorded' || state.receipt === 'superseded') {
      await options.outbox.delete(item.key)
      return { state: state.receipt === 'recorded' ? 'already_recorded' : 'superseded', operation_id: original.operation_id, current: state.current }
    }
    if (!sameBase(original.base, state.current)) return { state: 'conflict', operation_id: original.operation_id, current: state.current }
    return submit(item, true)
  }
  const controlMemory = async (conversationId: string, memoryId: string, base: MemoryBase, after: string | null): Promise<MemoryControlResult> => {
    text(conversationId, '对话'); text(memoryId, '记忆')
    const parsedBase = parseMemoryBase(base), parsedAfter = afterText(after)
    if (parsedAfter === null && parsedBase.version === null) throw new Error('删除记忆需要当前版本。')
    const key = options.key(conversationId, memoryId)
    const pending = (await options.outbox.all()).find(item => item.key === key)
    if (pending) {
      const original = request(pending)
      if (original.conversation_id !== conversationId || original.memory_id !== memoryId) throw new Error('待确认操作属于另一条记忆，未发送。')
      const result = await reconcile(pending)
      if (JSON.stringify(original.base) !== JSON.stringify(parsedBase) || original.after !== parsedAfter) throw new Error(result.state === 'conflict' ? '原操作的结果仍未确认。草稿已保留，等待设备同步后再试。' : '之前的操作已有结果，当前草稿尚未提交。草稿已保留，请查看最新版本后再保存。')
      return result
    }
    const signed = await options.sign(conversationId, { type: 'memory_controlled', memory_id: memoryId, base: parsedBase, after: parsedAfter })
    const item = await enqueue(options.outbox, { key, method: 'control_memory', params: { conversation_id: conversationId, memory_id: memoryId, base: parsedBase, after: parsedAfter, ...signed }, created_at_ms: Date.now() })
    return submit(item, false)
  }
  return { controlMemory, reconcile }
}
