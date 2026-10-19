"use client"

import { useId, useState } from "react"
import { ChevronDownIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { Artifact } from "../chat/elements/artifact"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName, stateLabel } from "./labels"
import { ActionButton, DetailList, StatusBadge, SubmitButton } from "./present"
import { reconcileDecision } from "./reconcile"
import type { ExecutionActivityRecord, HumanRequest, OperationRecord, PendingOperation } from "./types"

export function ExecutionActivity({ execution, conversationId, compact = false }: { execution: ExecutionActivityRecord; conversationId: string; compact?: boolean }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const deviceName = nodeName(execution.target_node, { nodes: workspace.nodes, directory }, "执行设备")
  const [steerGeneration, setSteerGeneration] = useState<number | null>(null)
  const steerBusy = steerGeneration === workspace.generation
  const targetOnline = workspace.nodes.some((node) => node.node_id === execution.target_node && node.online && !node.revoked)
  const pending = workspace.pending.filter((item) => item.conversation_id === conversationId && (item.execution_control?.expected_operation_id === execution.operation_id || item.target_node === execution.target_node && execution.session_id !== null && item.session_id === execution.session_id))
  const canControl = workspace.writable && Boolean(workspace.selectedBot) && targetOnline && execution.state !== "unknown" && execution.session_state !== "unknown" && execution.freshness !== "unknown" && pending.length === 0
  const followupMode = execution.goal?.followup_mode
  if (compact) {
    if (!execution.pending_requests.length) return null
    return <div className="grid gap-3">
      {execution.pending_requests.map((request) => <ExecutionApproval key={`${typeof request.request_id}:${request.request_id}`} execution={execution} request={request} canRespond={execution.capabilities.includes("respond_request")} disabled={!canControl} />)}
    </div>
  }
  return <Card>
    <CardHeader className="flex flex-wrap items-start justify-between gap-3">
      <CardTitle className="min-w-0 flex-1 break-words">{execution.title || "执行"}</CardTitle>
      <StatusBadge value={execution.state} />
    </CardHeader>
    <CardContent className="space-y-3">
      <p className="text-xs text-muted-foreground">{deviceName} · {execution.freshness === "cached" ? "上次核对的进展" : execution.freshness === "local_ledger" ? "执行设备已记录" : "等待核对进展"}{execution.checked_at_ms ? ` · ${new Date(execution.checked_at_ms).toLocaleString("zh-CN")}` : ""}</p>
      {execution.latest_progress ? <p className="whitespace-pre-wrap break-words text-sm">{execution.latest_progress}</p> : null}
      {execution.detail && execution.detail !== execution.latest_progress ? <p className="whitespace-pre-wrap break-words text-sm">{execution.detail}</p> : null}
      {!targetOnline ? <p role="status" className="text-sm text-muted-foreground">设备恢复连接后可核对最新进展。</p> : null}
      {execution.state === "unknown" ? <p role="status" className="text-sm">结果待核查。请先确认原进程和外部结果，避免重复执行。</p> : null}
      <div className="flex flex-wrap gap-2">
        <ActionButton disabled={!workspace.connectionOnline} action={() => workspace.run(() => workspace.refreshExecution(execution.operation_id))}>核对进展</ActionButton>
        {execution.capabilities.includes("interrupt") ? <ActionButton disabled={!canControl} variant="destructive" action={() => workspace.run(() => workspace.executionControl(execution.operation_id, { type: "interrupt" }))}>停止当前执行</ActionButton> : null}
      </div>
      {execution.capabilities.includes("interrupt") ? <p className="text-xs text-muted-foreground">只停这一次执行。</p> : null}
      {followupMode === "continue" || followupMode === "report" ? <section className="space-y-2 rounded-lg bg-muted p-3" aria-label="后续任务">
        <p className="text-sm">{followupMode === "continue" ? "已授权这项工作完成后继续。" : "已授权这项工作完成后自动汇报。"}</p>
        <ActionButton disabled={!workspace.writable} action={() => workspace.run(() => workspace.stopFollowup(execution.operation_id))}>取消后续任务</ActionButton>
        <p className="text-xs text-muted-foreground">只取消还没开始的后续。不改已经发出的消息。</p>
      </section> : null}
      {execution.capabilities.includes("steer") ? <Collapsible>
        <CollapsibleTrigger asChild><Button type="button" variant="ghost" disabled={!canControl} className="min-h-9 px-0">补充说明<ChevronDownIcon aria-hidden="true" /></Button></CollapsibleTrigger>
        <CollapsibleContent className="pt-3">
          <form onSubmit={(event) => {
            event.preventDefault()
            const form = event.currentTarget
            if (!form.reportValidity()) return
            const text = String(new FormData(form).get("text") ?? "").trim()
            const generation = workspace.generation
            setSteerGeneration(generation)
            void workspace.run(async () => {
              await workspace.executionControl(execution.operation_id, { type: "steer", text })
              if (workspace.isCurrent()) form.reset()
            }).finally(() => setSteerGeneration(owner => owner === generation ? null : owner))
          }}>
            <fieldset disabled={!canControl} className="min-w-0"><FieldGroup>
              <Field><FieldLabel htmlFor={`${id}-steer`}>补充说明</FieldLabel><Textarea id={`${id}-steer`} name="text" rows={3} required maxLength={50000} /></Field>
              <SubmitButton busy={steerBusy} variant="secondary">发送补充说明</SubmitButton>
            </FieldGroup></fieldset>
          </form>
        </CollapsibleContent>
      </Collapsible> : null}
      {execution.pending_requests.map((request) => <ExecutionApproval key={`${typeof request.request_id}:${request.request_id}`} execution={execution} request={request} canRespond={execution.capabilities.includes("respond_request")} disabled={!canControl} />)}
      {pending.map((item) => <PendingExecutionControl key={item.operation_id} operation={item} />)}
      {execution.blobs.map((blob) => <Artifact key={blob.hash} hash={blob.hash} size={blob.size} />)}
      {execution.goal ? <Collapsible><CollapsibleTrigger asChild><Button variant="ghost" className="min-h-9 px-0">查看交付要求<ChevronDownIcon aria-hidden="true" /></Button></CollapsibleTrigger><CollapsibleContent className="pt-3"><DetailList pairs={[["目标", execution.goal.title], ["验收条件", execution.goal.acceptance], ["状态", stateLabel(execution.goal.status)]]} /></CollapsibleContent></Collapsible> : null}
      {execution.state === "unknown" ? <ResolveForm targetNode={execution.target_node} operationId={execution.operation_id} /> : null}
    </CardContent>
  </Card>
}

