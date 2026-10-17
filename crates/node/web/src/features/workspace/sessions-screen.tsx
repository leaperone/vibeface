"use client"

import { useEffect, useState, type ChangeEvent } from "react"
import { ChevronDownIcon, PlusIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { parseAction } from "./model"
import { ActionButton, DetailList, RawDetails, StatusBadge, SubmitButton } from "./present"
import { reconcileDecision } from "./reconcile"
import { newId } from "./storage"
import type { OperationRecord, PendingOperation, SessionRecord } from "./types"

const executors = [
  ["codex", "Codex"],
  ["claude_code", "Claude Code"],
  ["cursor_agent", "Cursor Agent"],
] as const

export function SessionsScreen() {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [executorId, setExecutorId] = useState("")
  const [kind, setKind] = useState("codex")
  const devices = workspace.nodes.filter((node) => node.roles?.includes("executor"))
  const selected = devices.find((node) => node.node_id === executorId)
  const kinds = selected?.capabilities?.executors ?? []
  const sessions = workspace.data?.sessions ?? []
  const operations = [...(workspace.data?.operations ?? [])].reverse()
  const unresolved = operations.filter((operation) => operation.state === "unknown")
  const history = operations.filter((operation) => operation.state !== "unknown")
  const pending = workspace.pending.filter((item) => item.conversation_id === workspace.conversationId)

  useEffect(() => {
    if (typeof document !== "undefined" && document.activeElement?.id === "executor-node") return
    setExecutorId((current) => devices.some((node) => node.node_id === current) ? current : devices[0]?.node_id ?? "")
  }, [devices])

  useEffect(() => {
    if (kinds.length > 0 && !kinds.includes(kind)) setKind(kinds[0])
  }, [kind, kinds])

  return (
    <section className="workspace-screen space-y-5" aria-label="执行会话" hidden={workspace.view !== "sessions"}>
      <Collapsible open={open} onOpenChange={setOpen}>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-lg font-semibold">执行会话 <span className="text-sm font-normal text-muted-foreground">{sessions.length}</span></h2>
          <CollapsibleTrigger asChild disabled={!workspace.conversationId}>
            <Button type="button"><PlusIcon />{open ? "收起表单" : "打开执行会话"}</Button>
          </CollapsibleTrigger>
        </div>
        <CollapsibleContent forceMount className="pt-4 data-[state=closed]:hidden">
          <Card>
            <CardContent>
              <form
                onSubmit={(event) => {
                  event.preventDefault()
                  const form = event.currentTarget
                  if (!form.reportValidity()) return
                  const target = devices.find((node) => node.node_id === executorId)
                  setBusy(true)
                  void workspace.run(async () => {
                    if (!target?.online) throw new Error("执行设备离线，请先恢复连接。")
                    await workspace.operate(
                      { target_node: target.node_id, session_id: newId(), revision: 0 },
                      { type: "open", executor: kind, cwd: String(new FormData(form).get("cwd") ?? "").trim() },
                      workspace.conversationId,
                    )
                    setOpen(false)
                  }).finally(() => setBusy(false))
                }}
              >
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="executor-node">执行设备</FieldLabel>
                    <NativeSelect id="executor-node" name="target_node" required value={executorId} onChange={(event: ChangeEvent<HTMLSelectElement>) => setExecutorId(event.target.value)}>
                      {devices.length ? devices.map((node) => (
                        <NativeSelectOption key={node.node_id} value={node.node_id}>
                          {nodeName(node.node_id)}{node.local ? " · 本机" : ""}{node.online ? "" : " · 离线"}
                        </NativeSelectOption>
                      )) : <NativeSelectOption value="">没有执行设备</NativeSelectOption>}
                    </NativeSelect>
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="executor-kind">执行器</FieldLabel>
                    <NativeSelect id="executor-kind" name="executor" value={kind} onChange={(event: ChangeEvent<HTMLSelectElement>) => setKind(event.target.value)}>
                      {executors.map(([value, label]) => (
                        <NativeSelectOption key={value} value={value} disabled={kinds.length > 0 && !kinds.includes(value)}>{label}</NativeSelectOption>
                      ))}
                    </NativeSelect>
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="session-cwd">工作目录</FieldLabel>
                    <Input id="session-cwd" name="cwd" required placeholder="/Users/you/project" maxLength={4096} />
                    <p className="text-sm text-muted-foreground">使用执行设备允许的目录。</p>
                  </Field>
                  <SubmitButton busy={busy}>打开会话</SubmitButton>
                </FieldGroup>
              </form>
            </CardContent>
          </Card>
        </CollapsibleContent>
      </Collapsible>
      <div className="grid gap-4">
        {pending.map((operation) => <PendingCard key={operation.operation_id} operation={operation} />)}
        {unresolved.map((operation) => (
          <OperationCard key={operation.request.operation_id} operation={operation} />
        ))}
        {sessions.length ? sessions.map((session) => (
          <SessionCard key={`${session.target_node}:${session.session_id}`} session={session} conversationId={workspace.conversationId} />
        )) : <p className="py-10 text-center text-sm text-muted-foreground">{workspace.conversationId ? "还没有执行会话，可让调度 Agent 派发任务或打开执行器。" : "先选择或新建会话，再打开执行会话。"}</p>}
      </div>
      <Collapsible>
        <CollapsibleTrigger asChild>
          <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">操作记录<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
        </CollapsibleTrigger>
        <CollapsibleContent className="grid gap-4 pt-3">
          {history.length ? history.map((operation) => (
            <OperationCard key={operation.request.operation_id} operation={operation} />
          )) : <p className="text-sm text-muted-foreground">暂无操作记录。</p>}
        </CollapsibleContent>
      </Collapsible>
    </section>
  )
}

function OperationCard({ operation }: { operation: OperationRecord }) {
  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 flex-1 break-words">执行操作 · {nodeName(operation.request.target_node)}</CardTitle>
        <StatusBadge value={operation.state} />
      </CardHeader>
      <CardContent>
        {operation.state === "unknown" ? <p role="status" className="text-sm">结果未知，请先核查原进程与外部结果，避免重复执行。</p> : null}
        <RawDetails title="操作与结果" value={operation.raw} />
        {operation.state === "unknown" && operation.request.operation_id && operation.request.target_node ? (
          <ResolveForm targetNode={operation.request.target_node} operationId={operation.request.operation_id} />
        ) : null}
      </CardContent>
    </Card>
  )
}

