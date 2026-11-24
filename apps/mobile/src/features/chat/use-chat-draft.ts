import { useEffect, useMemo, useRef, useSyncExternalStore } from 'react'
import { AppState } from 'react-native'
import { chatDrafts, type DraftScope, type DraftSnapshot } from '@/platform/native-chat-drafts'
import type { Attachment, PickedAttachment } from '@/data/types'
import { privateFiles } from '@/platform/files'
import { selectionOwner } from '@vibeface/client/lib/selection.ts'

type DraftView = { snapshot: DraftSnapshot; text: string; files: Attachment[]; error: string }
/** Typing is written this long after its first unsaved change; sending, leaving the draft and leaving the foreground write it at once. */
const saveDelay = 500

export type ChatDraftOwner = ReturnType<typeof draftOwner>
// Typing not written yet, by draft key: a draft rebuilt for the same scope before the old one writes starts from it, not from disk.
const unsaved = new Map<string, { text: string; typed: () => boolean }>()

function draftOwner(key: string, scope: DraftScope, current: { current: unknown }) {
  let failure = ''
  let snapshot: DraftSnapshot = { key, revision: '', text: '', files: [], missingFiles: [] }
  if (scope.networkId && scope.deviceId && scope.conversationId && scope.inputTarget) {
    try { snapshot = chatDrafts.read(scope) }
    catch (cause) { failure = cause instanceof Error ? cause.message : '草稿无法读取，请重试。' }
  }
  const pending = unsaved.get(key)
  const owner = { scope, snapshot, text: pending?.typed() ? pending.text : snapshot.text, files: snapshot.files, sequence: 0, tail: Promise.resolve(snapshot), failure, copies: new Map<string, Attachment>(), flushed: new WeakMap<DraftSnapshot, number>(), timer: null as ReturnType<typeof setTimeout> | null, typed: () => true }
  let view: DraftView = { snapshot, text: owner.text, files: owner.files, error: failure }
  const listeners = new Set<() => void>()
  const setView = (next: DraftView | ((previous: DraftView) => DraftView)) => {
    view = typeof next === 'function' ? next(view) : next
    for (const listener of listeners) listener()
  }

  const persist = async () => {
    if (owner.timer) { clearTimeout(owner.timer); owner.timer = null }
    const sequence = ++owner.sequence
    const requested = { text: owner.text, files: owner.files }
    const previous = owner.tail
    const work = privateFiles.run(async (valid) => {
      await previous.catch(() => owner.snapshot)
      if (!valid()) throw new Error('应用内数据正在清除，请重试。')
      const snapshot = await chatDrafts.write(owner.scope, { text: requested.text, files: requested.files.map((file) => owner.copies.get(file.uri) ?? file) })
      requested.files.forEach((file, index) => { const saved = snapshot.files[index]; if (saved) owner.copies.set(file.uri, saved) })
      if (owner.files === requested.files) owner.files = snapshot.files
      return snapshot
    })
    owner.tail = work
    try {
      const snapshot = await work
      owner.snapshot = snapshot
      if (sequence === owner.sequence) {
        owner.files = snapshot.files
        owner.failure = ''
        if (unsaved.get(key)?.text === requested.text) unsaved.delete(key)
        if (current.current === draft) setView({ snapshot, text: owner.text, files: snapshot.files, error: '' })
      }
      return snapshot
    } catch (cause) {
      if (sequence === owner.sequence) {
        owner.files = owner.snapshot.files
        owner.failure = cause instanceof Error ? cause.message : '草稿没有保存，请重试。'
        if (current.current === draft) setView({ snapshot: owner.snapshot, text: owner.text, files: owner.files, error: owner.failure })
      }
      throw cause
    }
  }
  /** Writes typing still waiting for its delay. Typing from before an erase is not written back after it, as it was erased with the rest. */
  const save = () => {
    if (!owner.timer) return
    if (owner.typed()) void persist().catch(() => {})
    else { clearTimeout(owner.timer); owner.timer = null }
  }
  const draft = {
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener) } },
    view: () => view,
    save,
    changeText(text: string) {
      owner.text = text
      if (current.current === draft) setView((previous) => ({ ...previous, snapshot: owner.snapshot, text, files: owner.files, error: '' }))
      if (!owner.timer) { owner.typed = privateFiles.lease(); owner.timer = setTimeout(save, saveDelay) }
      unsaved.set(key, { text, typed: owner.typed })
    },
    async changeFiles(files: Attachment[]) { owner.files = files; await persist() },
    async addFiles(files: PickedAttachment[]) {
      if (!files.length) return
      const selection = selectionOwner<PickedAttachment>((cause) => {
        if (current.current === draft) setView((previous) => ({ ...previous, error: cause.message }))
        else console.error('选择副本清理失败，副本保留待清除。', cause)
      })
      selection.replace(files)
      try {
        await selection.use(async (picked) => {
          owner.files = [...owner.files, ...picked]
          await persist()
        })
      } finally { selection.close() }
    },
    async flush(): Promise<DraftSnapshot> {
      save()
      const sequence = owner.sequence
      const snapshot = await owner.tail
      if (owner.failure && owner.sequence === sequence) throw new Error(owner.failure)
      owner.flushed.set(snapshot, sequence)
      return snapshot
    },
    async clear(snapshot: DraftSnapshot) {
      const sequence = owner.sequence
      // Typing not written yet is newer than the flushed snapshot, just as a write after it would be.
      if (owner.timer || owner.flushed.get(snapshot) !== sequence) return false
      const previous = owner.tail
      const work = privateFiles.run(async (valid) => {
        await previous
        if (!valid()) throw new Error('应用内数据正在清除，请重试。')
        if (owner.timer || owner.sequence !== sequence || !await chatDrafts.clear(owner.scope, snapshot)) return false
        if (owner.sequence === sequence && !owner.timer) {
          const empty = chatDrafts.read(owner.scope)
          owner.snapshot = empty; owner.text = empty.text; owner.files = empty.files
          if (current.current === draft) setView({ snapshot: empty, text: empty.text, files: empty.files, error: '' })
        }
        return true
      })
      owner.tail = work.then(() => owner.snapshot)
      void owner.tail.catch(() => {})
      return work
    },
  }
  return draft
}

/** One scope's draft without its view: the methods keep their identity, and typing re-renders only what calls `useDraftView`. */
export function useChatDraftOwner(scope: DraftScope) {
  const key = JSON.stringify([scope.networkId, scope.deviceId, scope.conversationId, scope.inputTarget])
  const current = useRef<unknown>(null)
  const draft = useMemo(() => draftOwner(key, scope, current), [key])
  current.current = draft
  useEffect(() => {
    current.current = draft
    const foreground = AppState.addEventListener('change', (state) => { if (state !== 'active') draft.save() })
    return () => { foreground.remove(); if (current.current === draft) current.current = null; draft.save() }
  }, [draft])
  return draft
}

export const useDraftView = (draft: ChatDraftOwner) => useSyncExternalStore(draft.subscribe, draft.view)

export function useChatDraft(scope: DraftScope) {
  const draft = useChatDraftOwner(scope)
  const { text, files, error, snapshot } = useDraftView(draft)
  return { text, files, error, missingFiles: snapshot.missingFiles, changeText: draft.changeText, changeFiles: draft.changeFiles, addFiles: draft.addFiles, flush: draft.flush, clear: draft.clear }
}
