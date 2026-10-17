import { useEffect, useRef, useState } from 'react'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton, DetailList } from '@/features/workspace/present'
import { Chat } from './elements/chat'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import './chat.css'

export function ConversationScreen() {
  const workspace = useWorkspace()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const text = drafts.get(conversationId) ?? ''
  const messages = data?.messages ?? []
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const sendDisabled = !workspace.writable || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const hint = messagePending ? '先核对原消息，再发送新内容。' : composerHint(gate)

  useEffect(() => {
    if (messagePending) sentDrafts.current.set(messagePending.conversation_id, messagePending)
    const previous = sentDrafts.current.get(conversationId)
    if (!previous || !data?.messages.some((message) => message.message_id === previous.message_id)) return
    sentDrafts.current.delete(conversationId)
    setDrafts((current) => {
      if (current.get(conversationId)?.trim() !== previous.text) return current
      const next = new Map(current)
      next.delete(conversationId)
      return next
    })
  }, [conversationId, data, messagePending])

  function clearSentDraft(id: string, sent: string) {
    setDrafts((current) => {
      if (current.get(id)?.trim() !== sent.trim()) return current
      const next = new Map(current)
      next.delete(id)
      return next
    })
  }

  return (
    <section className="conversation-screen" aria-label="调度会话">
      <Chat
        messages={messages}
        emptyTitle={data ? '从你的目标开始' : '准备好继续工作'}
        emptyDetail={data ? '告诉调度 Agent 要做什么，也可以先创建一张工单。' : '新建会话，或从左侧选择已有会话。'}
      />
      {messagePending ? (
        <section aria-label="待核对消息">
          <h3>原消息尚未确认</h3>
          <DetailList pairs={[
            ['消息编号', messagePending.message_id],
            ['原调度设备', messagePending.scheduler_node],
            ['内容', messagePending.text],
          ]} />
          <p>刷新可核对原消息。重试会保留原编号、内容和调度设备。</p>
          <ActionButton action={() => workspace.run(() => workspace.refresh())}>核对原消息</ActionButton>
          <ActionButton disabled={!workspace.writable || messageBusy || workspace.incomplete} action={() => workspace.run(async () => {
            await workspace.sendMessage(messagePending.text)
            clearSentDraft(messagePending.conversation_id, messagePending.text)
          })}>重试原消息</ActionButton>
          <ActionButton disabled={!workspace.writable || messageBusy} action={() => workspace.run(() => workspace.releaseMessagePending())}>核对未提交并解除</ActionButton>
        </section>
      ) : null}
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={messageBusy}
        onChange={(value) => setDrafts((current) => new Map(current).set(conversationId, value))}
        onSubmit={() => {
          if (sendDisabled || !text.trim()) return
          void workspace.run(async () => {
            await workspace.sendMessage(text)
            clearSentDraft(conversationId, text)
          })
        }}
      />
    </section>
  )
}