function PendingCard({ operation }: { operation: PendingOperation }) {
  const workspace = useWorkspace()
  const [record, setRecord] = useState<unknown>(null)
  const [missing, setMissing] = useState(false)
  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 break-words">操作待核对</CardTitle>
        <StatusBadge value="unknown" label="待确认" />
      </CardHeader>
      <CardContent>
        <p>不会自动重发。请向执行设备核对原操作。</p>
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">操作编号<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-2"><code className="break-all text-xs">{operation.operation_id}</code></CollapsibleContent>
        </Collapsible>
        <ActionButton action={async () => {
          await workspace.run(async () => {
            const next = await workspace.reconcile(operation)
            if (reconcileDecision(next, operation) === "settled") return
            setRecord(next)
            setMissing(reconcileDecision(next, operation) === "missing")
          })
        }}>核对原操作</ActionButton>
        {record ? <RawDetails title="核对结果" value={record} defaultOpen /> : null}
        {missing ? (
          <>
            <p>执行设备当前未找到此编号。确认原请求已结束后，可解除本地待核对记录；不会重发操作。</p>
            <ActionButton action={() => workspace.run(async () => { await workspace.confirmMissing(operation.operation_id) })}>确认原请求结束并解除待核对</ActionButton>
          </>
        ) : null}
      </CardContent>
    </Card>
  )
}

