"use client"

import { useId, useState } from "react"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { ActionButton, StatusBadge, SubmitButton } from "./present"
import type { AgentTaskRecord, HumanRequest, NativeSessionRecord } from "./types"

export function AgentTaskActivity({ task, sessions }: { task: AgentTaskRecord; sessions: NativeSessionRecord[] }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const lookup = { nodes: workspace.nodes, directory }
  const canCancel = ["pending_delivery", "queued", "claimed", "accepted", "running", "unknown"].includes(task.state)
  const canCancelHere = ready?.entry.peer_id === task.source_node || ready?.entry.peer_id === task.target_node
  return <section className="grid min-w-0 gap-3" aria-label="Agent 任务">
    <div className="flex flex-wrap items-center gap-2"><StatusBadge value={task.state} /><span className="text-xs text-muted-foreground">由 {nodeName(task.source_node, lookup)} 交给 {nodeName(task.target_node, lookup)}</span></div>
    <p className="whitespace-pre-wrap break-words text-sm">{task.text}</p>
    {task.detail && task.detail !== task.text ? <p className="whitespace-pre-wrap break-words text-sm">{task.detail}</p> : null}
    {task.state === "unknown" ? <p role="status" className="text-sm">结果未知。请核对原任务和外部结果，避免重复派发。</p> : null}
    {canCancel ? <div><ActionButton className="min-h-11" variant="destructive" disabled={!workspace.writable || !workspace.selectedBot || !canCancelHere} action={async () => { if (canCancelHere) await workspace.run(() => workspace.cancelAgentTask(task.task_ref)) }}>请求取消任务</ActionButton></div> : null}
    {canCancel && !canCancelHere ? <p className="text-sm text-muted-foreground">请连接任务来源或目标设备后取消任务。</p> : null}
    {sessions.map((session) => <NativeSessionActivity key={`${session.target_node}:${session.work_id}`} session={session} />)}
  </section>
}

export function NativeSessionActivity({ session, compact = false }: { session: NativeSessionRecord; compact?: boolean }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const targetOnline = workspace.nodes.some((node) => node.peer_id === session.target_node && node.online && !node.revoked)
  const active = session.state === "running" || session.state === "waiting"
  const canControl = workspace.writable && Boolean(workspace.selectedBot) && targetOnline && active && session.freshness === "observed" && session.can_control
  const progress = workspace.liveProgress.find((item) => item.kind === "native" && item.work_id === session.work_id && item.target_node === session.target_node && item.thread_id === session.thread_id && (item.turn_id ?? null) === session.turn_id)
  const approvals = session.pending_requests.map((request) => <NativeApproval key={`${typeof request.request_id}:${request.request_id}`} session={session} request={request} disabled={!canControl} />)
  if (compact) return approvals.length ? <div className="grid gap-3">{approvals}</div> : null
  return <section className="grid min-w-0 gap-3 rounded-lg border p-3" aria-label="原生工作">
    <div className="flex flex-wrap items-center gap-2"><StatusBadge value={session.state} label={session.freshness === "observed" && session.pending_requests.length && active ? "等待本人决定" : undefined} /><span className="text-xs text-muted-foreground">{nodeName(session.target_node, { nodes: workspace.nodes, directory })} · 原生工作</span></div>
    {session.freshness === "cached" ? <p role="status" className="text-sm text-muted-foreground">上次记录的状态与进展，当前待核对。确认最新进展后才能操作。</p> : null}
    {progress?.status ? <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{progress.status}</p> : null}
    {progress?.text ? <p className="whitespace-pre-wrap break-words text-sm">{progress.text}</p> : null}
    {session.detail && session.detail !== progress?.text ? <p className="whitespace-pre-wrap break-words text-sm">{session.detail}</p> : null}
    {session.state === "unknown" ? <p role="status" className="text-sm">结果未知。请核对原生进程和外部结果，不会自动重跑。</p> : null}
    {!targetOnline ? <p role="status" className="text-sm text-muted-foreground">设备恢复连接后可核对最新进展。</p> : null}
    {active && !session.turn_id ? <p className="text-sm text-muted-foreground">尚未关联当前轮次，暂不能停止。</p> : null}
    <div className="flex flex-wrap gap-2">
      <ActionButton className="min-h-11" disabled={!workspace.connectionOnline} action={() => workspace.run(() => workspace.refresh())}>核对进展</ActionButton>
      {active && session.turn_id ? <ActionButton className="min-h-11" variant="destructive" disabled={!canControl} action={async () => { if (canControl) await workspace.run(() => workspace.interruptNative(session.work_id, session.target_node)) }}>停止原生工作</ActionButton> : null}
    </div>
    {approvals}
  </section>
}

