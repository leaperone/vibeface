import { rpc } from '@/lib/node'
import { useEffect, useRef, useState } from 'react'

import './chat.css'

import { Chat } from './elements/chat.tsx'
import { PromptInput } from './elements/prompt-input.tsx'
import {
  browserStore,
  canSend,
  composerHint,
  errorStatus,
  errorText,
  isIncomplete,
  parseConversation,
  pendingAfterFailure,
  prepareSend,
  readFrontier,
  readPending,
  writeFrontier,
  writePending,
} from './protocol.ts'
import type { ConversationPayload, ConversationScreenProps, PendingMessage } from './types.ts'

export function ConversationScreen({
  conversationId,
  schedulerNodeId,
  missing,
  schedulerOnline,
  managerReady,
  session = null,
}: ConversationScreenProps) {
  const [payload, setPayload] = useState<ConversationPayload | null>(null)
  const [loadedId, setLoadedId] = useState<string | null>(null)
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const [pending, setPending] = useState<Map<string, PendingMessage>>(() => {
    const store = browserStore()
    return store ? readPending(store, session) : new Map()
  })
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [reloadToken, setReloadToken] = useState(0)
  const draftsRef = useRef(drafts)
  const pendingRef = useRef(pending)
  const textRef = useRef(text)
  draftsRef.current = drafts
  pendingRef.current = pending
  textRef.current = text

  useEffect(() => {
    if (!conversationId) {
      setText('')
      return
    }
    const draft = draftsRef.current.get(conversationId)
    setText(draft ?? pendingRef.current.get(conversationId)?.text ?? '')
  }, [conversationId])

  useEffect(() => {
    if (!conversationId) {
      setPayload(null)
      setLoadedId(null)
      return
    }
    const id = conversationId
    let cancelled = false
    const store = browserStore()
    void rpc('conversation', {
      conversation_id: id,
      basis_heads: store ? readFrontier(store, session, id) : [],
    })
      .then((data: unknown) => {
        if (cancelled) return
        const next = parseConversation(data)
        setPayload(next)
        setLoadedId(id)
        setError('')
        if (store && !isIncomplete(next.missing)) writeFrontier(store, session, id, next.heads)
        const confirmed = pendingRef.current.get(id)
        if (!confirmed || !next.messages.some((message) => message.message_id === confirmed.message_id)) return
        setPending((current) => {
          const copy = new Map(current)
          copy.delete(id)
          const target = browserStore()
          if (target) writePending(target, session, copy)
          return copy
        })
        setDrafts((current) => {
          const copy = new Map(current)
          if ((copy.get(id) ?? confirmed.text).trim() === confirmed.text) copy.delete(id)
          draftsRef.current = copy
          return copy
        })
        if (textRef.current.trim() === confirmed.text) setText('')
      })
      .catch((reason: unknown) => {
        if (!cancelled) setError(errorText(reason))
      })
    return () => {
      cancelled = true
    }
  }, [conversationId, reloadToken])

  useEffect(() => {
    if (!conversationId) return
    const timer = window.setInterval(() => setReloadToken((value) => value + 1), 15000)
    const onVisible = () => {
      if (!document.hidden) setReloadToken((value) => value + 1)
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      window.clearInterval(timer)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [conversationId])

  const loaded = loadedId === conversationId && payload !== null
  const contextMissing = missing || (loaded && isIncomplete(payload.missing))
  const gate = {
    hasConversation: Boolean(conversationId),
    loaded,
    missing: contextMissing,
    schedulerOnline,
    managerReady,
    schedulerNodeId,
  }
  const hint = composerHint(gate)
  const sendDisabled = !canSend({ ...gate, busy })
  const empty = loaded
    ? { title: '从你的目标开始', detail: '告诉调度 Agent 要做什么，也可以先创建一张工单。' }
    : { title: '准备好继续工作', detail: '新建会话，或从左侧选择已有会话。' }

  function persist(next: Map<string, PendingMessage>) {
    const store = browserStore()
    if (!store) throw new Error('浏览器无法保存操作编号。请允许此站点使用本地存储后重试。')
    try {
      writePending(store, session, next)
    } catch {
      throw new Error('浏览器无法保存操作编号。请允许此站点使用本地存储后重试。')
    }
    pendingRef.current = next
    setPending(next)
  }

  async function submit() {
    if (!conversationId || !schedulerNodeId || !payload || sendDisabled) return
    const prepared = prepareSend({
      conversationId,
      text,
      schedulerNodeId,
      pending: pending.get(conversationId) ?? null,
      heads: payload.heads,
    })
    if (!prepared.ok) {
      setError(prepared.error)
      return
    }
    const nextPending = new Map(pending)
    nextPending.set(conversationId, prepared.pending)
    try {
      persist(nextPending)
    } catch (reason) {
      setError(errorText(reason))
      return
    }
    setBusy(true)
    setError('')
    try {
      await rpc('message', { ...prepared.body })
      setPending((current) => {
        const copy = new Map(current)
        if (copy.get(prepared.pending.conversation_id)?.message_id === prepared.pending.message_id) {
          copy.delete(prepared.pending.conversation_id)
        }
        const store = browserStore()
        if (store) writePending(store, session, copy)
        pendingRef.current = copy
        return copy
      })
      setDrafts((current) => {
        const copy = new Map(current)
        if (copy.get(prepared.pending.conversation_id)?.trim() === prepared.pending.text) {
          copy.delete(prepared.pending.conversation_id)
        }
        draftsRef.current = copy
        return copy
      })
      setText((current) => (current.trim() === prepared.pending.text ? '' : current))
      setReloadToken((value) => value + 1)
    } catch (reason) {
      if (pendingAfterFailure(errorStatus(reason)) === 'drop') {
        setPending((current) => {
          const copy = new Map(current)
          if (copy.get(prepared.pending.conversation_id)?.message_id === prepared.pending.message_id) {
            copy.delete(prepared.pending.conversation_id)
          }
          const store = browserStore()
          if (store) writePending(store, session, copy)
          pendingRef.current = copy
          return copy
        })
      }
      setError(errorText(reason))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="conversation-screen" aria-label="调度会话">
      {error ? <p role="alert">{error}</p> : null}
      <Chat messages={loaded ? payload.messages : []} emptyTitle={empty.title} emptyDetail={empty.detail} />
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={busy}
        onChange={(value) => {
          setText(value)
          if (!conversationId) return
          const copy = new Map(draftsRef.current)
          copy.set(conversationId, value)
          draftsRef.current = copy
          setDrafts(copy)
        }}
        onSubmit={() => {
          void submit()
        }}
      />
    </section>
  )
}