function SessionCard({ session, conversationId }: { session: SessionRecord; conversationId: string }) {
  const workspace = useWorkspace()
  const actual = workspace.actual[`${session.target_node}:${session.session_id}`]
  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 flex-1 break-words">{session.executor} · {nodeName(session.target_node)}</CardTitle>
        <StatusBadge value={session.state} />
      </CardHeader>
      <CardContent>
        <p className="break-all text-sm">{session.cwd}</p>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-xs text-muted-foreground">{actual !== undefined ? "已读取实际状态" : "尚未读取实际状态"}</p>
          <ActionButton disabled={!session.native_session_id} action={() => workspace.run(() => workspace.readSession(session))}>读取实际状态</ActionButton>
        </div>
        {session.state === "unknown" ? <p role="status" className="rounded-lg bg-amber-500/10 p-3 text-sm">结果未知。执行器可能仍在运行，先读取实际状态，不要重复派发。</p> : null}
        {actual !== undefined ? <Approvals session={session} actual={actual} conversationId={conversationId} /> : null}
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">会话详情<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="grid gap-3 pt-3">
            <DetailList pairs={[["会话 ID", session.session_id], ["版本", session.revision === undefined ? undefined : String(session.revision)]]} />
            {actual !== undefined ? <RawDetails title="实际执行状态" value={actual} /> : null}
          </CollapsibleContent>
        </Collapsible>
        <CommandForm session={session} conversationId={conversationId} />
      </CardContent>
    </Card>
  )
}

function CommandForm({ session, conversationId }: { session: SessionRecord; conversationId: string }) {
  const workspace = useWorkspace()
  const [action, setAction] = useState("send")
  const [busy, setBusy] = useState(false)
  return (
    <Collapsible>
      <CollapsibleTrigger asChild>
        <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">给执行器发指令<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
      </CollapsibleTrigger>
      <CollapsibleContent forceMount className="pt-3 data-[state=closed]:hidden">
        <form
          onSubmit={(event) => {
            event.preventDefault()
            const form = event.currentTarget
            if (!form.reportValidity()) return
            const data = new FormData(form)
            setBusy(true)
            void workspace.run(async () => {
              const next = parseAction({ type: action, text: String(data.get("text") ?? ""), turn_id: String(data.get("turn_id") ?? "") })
              await workspace.operate(session, next, conversationId)
              const text = form.elements.namedItem("text")
              if (text instanceof HTMLTextAreaElement) text.value = ""
            }).finally(() => setBusy(false))
          }}
        >
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor={`session-action-${session.session_id}`}>操作</FieldLabel>
              <NativeSelect id={`session-action-${session.session_id}`} name="action" value={action} onChange={(event: ChangeEvent<HTMLSelectElement>) => setAction(event.target.value)}>
                <NativeSelectOption value="send">发送新一轮任务</NativeSelectOption>
                <NativeSelectOption value="steer">补充当前轮指令</NativeSelectOption>
                <NativeSelectOption value="interrupt">中断当前轮</NativeSelectOption>
              </NativeSelect>
            </Field>
            <Field hidden={action === "send"}>
              <FieldLabel htmlFor={`session-turn-${session.session_id}`}>当前轮 ID</FieldLabel>
              <Input id={`session-turn-${session.session_id}`} name="turn_id" required={action !== "send"} />
            </Field>
            {action === "interrupt" ? null : (
              <Field>
                <FieldLabel htmlFor={`session-text-${session.session_id}`}>内容</FieldLabel>
                <Textarea id={`session-text-${session.session_id}`} name="text" rows={3} required={action !== "interrupt"} />
              </Field>
            )}
            <SubmitButton busy={busy} variant="secondary">提交操作</SubmitButton>
          </FieldGroup>
        </form>
      </CollapsibleContent>
    </Collapsible>
  )
}

type ApprovalRequest = {
  id: string | number
  threadId?: string
  raw: unknown
}

