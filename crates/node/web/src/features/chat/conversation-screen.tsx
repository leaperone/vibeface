import { useEffect, useRef, useState } from 'react'
import { ChevronDownIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
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
  const hint = messagePending ? '先核对原消息，再发送新内容。' : workspace.writeNotice || composerHint(gate)

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
        key={conversationId}
        messages={messages}
        emptyTitle={data ? '想完成什么？' : '选择一个会话'}
        emptyDetail={data ? '发一条消息，开始与调度 Agent 协作。' : '从会话列表中选择，或新建会话。'}
      />
      {messagePending ? (
        <section className="pending-message" aria-label="待核对消息">
          <h3>{messageBusy ? '正在提交消息…' : '消息发送结果待核对'}</h3>
          <Collapsible className="pending-message-details">
            <CollapsibleTrigger asChild>
              <Button variant="ghost" size="sm" className="group -ms-2">
                查看原消息
                <ChevronDownIcon aria-hidden="true" className="group-data-[state=open]:rotate-180" />
              </Button>
            </CollapsibleTrigger>
            <CollapsibleContent>
              <DetailList pairs={[
                ['消息编号', messagePending.message_id],
                ['原调度设备', messagePending.scheduler_node],
                ['内容', messagePending.text],
              ]} />
              <p>重试会保留原编号、内容和调度设备。</p>
            </CollapsibleContent>
          </Collapsible>
          <div className="pending-message-actions">
            <ActionButton variant="default" disabled={messageBusy} action={() => workspace.run(() => workspace.refresh())}>核对原消息</ActionButton>
            <ActionButton disabled={!workspace.writable || messageBusy || workspace.incomplete} action={() => workspace.run(async () => {
              await workspace.sendMessage(messagePending.text)
              clearSentDraft(messagePending.conversation_id, messagePending.text)
            })}>重试原消息</ActionButton>
            <ActionButton variant="ghost" disabled={!workspace.writable || messageBusy} action={() => workspace.run(() => workspace.releaseMessagePending())}>核对未提交并解除</ActionButton>
          </div>
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