export function PendingExecutionControl({ operation }: { operation: PendingOperation }) {
  const workspace = useWorkspace()
  const [result, setResult] = useState<{ generation: number; text: string; missing: boolean } | null>(null)
  const currentResult = result?.generation === workspace.generation ? result : null
  const online = workspace.nodes.some((node) => node.node_id === operation.target_node && node.online && !node.revoked)
  return <section className="space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3" aria-label="操作待核对">
    <h3 className="text-sm font-semibold">操作结果待核对</h3>
    <p className="text-sm">已保留原请求，不会自动重发。</p>
    <div className="flex flex-wrap gap-2">
      <ActionButton disabled={!workspace.connectionOnline || !online} action={() => workspace.run(async () => {
        const result = await workspace.reconcile(operation)
        if (!workspace.isCurrent()) return
        const decision = reconcileDecision(result, operation)
        setResult({ generation: workspace.generation, missing: decision === "missing", text: result.operation ? `执行设备已记录，${stateLabel(result.operation.state)}。` : "执行设备尚未找到原请求。" })
      })}>核对原操作</ActionButton>
      {operation.execution_control ? <ActionButton disabled={!workspace.writable || !online || !workspace.selectedBot} action={() => workspace.run(() => workspace.retryExecutionControl(operation))}>重试原请求</ActionButton> : null}
    </div>
    {currentResult ? <p role="status" className="text-sm">{currentResult.text}</p> : null}
    {currentResult?.missing ? <><p className="text-sm">确认原请求已结束后，可解除本地待核对记录。</p><ActionButton disabled={!workspace.writable} action={() => workspace.run(() => workspace.confirmMissing(operation.operation_id))}>确认原请求结束并解除待核对</ActionButton></> : null}
  </section>
}

