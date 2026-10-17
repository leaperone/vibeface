"use client"

import { useId, useState } from "react"
import { ChevronDownIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useWorkspace } from "./context"
import { nodeName, stateNames } from "./labels"
import { ActionButton, DetailList, RawDetails, StatusBadge, SubmitButton } from "./present"
import type { PendingOperation, TaskRecord } from "./types"
import { Artifact } from "../chat/elements/artifact"

const taskStatuses = ["open", "working", "blocked", "needs_review", "accepted", "cancelled"] as const

export function TaskActivity({ task, conversationId }: { task: TaskRecord; conversationId: string }) {
  const workspace = useWorkspace()
  const id = useId()
  const [busy, setBusy] = useState(false)
  const [followupBusy, setFollowupBusy] = useState(false)
  const [steerBusy, setSteerBusy] = useState(false)
  const status = taskStatuses.find((value) => value === task.status) ?? "open"
  const binding = task.binding
  const capabilities = binding?.capabilities ?? []
  const hasFollowupVersion = task.followup_version !== undefined
  const targetOnline = workspace.nodes.some((node) => node.node_id === binding?.target_node && node.online && !node.revoked)
  const pendingControls = workspace.pending.filter((operation) => operation.conversation_id === conversationId && (
    (operation.control?.task_id ?? operation.request?.task_id) === task.task_id ||
    (binding && operation.target_node === binding.target_node && operation.session_id === binding.session_id)
  ))
  const canControl = workspace.writable && targetOnline && Boolean(binding?.operation_id) && binding?.state !== "unknown" && binding?.freshness !== "unknown" && pendingControls.length === 0

  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 flex-1 break-words">{task.title || "未命名工单"}</CardTitle>
        <StatusBadge value={task.status} label={task.status === "accepted" ? "已验收" : undefined} />
      </CardHeader>
      <CardContent className="space-y-3">
        {task.detail ? <p className="line-clamp-3 whitespace-pre-line break-words text-sm text-muted-foreground">{task.detail}</p> : null}
        {task.binding_error ? <p role="status" className="text-sm">执行状态待核对：{task.binding_error}</p> : null}
        {task.followup_error ? <p role="status" className="text-sm">后续授权待核对：{task.followup_error}</p> : null}
        {binding ? (
          <section className="space-y-3 rounded-lg border p-3" aria-label="后台执行">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-sm font-medium">后台执行 · {nodeName(binding.target_node)}</h3>
              <StatusBadge value={binding.state} />
            </div>
            <p className="text-xs text-muted-foreground">{binding.freshness === "cached" ? "上次读取的执行状态" : binding.freshness === "local_ledger" ? "本机账本状态" : "尚未取得当前执行状态"}{binding.checked_at_ms ? ` · ${new Date(binding.checked_at_ms).toLocaleString('zh-CN')}` : ''}</p>
            {binding.latest_progress ? <p className="whitespace-pre-line break-words text-sm">{binding.latest_progress}</p> : null}
            {!targetOnline ? <p role="status" className="text-sm text-muted-foreground">当前只能看到已同步进展；执行设备恢复连接后才能核对实时状态。</p> : null}
            {binding.state === "unknown" ? <p role="status" className="text-sm">执行结果未知，请核对当前进程与外部结果，避免重复执行。</p> : null}
            <div className="flex flex-wrap gap-2">
              <ActionButton disabled={!workspace.connectionOnline} action={() => workspace.run(async () => { await workspace.taskControl(task.task_id, "status") })}>核对执行状态</ActionButton>
              {capabilities.includes("interrupt") ? <ActionButton disabled={!canControl} variant="destructive" action={() => workspace.run(async () => { await workspace.taskControl(task.task_id, "interrupt") })}>停止当前执行</ActionButton> : null}
            </div>
            {capabilities.includes("steer") ? (
              <Collapsible>
                <CollapsibleTrigger asChild>
                  <Button type="button" variant="ghost" disabled={!canControl} className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">补充当前任务<ChevronDownIcon aria-hidden="true" className="size-4 shrink-0 transition-transform" /></Button>
                </CollapsibleTrigger>
                <CollapsibleContent className="pt-3">
                  <form onSubmit={(event) => {
                    event.preventDefault()
                    const form = event.currentTarget
                    if (!form.reportValidity()) return
                    const text = String(new FormData(form).get("text") ?? "").trim()
                    setSteerBusy(true)
                    void workspace.run(async () => {
                      await workspace.taskControl(task.task_id, "steer", { text })
                      form.reset()
                    }).finally(() => setSteerBusy(false))
                  }}>
                    <fieldset disabled={!canControl} className="min-w-0">
                      <FieldGroup>
                        <Field>
                          <FieldLabel htmlFor={`${id}-steer`}>补充说明</FieldLabel>
                          <Textarea id={`${id}-steer`} name="text" rows={3} required maxLength={50000} />
                        </Field>
                        <SubmitButton busy={steerBusy} variant="secondary">发送补充说明</SubmitButton>
                      </FieldGroup>
                    </fieldset>
                  </form>
                </CollapsibleContent>
              </Collapsible>
            ) : null}
            {binding.pending_requests.map((request) => (
              <TaskApproval key={`${typeof request.request_id}:${request.request_id}`} task={task} request={request} canRespond={capabilities.includes("respond_request")} disabled={!canControl} />
            ))}
          </section>
        ) : <p className="text-sm text-muted-foreground">尚未绑定后台执行。</p>}
        {pendingControls.map((operation) => <TaskPendingControl key={operation.operation_id} operation={operation} />)}
        {task.blobs?.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} />)}
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">查看工单详情<ChevronDownIcon aria-hidden="true" className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="space-y-3 pt-3">
            <DetailList pairs={[["目标", task.goal], ["验收条件", task.acceptance], ["完整进展", task.detail || "暂无进展"]]} />
            {binding ? <DetailList pairs={[["执行会话", binding.session_id], ["后台操作", binding.operation_id]]} /> : null}
          </CollapsibleContent>
        </Collapsible>
        <section className="space-y-3" aria-label="后续处理授权">
          <form key={task.followup_mode} onSubmit={(event) => {
            event.preventDefault()
            const form = event.currentTarget
            if (!form.reportValidity()) return
            const mode = String(new FormData(form).get("followup_mode") ?? "")
            if (mode !== "off" && mode !== "report" && mode !== "continue") return
            setFollowupBusy(true)
            void workspace.run(() => workspace.setFollowup(task.task_id, mode)).finally(() => setFollowupBusy(false))
          }}>
            <fieldset disabled={!workspace.writable || !hasFollowupVersion} className="min-w-0">
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor={`${id}-followup`}>后续处理授权</FieldLabel>
                  <NativeSelect id={`${id}-followup`} name="followup_mode" defaultValue={task.followup_mode ?? "off"} aria-describedby={`${id}-followup-help`}>
                    <NativeSelectOption value="off">关闭</NativeSelectOption>
                    <NativeSelectOption value="report" disabled={!workspace.canAutoFollowup}>完成后只读汇报</NativeSelectOption>
                    <NativeSelectOption value="continue" disabled={!workspace.canAutoFollowup}>允许本任务继续执行</NativeSelectOption>
                  </NativeSelect>
                  <p id={`${id}-followup-help`} className="text-sm text-muted-foreground">只读汇报只检查并汇总结果；继续执行允许调度 Agent 为本工单发起后续操作。</p>
                  {!hasFollowupVersion ? <p className="text-sm text-muted-foreground">当前入口尚未提供可核对的授权版本，暂不能更改后续授权。</p> : null}
                  {!workspace.canAutoFollowup ? <p className="text-sm text-muted-foreground">当前入口未启用接入角色，暂不能开启后续处理。</p> : null}
                </Field>
                <SubmitButton busy={followupBusy} variant="secondary">保存授权</SubmitButton>
              </FieldGroup>
            </fieldset>
          </form>
          {task.status !== "accepted" && task.status !== "cancelled" ? (
            <div className="space-y-2">
              <ActionButton disabled={!workspace.writable || !hasFollowupVersion} action={() => workspace.run(() => workspace.cancelFollowup(task))}>取消后续任务</ActionButton>
              <p className="text-sm text-muted-foreground">取消后续会关闭授权并标记工单已取消，不会停止正在运行的外部进程。</p>
            </div>
          ) : null}
        </section>
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">更新工单状态<ChevronDownIcon aria-hidden="true" className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-3">
            <form onSubmit={(event) => {
              event.preventDefault()
              const form = event.currentTarget
              if (!form.reportValidity()) return
              const data = new FormData(form)
              setBusy(true)
              void workspace.run(() => workspace.updateTask(conversationId, task.task_id, String(data.get("status") ?? ""), String(data.get("detail") ?? ""))).finally(() => setBusy(false))
            }}>
              <fieldset disabled={!workspace.writable} className="min-w-0">
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor={`${id}-status`}>状态</FieldLabel>
                    <NativeSelect id={`${id}-status`} name="status" defaultValue={status}>
                      {taskStatuses.map((value) => <NativeSelectOption key={value} value={value}>{value === "accepted" ? "已验收" : stateNames[value]}</NativeSelectOption>)}
                    </NativeSelect>
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-detail`}>说明</FieldLabel>
                    <Textarea id={`${id}-detail`} name="detail" rows={3} required defaultValue={task.detail || ""} />
                  </Field>
                  <SubmitButton busy={busy} variant="secondary">保存状态</SubmitButton>
                </FieldGroup>
              </fieldset>
            </form>
          </CollapsibleContent>
        </Collapsible>
      </CardContent>
    </Card>
  )
}

function TaskPendingControl({ operation }: { operation: PendingOperation }) {
  const workspace = useWorkspace()
  const [result, setResult] = useState<unknown>()
  const online = workspace.nodes.some((node) => node.node_id === operation.target_node && node.online && !node.revoked)
  return (
    <section className="space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3" aria-label="后台操作待核对">
      <h3 className="text-sm font-semibold">后台操作待核对</h3>
      <p role="status" className="text-sm">执行结果待确认，已暂停本任务的执行操作与审批。</p>
      <p className="text-sm">原操作编号：<code className="break-all text-xs">{operation.operation_id}</code></p>
      <div className="flex flex-wrap gap-2">
        <ActionButton disabled={!workspace.writable || !online} action={() => workspace.run(async () => { setResult(await workspace.reconcile(operation)) })}>核对原操作</ActionButton>
        {operation.control ? <ActionButton disabled={!workspace.writable || !online} action={() => workspace.run(async () => { await workspace.retryTaskControl(operation) })}>重试原请求</ActionButton> : null}
      </div>
      {operation.control ? <p className="text-sm text-muted-foreground">重试会保留原操作编号与内容，不会新建请求。</p> : null}
      {result !== undefined ? <RawDetails title="核对结果" value={result} defaultOpen /> : null}
    </section>
  )
}

function TaskApproval({ task, request, canRespond, disabled }: { task: TaskRecord; request: NonNullable<TaskRecord["binding"]>["pending_requests"][number]; canRespond: boolean; disabled: boolean }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const id = useId()
  const questions = request.questions ?? []
  return (
    <section className="space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3" aria-label="人工审批">
      <h4 className="text-sm font-semibold">{request.title || "等待本人决定"}</h4>
      <p className="whitespace-pre-line break-words text-sm">{request.text}</p>
      {canRespond && request.choices?.length ? (
        <div className="flex flex-wrap gap-2">
          {request.choices.map((choice, index) => <ActionButton key={index} disabled={busy || disabled} action={async () => {
            setBusy(true)
            try {
              await workspace.run(async () => { await workspace.taskControl(task.task_id, "respond_request", { request_id: request.request_id, response: choice.response }) })
            } finally {
              setBusy(false)
            }
          }}>{choice.label}</ActionButton>)}
        </div>
      ) : canRespond && questions.length > 0 && !questions.some((question) => question.is_secret) ? (
        <form onSubmit={(event) => {
          event.preventDefault()
          const form = event.currentTarget
          if (!form.reportValidity()) return
          const values = new FormData(form)
          setBusy(true)
          void workspace.run(async () => {
            const answers = Object.fromEntries(questions.map((question, index) => {
              const answer = String(values.get(`answer-${index}`) ?? '').trim()
              if (!answer) throw new Error('请回答每个问题后再提交。')
              return [question.id, { answers: [answer] }]
            }))
            await workspace.taskControl(task.task_id, "respond_request", { request_id: request.request_id, response: { answers } })
          }).finally(() => setBusy(false))
        }}>
          <fieldset disabled={disabled || busy || !workspace.writable} className="min-w-0">
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
      ) : <p className="text-sm text-muted-foreground">{questions.some((question) => question.is_secret) ? '此请求包含敏感输入，请在执行设备填写；答案不会保存到浏览器待核对记录。' : '此请求暂无可用的回复选项，请查看请求详情并到执行设备人工核对。'}</p>}
      <RawDetails title="查看请求详情" value={request} />
    </section>
  )
}
