import { useEffect, useRef, useState } from 'react'
import { Reasoning } from '@/components/ai-elements/reasoning'
import { Task } from '@/components/ai-elements/task'
import { Tool } from '@/components/ai-elements/tool'
import { StatusOrb } from '@/components/status-orb'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Trace, TraceCollapsible, TraceCollapsibleContent, TraceCollapsibleTrigger, TraceLine, TraceTitle } from '@/components/ui/trace'
import type { OrbPlace } from './orb-state'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton } from '@/features/workspace/present'
import { Chat } from './elements/chat'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import { AgentTaskActivity, NativeSessionActivity, PendingCheckItem } from '@/features/workspace/task-activity'
import { stateLabel } from '@/features/workspace/labels'
import type { AutonomyMode } from '@/features/workspace/types'
import { ChatSkeleton } from '@/features/workspace/workspace-shell'
import { ManagerStatus, managerTraceStatus, managerTraceTitle } from './elements/manager-status'
import './chat.css'

export function ConversationScreen() {
  const workspace = useWorkspace()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const text = drafts.get(conversationId) ?? ''
  const originalEntry = !messagePending || messagePending.entry_node === workspace.info.peer_id
  const jobs = data?.scheduler?.jobs ?? []
  const activeJobs = jobs.filter((job) => ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling'].includes(job.state))
  const pendingChecks = data?.pending_checks ?? []
  const visibleJobs = activeJobs.length ? activeJobs : jobs.slice(-1)
  const liveMessages = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (['completed', 'failed', 'cancelled', 'interrupted', 'rejected'].includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id)))).map((progress) => ({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.text, created_at_ms: 0 }))
  const messages = [...(data?.messages ?? []), ...liveMessages]
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const agentReady = workspace.nodes.some((node) => node.online && !node.revoked && node.capabilities?.manager)
  const sendDisabled = !workspace.writable || !workspace.selectedBot || !agentReady || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const assistantBusy = activeJobs.some((job) => ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling'].includes(job.state))
  const stopJob = jobs.find((job) => workspace.data?.scheduler?.capabilities?.cancel === true && ['queued', 'claimed', 'running'].includes(job.state))
  const pendingShown = messagePending && !messages.some((message) => message.message_id === messagePending.message_id)
    ? [{ message_id: messagePending.message_id, event_hash: messagePending.message_id, role: 'user', text: messagePending.text, created_at_ms: 0 }]
    : []
  const shown = [...messages, ...pendingShown]
  const needsCheck = Boolean(messagePending && !messageBusy && !assistantBusy) || pendingChecks.length > 0 || data?.agent_tasks.some((item) => item.state === 'pending_check')
  const hint = !workspace.connectionOnline ? '连接中断，稍等。'
    : needsCheck && messagePending ? (!originalEntry ? '上一条还没确认送达，请回到原来的入口核对。' : messagePending.rejected ? '上一条没有发出。展开上方「需要核对」重试或解除。' : '上一条还没确认送达。展开上方「需要核对」核对或重试。')
    : workspace.writeNotice || (!workspace.selectedBot ? '' : composerHint(gate))
      || (!agentReady ? '还没有可用的 Agent。请到「设置」为节点启用管理 Agent。' : '')
      || (pendingChecks.length ? '有待核对的工作。展开上方「需要核对」处理。' : '')
      || (stopJob ? '正在回复。可以先写下一条，回复结束后再发送。' : '')
  const hasDetails = Boolean(data && (visibleJobs.length > 0 || pendingChecks.length > 0 || data.agent_tasks.length > 0 || data.native_sessions.length > 0 || messagePending))
  const reasoning = workspace.liveProgress.find((item) => item.kind === 'manager' && item.conversation_id === conversationId && item.status)?.status
  const awaitingDecision = data?.native_sessions.some((session) => session.pending_requests.length > 0)
  const workRunning = data?.agent_tasks.some((task) => ['pending_delivery', 'queued', 'claimed', 'accepted', 'running', 'cancelling'].includes(task.state)) || data?.native_sessions.some((session) => ['running', 'waiting', 'interrupting'].includes(session.state))
  const multiStep = visibleJobs.length + (data?.agent_tasks.length ?? 0) + (data?.native_sessions.length ?? 0) > 1
  const tracePlace: OrbPlace | null = needsCheck ? 'reconcile' : workRunning ? 'wait' : multiStep ? 'trace' : null
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
      {workspace.selectedBot && !data ? <ChatSkeleton /> : <Chat
        key={conversationId}
        messages={shown}
        emptyTitle={workspace.selectedBot ? workspace.selectedBot.name : workspace.bots.length ? '选择一个机器人' : '还没有机器人'}
        emptyDetail={workspace.selectedBot ? workspace.selectedBot.instructions || '发一条消息开始聊。' : workspace.bots.length ? '从侧栏选择机器人，开始对话。' : '先创建一个机器人，再开始对话。'}
        emptyMark={workspace.selectedBot ? <StatusOrb place="idle-chat" size={20} /> : undefined}
      >
        {reasoning ? <Reasoning text={reasoning} running={assistantBusy} /> : null}
        {(messageBusy || assistantBusy) && liveMessages.length === 0 ? <p role="status" className="replying" aria-label={waitLabel}><StatusOrb place={assistantBusy ? 'reply' : 'wait'} size={20} /><span>{waitLabel}</span></p> : null}
      </Chat>}
      {awaitingDecision || hasDetails ? <div className="chat-activity">
      {awaitingDecision ? <div className="chat-approvals">
        {data?.native_sessions.map((session) => <NativeSessionActivity key={`approval-${session.target_node}:${session.work_id}`} session={session} compact />)}
      </div> : null}
      {hasDetails ? <div>
        <Trace>
          <TraceCollapsible defaultOpen={false}>
            <TraceCollapsibleTrigger>
              <TraceLine status={needsCheck || awaitingDecision ? 'pending' : workRunning || assistantBusy ? 'running' : undefined}>
                {tracePlace ? <StatusOrb place={tracePlace} size={20} /> : null}
                <TraceTitle>{needsCheck ? '需要核对' : awaitingDecision ? '等待本人决定' : workRunning || assistantBusy ? '工作进行中' : '工作进展'}</TraceTitle>
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
                {pendingChecks.map((check) => <Task key={`${check.target_node}:${check.check_id}`} title={check.kind === 'native_turn' ? '原生工作待核对' : '管理进程待核对'} status="pending"><PendingCheckItem check={check} /></Task>)}
                {visibleJobs.map((job) => <Tool key={job.trigger} title={managerTraceTitle(job)} status={managerTraceStatus(job.state)}>
                  <ManagerStatus job={job} progress={workspace.liveProgress.find((item) => item.kind === 'manager' && item.trigger === job.trigger)?.status} hasLiveProgress={workspace.liveProgress.some((item) => item.kind === 'manager' && item.trigger === job.trigger)} />
                </Tool>)}
                {data?.agent_tasks.map((task) => <Task key={task.task_ref} title={`Agent 任务 · ${stateLabel(task.state)}`} status={task.state === 'failed' ? 'error' : task.state === 'pending_check' ? 'pending' : ['completed', 'interrupted', 'cancelled'].includes(task.state) ? undefined : 'running'}>
                  <AgentTaskActivity task={task} sessions={data.native_sessions.filter((session) => session.task_ref === task.task_ref)} />
                </Task>)}
                {data?.native_sessions.filter((session) => !session.task_ref || !data.agent_tasks.some((task) => task.task_ref === session.task_ref)).map((session) => <Task key={`${session.target_node}:${session.work_id}`} title={`原生工作 · ${stateLabel(session.state)}`} status={session.state === 'failed' ? 'error' : session.state === 'unknown' || session.state === 'waiting' ? 'pending' : ['running', 'interrupting'].includes(session.state) ? 'running' : undefined}>
                  <NativeSessionActivity session={session} />
                </Task>)}
              </Trace>
            </TraceCollapsibleContent>
          </TraceCollapsible>
        </Trace>
      </div> : null}
      </div> : null}
      {data && workspace.selectedBot ? <div className="autonomy-control">
        <label htmlFor="autonomy-mode">收到结果后</label>
        <NativeSelect id="autonomy-mode" size="sm" value={data.autonomy.mode} disabled={!workspace.writable || data.autonomy.version === null} onChange={(event) => {
          const mode = event.currentTarget.value as AutonomyMode
          void workspace.run(() => workspace.setAutonomy(mode))
        }}>
          <NativeSelectOption value="off">停止后续</NativeSelectOption>
          <NativeSelectOption value="report">只汇报</NativeSelectOption>
          <NativeSelectOption value="continue">继续工作</NativeSelectOption>
        </NativeSelect>
        {data.autonomy.version === null ? <span>授权记录无法读取</span> : null}
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
