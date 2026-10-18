import { useEffect, useRef, useState } from 'react'
import { ChevronDownIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton } from '@/features/workspace/present'
import { Chat } from './elements/chat'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import { ExecutionActivity, PendingExecutionControl, UnknownOperationActivity } from '@/features/workspace/execution-activity'
import { ManagerStatus } from './elements/manager-status'
import './chat.css'

export function ConversationScreen() {
  const workspace = useWorkspace()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const text = drafts.get(conversationId) ?? ''
  const originalEntry = !messagePending || messagePending.entry_node === workspace.info.node_id
  const jobs = data?.scheduler?.jobs ?? []
  const activeJobs = jobs.filter((job) => ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling', 'unknown'].includes(job.state))
  const visibleJobs = activeJobs.length ? activeJobs : jobs.slice(-1)
  const liveMessages = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (['completed', 'failed', 'cancelled', 'unknown', 'rejected'].includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id)))).map((progress) => ({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.needsRefresh ? `实时片段不完整，等待最终结果。\n\n${progress.text}` : progress.text, created_at_ms: 0 }))
  const messages = [...(data?.messages ?? []), ...liveMessages]
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const sendDisabled = !workspace.writable || !workspace.selectedBot || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const hint = !workspace.connectionOnline ? '正在恢复只读连接。未确认操作保留原请求，不会自动重发。' : messagePending ? originalEntry ? '先核对原消息，再发送新内容。' : '原消息尚未确认，请回原入口核对或重试；当前入口可只读查看进展。' : workspace.writeNotice || (!workspace.selectedBot ? conversationId ? '旧聊天只读。可在设置中将它接入一个机器人。' : '创建或选择一个机器人，开始聊天。' : composerHint(gate)) || (data?.scheduler?.write_frozen ? '旧调用结果待核对，新的消息只允许只读交谈。' : activeJobs.some((job) => ['queued', 'claimed', 'running'].includes(job.state)) ? '可继续发送；新消息会可靠排队，后台任务继续运行。' : '')

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
    <section className="conversation-screen" aria-label="对话">
      <Chat
        key={conversationId}
        messages={messages}
        emptyTitle={workspace.selectedBot ? `你好，我是${workspace.selectedBot.name}` : '选择一个机器人'}
        emptyDetail={workspace.selectedBot?.instructions || '创建机器人，告诉它需要帮你做什么。'}
      >
        {data && (visibleJobs.length > 0 || data.executions.length > 0 || workspace.pending.some((item) => item.conversation_id === conversationId) || data.operations.some((item) => item.state === 'unknown')) ? <div className="grid gap-4" aria-label="工作进展">
          {workspace.liveProgress.some((item) => item.needsRefresh) ? <p role="status" className="text-sm text-muted-foreground">正在核对完整进展，最终结果会在此显示。</p> : null}
          {visibleJobs.map((job) => <ManagerStatus key={job.trigger} job={job} progress={workspace.liveProgress.find((item) => item.kind === 'manager' && item.trigger === job.trigger)?.status} hasLiveProgress={workspace.liveProgress.some((item) => item.kind === 'manager' && item.trigger === job.trigger)} />)}
          {data.executions.map((execution) => {
            const progress = workspace.liveProgress.find((item) => item.kind === 'execution' && item.session_id === execution.session_id && item.target_node === execution.target_node && item.operation_id === execution.operation_id)
            const projected = progress ? { ...execution, latest_progress: progress.text || progress.status || execution.latest_progress } : execution
            return <ExecutionActivity key={execution.operation_id} execution={projected} conversationId={conversationId} />
          })}
          {workspace.pending.filter((item) => item.conversation_id === conversationId && !data.executions.some((execution) => item.execution_control?.expected_operation_id === execution.operation_id || execution.session_id !== null && item.target_node === execution.target_node && item.session_id === execution.session_id)).map((operation) => <PendingExecutionControl key={operation.operation_id} operation={operation} />)}
          {data.operations.filter((operation) => operation.state === 'unknown' && !data.executions.some((execution) => execution.operation_id === operation.request.operation_id)).map((operation) => <UnknownOperationActivity key={operation.request.operation_id} operation={operation} />)}
        </div> : null}
      </Chat>
      {messagePending ? (
        <section className="pending-message" aria-label="待核对消息">
          <h3 role="status">{messageBusy ? '正在提交消息…' : jobs.some((job) => job.message_id === messagePending.message_id && job.target_node === messagePending.scheduler_node && job.state === 'pending_delivery') ? '消息已提交，等待助手接收' : '消息发送结果待核对'}</h3>
          <Collapsible className="pending-message-details">
            <CollapsibleTrigger asChild>
              <Button variant="ghost" size="sm" className="group -ms-2">
                查看原消息
                <ChevronDownIcon aria-hidden="true" className="group-data-[state=open]:rotate-180" />
              </Button>
            </CollapsibleTrigger>
            <CollapsibleContent>
              <p className="whitespace-pre-wrap break-words text-sm">{messagePending.text}</p>
              <p className="text-sm text-muted-foreground">重试会保留原消息。{originalEntry ? '' : '请回原入口重试。'}</p>
            </CollapsibleContent>
          </Collapsible>
          <div className="pending-message-actions">
            <ActionButton variant="default" disabled={messageBusy} action={() => workspace.run(() => workspace.refresh())}>核对原消息</ActionButton>
            <ActionButton disabled={!workspace.writable || !originalEntry || messageBusy || workspace.incomplete} action={() => workspace.run(async () => {
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
