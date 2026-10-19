import { useEffect, useRef, useState } from 'react'
import { Reasoning } from '@/components/ai-elements/reasoning'
import { Task } from '@/components/ai-elements/task'
import { Tool } from '@/components/ai-elements/tool'
import { StatusOrb } from '@/components/status-orb'
import { Trace, TraceCollapsible, TraceCollapsibleContent, TraceCollapsibleTrigger, TraceLine, TraceTitle } from '@/components/ui/trace'
import type { OrbPlace } from './orb-state'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton } from '@/features/workspace/present'
import { Chat } from './elements/chat'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import { ExecutionActivity, PendingExecutionControl, UnknownOperationActivity } from '@/features/workspace/execution-activity'
import { ManagerStatus, managerTraceStatus, managerTraceTitle } from './elements/manager-status'
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
  const liveMessages = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (['completed', 'failed', 'cancelled', 'unknown', 'rejected'].includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id)))).map((progress) => ({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0 }))
  const messages = [...(data?.messages ?? []), ...liveMessages]
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const sendDisabled = !workspace.writable || !workspace.selectedBot || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const assistantBusy = activeJobs.some((job) => ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling'].includes(job.state))
  const stopJob = jobs.find((job) => workspace.data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const pendingShown = messagePending && !messages.some((message) => message.message_id === messagePending.message_id)
    ? [{ message_id: messagePending.message_id, event_hash: messagePending.message_id, role: 'user', text: messagePending.text, created_at_ms: 0 }]
    : []
  const shown = [...messages, ...pendingShown]
  const needsCheck = Boolean(messagePending && !messageBusy && !assistantBusy) || Boolean(data?.scheduler?.write_frozen) || data?.operations.some((item) => item.state === 'unknown') || workspace.pending.some((item) => item.conversation_id === conversationId)
  const hint = !workspace.connectionOnline ? '连接中断，稍等。' : needsCheck && messagePending ? (originalEntry ? '上一条还没发出。' : '请回到原来的入口。') : workspace.writeNotice || (!workspace.selectedBot ? '' : composerHint(gate)) || (data?.scheduler?.write_frozen ? '先核对上一条。' : '')
  const hasDetails = Boolean(data && (visibleJobs.length > 0 || data.executions.length > 0 || workspace.pending.some((item) => item.conversation_id === conversationId) || data.operations.some((item) => item.state === 'unknown') || messagePending))
  const reasoning = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  const planning = data?.executions.some((execution) => execution.goal && !['completed', 'failed', 'cancelled', 'rejected'].includes(execution.state)) ?? false
  const multiStep = visibleJobs.length + (data?.executions.length ?? 0) > 1
  const tracePlace: OrbPlace | null = needsCheck ? 'reconcile' : planning ? 'plan' : multiStep ? 'trace' : null
  const waitLabel = messageBusy && !assistantBusy ? '稍等' : '正在回复'

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
        messages={shown}
        emptyTitle={workspace.selectedBot ? workspace.selectedBot.name : '选择一个机器人'}
        emptyDetail={workspace.selectedBot?.instructions || '发一条消息开始聊。'}
        emptyMark={workspace.selectedBot ? <StatusOrb place="idle-chat" size={20} /> : undefined}
      >
        {reasoning ? <Reasoning text={reasoning} running={assistantBusy} /> : null}
        {data?.executions.map((execution) => {
          const progress = workspace.liveProgress.find((item) => item.kind === 'execution' && item.session_id === execution.session_id && item.target_node === execution.target_node && item.operation_id === execution.operation_id)
          const projected = progress ? { ...execution, latest_progress: progress.text || progress.status || execution.latest_progress } : execution
          return <ExecutionActivity key={execution.operation_id} execution={projected} conversationId={conversationId} compact />
        })}
        {(messageBusy || assistantBusy) && liveMessages.length === 0 ? <p role="status" className="replying" aria-label={waitLabel}><StatusOrb place={assistantBusy ? 'reply' : 'wait'} size={20} /><span>{waitLabel}</span></p> : null}
      </Chat>
      {hasDetails ? <div className="chat-trace">
        <Trace>
          <TraceCollapsible defaultOpen={false}>
            <TraceCollapsibleTrigger>
              <TraceLine status={needsCheck ? 'pending' : undefined}>
                {tracePlace ? <StatusOrb place={tracePlace} size={20} /> : null}
                <TraceTitle>{needsCheck ? '需要核对' : '详情'}</TraceTitle>
              </TraceLine>
            </TraceCollapsibleTrigger>
            <TraceCollapsibleContent>
              {messagePending ? <div className="pending-message-actions">
                <ActionButton variant="default" disabled={messageBusy} action={() => workspace.run(() => workspace.refresh())}>核对</ActionButton>
                <ActionButton disabled={!workspace.writable || !originalEntry || messageBusy || workspace.incomplete} action={() => workspace.run(async () => {
                  if (await workspace.sendMessage(messagePending.text) && workspace.isCurrent()) clearSentDraft(messagePending.conversation_id, messagePending.text)
                })}>重试</ActionButton>
                <ActionButton variant="ghost" disabled={!workspace.writable || messageBusy} action={() => workspace.run(() => workspace.releaseMessagePending())}>解除</ActionButton>
              </div> : null}
              <Trace>
                {visibleJobs.map((job) => <Tool key={job.trigger} title={managerTraceTitle(job)} status={managerTraceStatus(job.state)}>
                  <ManagerStatus job={job} progress={workspace.liveProgress.find((item) => item.kind === 'manager' && item.trigger === job.trigger)?.status} hasLiveProgress={workspace.liveProgress.some((item) => item.kind === 'manager' && item.trigger === job.trigger)} />
                </Tool>)}
                {data?.executions.map((execution) => {
                  const progress = workspace.liveProgress.find((item) => item.kind === 'execution' && item.session_id === execution.session_id && item.target_node === execution.target_node && item.operation_id === execution.operation_id)
                  const projected = progress ? { ...execution, latest_progress: progress.text || progress.status || execution.latest_progress } : execution
                  const running = ['running', 'accepted', 'claimed'].includes(execution.state)
                  const failed = execution.state === 'failed' || execution.state === 'rejected'
                  const plan = Boolean(execution.goal) && !['completed', 'failed', 'cancelled', 'rejected'].includes(execution.state)
                  return <Task key={`detail-${execution.operation_id}`} title={execution.title || '执行'} status={failed ? 'error' : execution.state === 'unknown' ? 'pending' : running ? 'running' : undefined} mark={plan ? <StatusOrb place="plan" size={20} /> : running ? <StatusOrb place="wait" size={20} /> : undefined}>
                    <ExecutionActivity execution={projected} conversationId={conversationId} />
                  </Task>
                })}
                {data ? workspace.pending.filter((item) => item.conversation_id === conversationId && !data.executions.some((execution) => item.execution_control?.expected_operation_id === execution.operation_id || execution.session_id !== null && item.target_node === execution.target_node && item.session_id === execution.session_id)).map((operation) => <Tool key={operation.operation_id} title="操作待核对" status="pending"><PendingExecutionControl operation={operation} /></Tool>) : null}
                {data?.operations.filter((operation) => operation.state === 'unknown' && !data.executions.some((execution) => execution.operation_id === operation.request.operation_id)).map((operation) => <Tool key={operation.request.operation_id} title="结果待核查" status="pending"><UnknownOperationActivity operation={operation} /></Tool>)}
              </Trace>
            </TraceCollapsibleContent>
          </TraceCollapsible>
        </Trace>
      </div> : null}
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={messageBusy}
        onStop={stopJob && workspace.writable && workspace.selectedBot ? () => { void workspace.run(() => workspace.cancelManager(stopJob.trigger)) } : undefined}
        onChange={(value) => setDrafts((current) => new Map(current).set(conversationId, value))}
        onSubmit={() => {
          if (sendDisabled || !text.trim()) return
          void workspace.run(async () => {
            if (await workspace.sendMessage(text) && workspace.isCurrent()) clearSentDraft(conversationId, text)
          })
        }}
      />
    </section>
  )
}