export function UnknownOperationActivity({ operation }: { operation: OperationRecord }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const deviceName = nodeName(operation.request.target_node, { nodes: workspace.nodes, directory }, "执行设备")
  return <Card><CardHeader><CardTitle>结果待核查 · {deviceName}</CardTitle></CardHeader><CardContent className="space-y-3"><p className="text-sm">先确认原进程和外部结果，避免重复执行。</p><ResolveForm targetNode={operation.request.target_node} operationId={operation.request.operation_id} /></CardContent></Card>
}

function ResolveForm({ targetNode, operationId }: { targetNode: string; operationId: string }) {
  const workspace = useWorkspace()
  const id = useId()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === workspace.generation
  return <Collapsible>
    <CollapsibleTrigger asChild><Button type="button" variant="secondary">记录本人核查结果</Button></CollapsibleTrigger>
    <CollapsibleContent className="pt-3"><form onSubmit={(event) => {
      event.preventDefault()
      const form = event.currentTarget
      if (!form.reportValidity()) return
      const data = new FormData(form)
      const generation = workspace.generation
      setBusyGeneration(generation)
      void workspace.run(() => workspace.resolveUnknown(targetNode, operationId, String(data.get("outcome") ?? ""), String(data.get("evidence") ?? ""))).finally(() => setBusyGeneration(owner => owner === generation ? null : owner))
    }}><fieldset disabled={!workspace.writable || !workspace.selectedBot} className="min-w-0"><FieldGroup>
      <p className="text-sm">只有确认原进程已结束和外部结果后，才能解除占用。</p>
      <Field><FieldLabel htmlFor={`${id}-evidence`}>核查证据</FieldLabel><Textarea id={`${id}-evidence`} name="evidence" rows={3} required minLength={12} /></Field>
      <Field><FieldLabel htmlFor={`${id}-outcome`}>经本人核查的结果</FieldLabel><NativeSelect id={`${id}-outcome`} name="outcome" defaultValue="failed"><NativeSelectOption value="failed">未完成，原执行已结束</NativeSelectOption><NativeSelectOption value="succeeded">已完成，结果已经核实</NativeSelectOption></NativeSelect></Field>
      <SubmitButton busy={busy} variant="secondary">记录结果并解除占用</SubmitButton>
    </FieldGroup></fieldset></form></CollapsibleContent>
  </Collapsible>
}

function ExecutionApproval({ execution, request, canRespond, disabled }: { execution: ExecutionActivityRecord; request: HumanRequest; canRespond: boolean; disabled: boolean }) {
  const workspace = useWorkspace()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === workspace.generation
  const id = useId()
  const questions = request.questions ?? []
  return (
    <section className="space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3" aria-label="人工审批">
      <h4 className="text-sm font-semibold">{request.title || "等待本人决定"}</h4>
      <p className="whitespace-pre-line break-words text-sm">{request.text}</p>
      {canRespond && request.choices?.length ? (
        <div className="flex flex-wrap gap-2">
          {request.choices.map((choice, index) => <ActionButton key={index} disabled={busy || disabled} action={async () => {
            const generation = workspace.generation
            setBusyGeneration(generation)
            try {
              await workspace.run(async () => { await workspace.executionControl(execution.operation_id, { type: "respond_request", request_id: request.request_id, response: choice.response }) })
            } finally {
              setBusyGeneration(owner => owner === generation ? null : owner)
            }
          }}>{choice.label}</ActionButton>)}
        </div>
      ) : canRespond && questions.length > 0 && !questions.some((question) => question.is_secret) ? (
        <form onSubmit={(event) => {
          event.preventDefault()
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
            await workspace.executionControl(execution.operation_id, { type: "respond_request", request_id: request.request_id, response: { answers } })
          }).finally(() => setBusyGeneration(owner => owner === generation ? null : owner))
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
    </section>
  )
}