export function NativeApproval({ session, request, disabled }: { session: NativeSessionRecord; request: HumanRequest; disabled: boolean }) {
  const workspace = useWorkspace()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === workspace.generation
  const blocked = disabled || session.freshness !== "observed" || !session.can_control
  const id = useId()
  const questions = request.questions ?? []
  return (
    <section className="space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3" aria-label="人工审批">
      <h4 className="text-sm font-semibold">{request.title || "等待本人决定"}</h4>
      <p className="whitespace-pre-line break-words text-sm">{request.text}</p>
      {session.freshness === "cached" ? <p role="status" className="text-sm text-muted-foreground">这是上次记录的请求，待核对当前状态后才能回复。</p> : null}
      {!questions.some((question) => question.is_secret) && request.choices?.length ? (
        <div className="flex flex-wrap gap-2">
          {request.choices.map((choice, index) => <ActionButton key={index} className="min-h-11" disabled={busy || blocked} action={async () => {
            if (blocked || busy || !workspace.writable) return
            const generation = workspace.generation
            setBusyGeneration(generation)
            try {
              await workspace.run(async () => { await workspace.respondNative(session.work_id, session.target_node, request.request_id, choice.response) })
            } finally {
              setBusyGeneration(owner => owner === generation ? null : owner)
            }
          }}>{choice.label}</ActionButton>)}
        </div>
      ) : questions.length > 0 && !questions.some((question) => question.is_secret) ? (
        <form onSubmit={(event) => {
          event.preventDefault()
          if (blocked || busy || !workspace.writable) return
          const form = event.currentTarget
          if (!form.reportValidity()) return
          const values = new FormData(form)
          const generation = workspace.generation
          setBusyGeneration(generation)
          void workspace.run(async () => {
            const answers = Object.fromEntries(questions.map((question, index) => {
              const answer = String(values.get(`answer-${index}`) ?? '').trim()
              if (!answer) throw new Error('请回答每个问题后再提交。')
              return [question.id, { answers: [answer] }]
            }))
            await workspace.respondNative(session.work_id, session.target_node, request.request_id, { answers })
          }).finally(() => setBusyGeneration(owner => owner === generation ? null : owner))
        }}>
          <fieldset disabled={blocked || busy || !workspace.writable} className="min-w-0">
            <FieldGroup>
              {questions.map((question, index) => <Field key={question.id}>
                <FieldLabel htmlFor={`${id}-answer-${index}`}>{question.header || question.question}</FieldLabel>
                {question.header ? <p className="text-sm">{question.question}</p> : null}
                {question.options?.length ? <NativeSelect id={`${id}-answer-${index}`} name={`answer-${index}`} defaultValue="" required>
                  <NativeSelectOption value="" disabled>请选择</NativeSelectOption>
                  {question.options.map((option) => <NativeSelectOption key={option.label} value={option.label}>{option.label}{option.description ? ` · ${option.description}` : ''}</NativeSelectOption>)}
                </NativeSelect> : <Textarea id={`${id}-answer-${index}`} name={`answer-${index}`} rows={2} required maxLength={50000} />}
              </Field>)}
              <SubmitButton busy={busy} variant="secondary">发送本人回答</SubmitButton>
            </FieldGroup>
          </fieldset>
        </form>
      ) : <p className="text-sm text-muted-foreground">{questions.some((question) => question.is_secret) ? '此请求包含敏感输入，请在工作设备填写。' : '此请求暂无可用的回复选项，请到工作设备人工核对。'}</p>}
    </section>
  )
}