function readApprovals(actual: unknown): ApprovalRequest[] {
  if (!actual || typeof actual !== "object" || !("pending_requests" in actual) || !Array.isArray(actual.pending_requests)) return []
  const requests: ApprovalRequest[] = []
  for (const request of actual.pending_requests) {
    if (!request || typeof request !== "object" || !("id" in request)) continue
    if (typeof request.id !== "string" && (typeof request.id !== "number" || !Number.isSafeInteger(request.id))) continue
    if (typeof request.id === "string" && !request.id) continue
    let threadId: string | undefined
    if ("params" in request && request.params && typeof request.params === "object" && "threadId" in request.params) {
      if (typeof request.params.threadId !== "string") continue
      threadId = request.params.threadId
    }
    requests.push({ id: request.id, threadId, raw: request })
  }
  return requests
}

function Approvals({ session, actual, conversationId }: { session: SessionRecord; actual: unknown; conversationId: string }) {
  return (
    <>
      {readApprovals(actual).map((request) => {
        if (request.threadId && request.threadId !== session.native_session_id) return null
        return <ApprovalForm key={`${typeof request.id}:${request.id}`} session={session} request={request} conversationId={conversationId} />
      })}
    </>
  )
}

function isObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value)
}

function ApprovalForm({
  session,
  request,
  conversationId,
}: {
  session: SessionRecord
  request: ApprovalRequest
  conversationId: string
}) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  return (
    <section className="grid gap-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-4">
      <h3 className="text-sm font-semibold">等待人工审批</h3>
      <RawDetails title="原始审批请求" value={request.raw} />
      <form
        onSubmit={(event) => {
          event.preventDefault()
          const form = event.currentTarget
          if (!form.reportValidity()) return
          setBusy(true)
          void workspace.run(async () => {
            const text = String(new FormData(form).get("response") ?? "")
            let value: unknown
            try {
              value = JSON.parse(text)
            } catch {
              throw new Error("审批回复必须是有效 JSON。请按原始请求的格式填写。")
            }
            if (!isObject(value)) throw new Error("审批回复必须是 JSON 对象。")
            await workspace.operate(session, { type: "respond_request", request_id: request.id, response: value }, conversationId)
            workspace.clearActual(session)
          }).finally(() => setBusy(false))
        }}
      >
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor={`approval-${session.session_id}-${request.id}`}>回复内容（JSON）</FieldLabel>
            <Textarea id={`approval-${session.session_id}-${request.id}`} name="response" rows={3} required placeholder='{"decision":"decline"}' />
          </Field>
          <SubmitButton busy={busy}>发送人工决定</SubmitButton>
        </FieldGroup>
      </form>
    </section>
  )
}

function ResolveForm({ targetNode, operationId }: { targetNode: string; operationId: string }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault()
        const form = event.currentTarget
        if (!form.reportValidity()) return
        const data = new FormData(form)
        setBusy(true)
        void workspace.run(async () => {
          await workspace.resolveUnknown(targetNode, operationId, String(data.get("outcome") ?? ""), String(data.get("evidence") ?? ""))
        }).finally(() => setBusy(false))
      }}
    >
      <FieldGroup>
        <p>只有你确认原进程和外部结果后，才能解除此操作占用的资源。</p>
        <Field>
          <FieldLabel htmlFor={`evidence-${operationId}`}>核查证据</FieldLabel>
          <Textarea id={`evidence-${operationId}`} name="evidence" rows={3} required minLength={12} />
        </Field>
        <Field>
          <FieldLabel htmlFor={`outcome-${operationId}`}>经本人核查的结果</FieldLabel>
          <NativeSelect id={`outcome-${operationId}`} name="outcome" defaultValue="failed">
            <NativeSelectOption value="failed">未完成，原执行已结束</NativeSelectOption>
            <NativeSelectOption value="succeeded">已完成，结果已经核实</NativeSelectOption>
          </NativeSelect>
        </Field>
        <SubmitButton busy={busy} variant="secondary">记录核查结果并解除占用</SubmitButton>
      </FieldGroup>
    </form>
  )
}
