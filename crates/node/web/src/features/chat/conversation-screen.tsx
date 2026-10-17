import { useEffect, useRef, useState } from 'react'
import { ChevronDownIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { useWorkspace } from '@/features/workspace/context'
import { ActionButton, DetailList } from '@/features/workspace/present'
import { Chat } from './elements/chat'
import { PromptInput } from './elements/prompt-input'
import { canSend, composerHint } from './protocol'
import { Checkbox } from '@/components/ui/checkbox'
import { FieldLabel } from '@/components/ui/field'
import { TaskActivity } from '@/features/workspace/task-activity'
import { ManagerStatus } from './elements/manager-status'
import './chat.css'

export function ConversationScreen() {
  const workspace = useWorkspace()
  const [drafts, setDrafts] = useState<Map<string, string>>(() => new Map())
  const sentDrafts = useRef(new Map<string, { message_id: string; text: string }>())
  const [reportResults, setReportResults] = useState(false)
  const { conversationId, data, messagePending, messageBusy, chatSlot } = workspace
  const text = drafts.get(conversationId) ?? ''
  const originalEntry = !messagePending || messagePending.entry_node === workspace.info.node_id
  const jobs = data?.scheduler?.jobs ?? []
  const activeJobs = jobs.filter((job) => ['pending_delivery', 'queued', 'claimed', 'running', 'cancelling', 'unknown'].includes(job.state))
  const visibleJobs = activeJobs.length ? activeJobs : jobs.slice(-1)
  const liveMessages = workspace.liveProgress.filter((progress) => progress.kind === 'manager' && progress.conversation_id === conversationId && progress.text && !jobs.some((job) => job.trigger === progress.trigger && (['completed', 'failed', 'cancelled', 'unknown', 'rejected'].includes(job.state) || job.reply_message_id && data?.messages.some((message) => message.message_id === job.reply_message_id)))).map((progress) => ({ message_id: progress.key, event_hash: progress.key, role: 'assistant', text: progress.needsRefresh ? `实时片段不完整，等待最终结果。\n\n${progress.text}` : progress.text, created_at_ms: 0 }))
  const messages = [...(data?.messages ?? []), ...liveMessages]
  const gate = { ...chatSlot, hasConversation: Boolean(conversationId), loaded: Boolean(data) }
  const sendDisabled = !workspace.writable || Boolean(messagePending) || !canSend({ ...gate, busy: messageBusy })
  const hint = !workspace.connectionOnline ? '正在恢复只读连接。未确认操作保留原编号，不会自动重发。' : messagePending ? originalEntry ? '先核对原消息，再发送新内容。' : '原消息尚未确认，请回原入口核对或重试；当前入口可只读查看进展。' : workspace.writeNotice || composerHint(gate) || (data?.scheduler?.write_frozen ? '旧调用结果待核对，新的消息只允许只读交谈。' : activeJobs.some((job) => ['queued', 'claimed', 'running'].includes(job.state)) ? '可继续发送；新消息会可靠排队，后台任务继续运行。' : '')

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
      >
        {data && (visibleJobs.length > 0 || data.tasks.length > 0 || workspace.liveProgress.some((item) => item.needsRefresh)) ? <div className="grid gap-4" aria-label="当前任务与调度状态">
          {data.scheduler ? <p className="text-xs text-muted-foreground">调度：{data.scheduler.provider ?? '提供方未报告'}{data.scheduler.model ? ` · ${data.scheduler.model}` : ''}。{data.scheduler.capabilities ? `${data.scheduler.capabilities.streaming ? '支持实时回复' : '此调度端未提供实时回复'}；${data.scheduler.capabilities.cancel ? '支持停止当前回复' : '此调度端不支持停止当前回复'}。` : '当前入口未报告可用的调度控制能力。'}</p> : <p className="text-sm text-muted-foreground">当前入口未提供持久队列状态，请保留待核对消息。</p>}
          {workspace.liveProgress.some((item) => item.needsRefresh) ? <p role="status" className="text-sm text-muted-foreground">实时片段不完整，正在核对持久状态；最终结果以已同步记录为准。</p> : null}
          {visibleJobs.map((job) => <ManagerStatus key={job.trigger} job={job} progress={workspace.liveProgress.find((item) => item.kind === 'manager' && item.trigger === job.trigger)?.status} hasLiveProgress={workspace.liveProgress.some((item) => item.kind === 'manager' && item.trigger === job.trigger)} />)}
          {data.tasks.map((task) => {
            const binding = task.binding
            const progress = workspace.liveProgress.find((item) => item.kind === 'execution' && binding && item.session_id === binding.session_id && item.target_node === binding.target_node && (!binding.operation_id || item.operation_id === binding.operation_id) && (!item.task_id || item.task_id === task.task_id))
            const projected = binding && progress ? { ...task, binding: { ...binding, latest_progress: progress.text || progress.status || binding.latest_progress } } : task
            return <TaskActivity key={task.task_id} task={projected} conversationId={conversationId} />
          })}
        </div> : null}
      </Chat>
      {messagePending ? (
        <section className="pending-message" aria-label="待核对消息">
          <h3 role="status">{messageBusy ? '正在提交消息…' : jobs.some((job) => job.message_id === messagePending.message_id && job.target_node === messagePending.scheduler_node && job.state === 'pending_delivery') ? '入口已接收，等待目标调度端入队' : '消息发送结果待核对'}</h3>
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
              <p>完成后只读汇报：{messagePending.report_results ? '本人已授权' : '未授权'}。</p>
              <p>重试会保留原编号、内容、调度设备和原入口。{originalEntry ? '' : '请回原入口重试；当前入口只能核对已同步进展。'}</p>
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
      <div className="mx-auto flex w-full max-w-3xl items-center gap-2 px-4 pt-2 sm:px-6">
        <Checkbox id="message-report-results" checked={messagePending?.report_results ?? reportResults} disabled={!workspace.writable || Boolean(messagePending) || messageBusy} onCheckedChange={(checked) => setReportResults(checked === true)} />
        <FieldLabel htmlFor="message-report-results" className="text-xs font-normal">由这条消息派出的任务完成后，自动只读汇报</FieldLabel>
      </div>
      <PromptInput
        value={text}
        hint={hint}
        disabled={sendDisabled}
        busy={messageBusy}
        onChange={(value) => setDrafts((current) => new Map(current).set(conversationId, value))}
        onSubmit={() => {
          if (sendDisabled || !text.trim()) return
          void workspace.run(async () => {
            await workspace.sendMessage(text, reportResults)
            clearSentDraft(conversationId, text)
          })
        }}
      />
    </section>
  )
}
