import type { Attachment } from '../data/types.ts'

export type DraftScope = { networkId: string; deviceId: string; conversationId: string; inputTarget: string }
export type ChatDraft = { text: string; files: Attachment[] }
export type DraftSnapshot = ChatDraft & { key: string; revision: string; missingFiles: string[] }
type Storage = { get(key: string): string | undefined; set(key: string, value: string): void; remove(key: string): void }
type Files = { retain(file: Attachment): Promise<Attachment>; exists(uri: string): boolean; release(uri: string): void; owns(uri: string): boolean }

export function draftScopeKey(scope: DraftScope) {
  if ([scope.networkId, scope.deviceId, scope.conversationId, scope.inputTarget].some(value => typeof value !== 'string' || !value)) throw new Error('请先选择网络、设备和对话。')
  return JSON.stringify([scope.networkId, scope.deviceId, scope.conversationId, scope.inputTarget])
}
function parse(value: string | undefined): { text: string; files: Attachment[]; revision: string } | null {
  if (!value) return null
  let data: unknown
  try { data = JSON.parse(value) } catch { throw new Error('本地草稿无法读取，请先保留文字后重试。') }
  if (!data || typeof data !== 'object' || !('text' in data) || typeof data.text !== 'string' || !('revision' in data) || typeof data.revision !== 'string' || !('files' in data) || !Array.isArray(data.files)) throw new Error('本地草稿无法读取，请先保留文字后重试。')
  const files: Attachment[] = []
  const entries: unknown[] = data.files
  for (const file of entries) {
    if (!file || typeof file !== 'object' || !('uri' in file) || typeof file.uri !== 'string' || !('name' in file) || typeof file.name !== 'string' || !('size' in file) || !(file.size === null || (typeof file.size === 'number' && Number.isSafeInteger(file.size) && file.size >= 0)) || !('mimeType' in file) || !(file.mimeType === null || typeof file.mimeType === 'string')) throw new Error('本地附件记录无法读取，请重新选择。')
    files.push({ uri: file.uri, name: file.name, size: file.size, mimeType: file.mimeType })
  }
  return { text: data.text, files, revision: data.revision }
}

/** Commit the complete draft before publishing UI state. Reader leases outlive edits and successful sends. */
export function createChatDrafts({ storage, files, revision }: { storage: Storage; files: Files; revision(): string }) {
  const readers = new Map<string, number>(), retired = new Set<string>(), queues = new Map<string, Promise<unknown>>()
  const release = (uri: string) => {
    if (readers.has(uri)) { retired.add(uri); return }
    try { files.release(uri); retired.delete(uri) } catch { retired.add(uri) }
  }
  function read(scope: DraftScope): DraftSnapshot {
    const key = draftScopeKey(scope), saved = parse(storage.get(key))
    const available: Attachment[] = [], missingFiles: string[] = []
    for (const file of saved?.files ?? []) {
      if (files.owns(file.uri) && files.exists(file.uri)) available.push(file)
      else missingFiles.push(file.name)
    }
    return { key, revision: saved?.revision ?? '', text: saved?.text ?? '', files: available, missingFiles }
  }
  function enqueue<T>(key: string, operation: () => Promise<T>): Promise<T> {
    const work = (queues.get(key) ?? Promise.resolve()).catch(() => {}).then(operation)
    queues.set(key, work)
    void work.finally(() => { if (queues.get(key) === work) queues.delete(key) }).catch(() => {})
    return work
  }
  function write(scope: DraftScope, draft: ChatDraft) {
    const key = draftScopeKey(scope)
    return enqueue(key, async () => {
      const previous = parse(storage.get(key)), retained: Attachment[] = [], added: Attachment[] = []
      try {
        for (const file of draft.files) {
          if (!files.exists(file.uri)) throw new Error(`附件「${file.name}」已被系统清除，请重新选择。`)
          const owned = previous?.files.some(saved => saved.uri === file.uri)
          const copy = owned && files.owns(file.uri) ? file : await files.retain(file)
          retained.push({ uri: copy.uri, name: copy.name, size: copy.size, mimeType: copy.mimeType })
          if (copy !== file) added.push(copy)
        }
        const saved = { text: draft.text, files: retained, revision: revision() }
        const encoded = JSON.stringify(saved)
        try { storage.set(key, encoded) } catch (cause) { if (storage.get(key) !== encoded) throw cause }
        if (storage.get(key) !== encoded) throw new Error('本地草稿未完整保存，请重试。')
        for (const file of previous?.files ?? []) if (!retained.some(next => next.uri === file.uri)) release(file.uri)
        return { ...saved, key, missingFiles: [] } satisfies DraftSnapshot
      } catch (cause) { for (const file of added) release(file.uri); throw cause }
    })
  }
  function clear(scope: DraftScope, expected: DraftSnapshot) {
    const key = draftScopeKey(scope)
    return enqueue(key, async () => {
      const saved = parse(storage.get(key))
      if (expected.key !== key || saved?.revision !== expected.revision) return false
      try { storage.remove(key) } catch (cause) { if (storage.get(key) !== undefined) throw cause }
      if (storage.get(key) !== undefined) throw new Error('本地草稿尚未清除，请重试。')
      for (const file of saved.files) release(file.uri)
      return true
    })
  }
  function lease(draft: DraftSnapshot) {
    let active = true
    for (const file of draft.files) {
      if (!files.owns(file.uri) || !files.exists(file.uri)) throw new Error(`附件「${file.name}」已被系统清除，请重新选择。`)
    }
    for (const file of draft.files) readers.set(file.uri, (readers.get(file.uri) ?? 0) + 1)
    return { files: draft.files, release() {
      if (!active) return
      active = false
      for (const file of draft.files) {
        const count = (readers.get(file.uri) ?? 1) - 1
        if (count) readers.set(file.uri, count)
        else { readers.delete(file.uri); if (retired.has(file.uri)) release(file.uri) }
      }
    } }
  }
  return { read, write, clear, lease }
}
