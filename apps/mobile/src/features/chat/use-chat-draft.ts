import { useEffect, useMemo, useRef, useState } from 'react'
import { chatDrafts, type DraftScope, type DraftSnapshot } from '@/platform/native-chat-drafts'
import type { Attachment } from '@/data/types'
import { privateFiles } from '@/platform/files'

export function useChatDraft(scope: DraftScope) {
  const key = JSON.stringify([scope.networkId, scope.deviceId, scope.conversationId, scope.inputTarget])
  const owner = useMemo(() => {
    let failure = ''
    let snapshot: DraftSnapshot = { key, revision: '', text: '', files: [], missingFiles: [] }
    if (scope.networkId && scope.deviceId && scope.conversationId && scope.inputTarget) {
      try { snapshot = chatDrafts.read(scope) }
      catch (cause) { failure = cause instanceof Error ? cause.message : '草稿无法读取，请重试。' }
    }
    return { key, scope, snapshot, text: snapshot.text, files: snapshot.files, sequence: 0, tail: Promise.resolve(snapshot), failure, copies: new Map<string, Attachment>() }
  }, [key])
  const current = useRef<typeof owner | null>(owner)
  current.current = owner
  const [view, setView] = useState(() => ({ key, snapshot: owner.snapshot, text: owner.text, files: owner.files, error: owner.failure }))
  const shown = view.key === key ? view : { key, snapshot: owner.snapshot, text: owner.text, files: owner.files, error: owner.failure }
  useEffect(() => { current.current = owner; return () => { if (current.current === owner) current.current = null } }, [owner])

  const persist = async () => {
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
        if (current.current === owner) setView({ key, snapshot, text: owner.text, files: snapshot.files, error: '' })
      }
      return snapshot
    } catch (cause) {
      if (sequence === owner.sequence) {
        owner.files = owner.snapshot.files
        owner.failure = cause instanceof Error ? cause.message : '草稿没有保存，请重试。'
        if (current.current === owner) setView({ key, snapshot: owner.snapshot, text: owner.text, files: owner.files, error: owner.failure })
      }
      throw cause
    }
  }
  return {
    text: shown.text,
    files: shown.files,
    error: shown.error,
    missingFiles: shown.snapshot.missingFiles,
    changeText(text: string) {
      owner.text = text
      if (current.current === owner) setView((previous) => ({ ...previous, key, snapshot: owner.snapshot, text, files: owner.files, error: '' }))
      void persist().catch(() => {})
    },
    async changeFiles(files: Attachment[]) { owner.files = files; await persist() },
    async flush(): Promise<DraftSnapshot> {
      await owner.tail
      if (owner.failure) throw new Error(owner.failure)
      return chatDrafts.read(owner.scope)
    },
    async clear(snapshot: DraftSnapshot) {
      const sequence = owner.sequence
      if (!await privateFiles.run(async () => chatDrafts.clear(owner.scope, snapshot))) return false
      if (current.current === owner && owner.sequence === sequence) {
        const empty = chatDrafts.read(owner.scope)
        owner.snapshot = empty; owner.text = empty.text; owner.files = empty.files
        setView({ key, snapshot: empty, text: empty.text, files: empty.files, error: '' })
      }
      return true
    },
  }
}
