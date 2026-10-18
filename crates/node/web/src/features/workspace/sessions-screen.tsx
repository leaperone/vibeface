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
import { parseAction, record } from "./model"
import { ActionButton, DetailList, RawDetails, StatusBadge, SubmitButton } from "./present"
import { reconcileDecision } from "./reconcile"
import { newId } from "./storage"
import { remoteExecutionProposal } from "@/lib/remote-execution"
import { nodeFingerprint } from "../../../generated/vibeface_secure.js"
import type { TrustedRemoteExecution } from "@/lib/remote-execution"
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
  const [taskId, setTaskId] = useState("")
  const [kind, setKind] = useState("codex")
  const devices = workspace.nodes.filter((node) => node.roles?.includes("executor"))
  const selected = devices.find((node) => node.node_id === executorId)
  const kinds = selected?.capabilities?.executors ?? []
  const sessions = workspace.data?.sessions ?? []
  const tasks = (workspace.data?.tasks ?? []).filter(task => task.binding === null && !["accepted", "cancelled"].includes(task.status))
  const operations = [...(workspace.data?.operations ?? [])].reverse()
  const unresolved = operations.filter((operation) => operation.state === "unknown")
  const history = operations.filter((operation) => operation.state !== "unknown")
  const pending = workspace.pending.filter((item) => item.conversation_id === workspace.conversationId)
  const pendingBindings = workspace.pendingBindings.filter(item => item.request.conversation_id === workspace.conversationId)
  const pendingResolutions = workspace.pendingRemoteResolutions.filter(item => item.request.conversation_id === workspace.conversationId)

  useEffect(() => {
    if (typeof document !== "undefined" && document.activeElement?.id === "executor-node") return
    setExecutorId((current) => devices.some((node) => node.node_id === current) ? current : devices[0]?.node_id ?? "")
  }, [devices])

  useEffect(() => {
    if (kinds.length > 0 && !kinds.includes(kind)) setKind(kinds[0])
  }, [kind, kinds])

  useEffect(() => {
    setTaskId(current => tasks.some(task => task.task_id === current) ? current : tasks[0]?.task_id ?? "")
  }, [tasks])

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
                    if (!tasks.some(task => task.task_id === taskId)) throw new Error("先选择尚未绑定的工单。")
                    await workspace.operate(
                      { target_node: target.node_id, session_id: newId(), task_id: taskId, revision: 0 },
                      { type: "open", executor: kind, cwd: String(new FormData(form).get("cwd") ?? "").trim() },
                      workspace.conversationId,
                    )
                    setOpen(false)
                  }).finally(() => setBusy(false))
                }}
              >
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="executor-task">工单</FieldLabel>
                    <NativeSelect id="executor-task" name="task_id" required value={taskId} onChange={(event: ChangeEvent<HTMLSelectElement>) => setTaskId(event.target.value)}>
                      {tasks.length ? tasks.map(task => (
                        <NativeSelectOption key={task.task_id} value={task.task_id}>{task.title}</NativeSelectOption>
                      )) : <NativeSelectOption value="">先创建尚未绑定的工单</NativeSelectOption>}
                    </NativeSelect>
                  </Field>
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
                  <Button type="submit" disabled={busy || !tasks.length || !workspace.writable} aria-busy={busy}>绑定工单并打开会话</Button>
                </FieldGroup>
              </form>
            </CardContent>
          </Card>
        </CollapsibleContent>
      </Collapsible>
      <RemoteExecutionControls />
      {workspace.pendingRemoteRebinds.filter(item => record(item.plan.binding.binding).conversation_id === workspace.conversationId).map(item => <Card key={item.plan.id}>
        <CardHeader><CardTitle>执行重绑待确认</CardTitle></CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm">{item.phase === 'seal' ? item.attempt ? '原封存可能已提交。先按原签名哈希核对 C 的封存结果。' : '重绑计划已保存，原会话尚未封存。' : item.phase === 'rebind' ? 'C 的封存回执已保存。B 的重绑使用另一份明确授权和原签名请求。' : 'B 的新绑定已保存。新执行端只接受这份绑定下的原打开请求。'}</p>
          <p className="text-sm text-muted-foreground">依次核对原 C 封存、B 重绑和新 C 打开。超时不会生成另一份写请求，也不会自动终止原进程。</p>
          <DetailList pairs={[["当前阶段", item.phase === 'seal' ? '原 C 封存' : item.phase === 'rebind' ? 'B 重绑' : '新 C 打开'], ["原执行端", item.plan.oldAuthorization.share.selection.target_public_key], ["新执行端", item.plan.nextAuthorization.share.selection.target_public_key], ["新工作目录", item.plan.cwd]]} />
          <ActionButton disabled={!workspace.localWritable} action={() => workspace.run(() => workspace.resumeRemoteRebind(item.plan.id))}>{item.phase === 'seal' ? item.attempt ? '核对原封存并继续' : '确认封存原会话' : item.phase === 'rebind' ? item.attempt ? '核对原重绑并继续' : '确认 B 的新绑定' : item.attempt ? '核对原打开并继续' : '确认在新 C 打开'}</ActionButton>
          {item.phase === 'seal' && !item.attempt ? <ActionButton disabled={!workspace.localWritable} action={() => workspace.run(() => workspace.discardRemoteRebind(item.plan.id))}>取消尚未提交的计划</ActionButton> : null}
        </CardContent>
      </Card>)}
      <div className="grid gap-4">
        {pendingResolutions.map(item => <Card key={`resolution-${item.request.operation_id}`}><CardHeader><CardTitle>结果处理请求待核对</CardTitle></CardHeader><CardContent className="space-y-3"><p className="text-sm">{workspace.writable ? '保留原执行回执、核查证据和完整签名。先核对既有裁定；仍未完成时，只重试这份结果处理请求。' : '此入口只核对已完成结果。不会发送新的结果处理请求。'}</p><ActionButton disabled={!workspace.localWritable} action={() => workspace.run(() => workspace.resumeRemoteResolution(item.request.operation_id))}>{workspace.writable ? '核对并重试原结果处理请求' : '核对原结果处理请求'}</ActionButton></CardContent></Card>)}
        {pending.map((operation) => <PendingCard key={operation.operation_id} operation={operation} />)}
        {pendingBindings.map(item => (
          <Card key={item.request.operation_id}>
            <CardHeader><CardTitle>有一笔工单绑定尚未确认</CardTitle></CardHeader>
            <CardContent>
              <p className="break-words text-sm">{item.task.title} · {nodeName(item.request.target_node)} · {item.request.action.cwd}</p>
              <p className="text-sm text-muted-foreground">保留原会话编号和绑定内容。先向节点核对；仍未找到时，按原内容重试绑定，验证后打开会话。</p>
              <ActionButton disabled={!workspace.writable} action={() => workspace.run(() => workspace.resumeBinding(item.request.operation_id))}>核对原绑定并打开会话</ActionButton>
            </CardContent>
          </Card>
        ))}
        {unresolved.map((operation) => (
          <OperationCard key={operation.request.operation_id} operation={operation} />
        ))}
        {sessions.length ? sessions.map((session) => (
          <SessionCard key={`${session.target_node}:${session.session_id}`} session={session} conversationId={workspace.conversationId} />
        )) : <p className="py-10 text-center text-sm text-muted-foreground">{workspace.conversationId ? "还没有执行会话，请先创建工单，再绑定到执行设备。" : "先选择或新建会话，再打开执行会话。"}</p>}
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

function RemoteExecutionControls() {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const [request, setRequest] = useState('')
  const availableTasks = (workspace.data?.tasks ?? []).filter(task => task.binding !== null || !['accepted', 'cancelled'].includes(task.status))
  useEffect(() => { setRequest('') }, [workspace.conversationId, workspace.projectScopeId])
  return (
    <Collapsible>
      <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="gap-2">远端受限执行<ChevronDownIcon className="size-4" /></Button></CollapsibleTrigger>
      <CollapsibleContent forceMount className="grid gap-4 pt-3 data-[state=closed]:hidden">
        <Card>
          <CardHeader><CardTitle>提供 B 项目的执行授权范围</CardTitle></CardHeader>
          <CardContent>
            <p className="mb-3 text-sm text-muted-foreground">选择任务与执行器，导出 B 的完整受签申请。C 管理者导入并核验后，再选择执行资源、允许目录和授权期限。这里不会派发任务。</p>
            <p className="mb-3 text-sm text-muted-foreground">已绑定的任务会沿用原受签会话和执行器，可据此单独续发结果处理授权。</p>
            <form onSubmit={event => {
              event.preventDefault(); const form = event.currentTarget
              if (!form.reportValidity()) return
              const data = new FormData(form)
              setBusy(true)
              void workspace.run(async () => setRequest(await workspace.remoteExecutionRequest(String(data.get('task_id') ?? ''), String(data.get('executor') ?? ''), Date.parse(String(data.get('expires_at') ?? '')), String(data.get('purpose')) === 'rebind' ? 'rebind' : String(data.get('purpose')) === 'resolution' ? 'resolution' : 'execution', data.has('for_rebind')))).finally(() => setBusy(false))
            }}>
              <FieldGroup>
                <Field><FieldLabel htmlFor="remote-request-task">任务</FieldLabel><NativeSelect id="remote-request-task" name="task_id" required>{availableTasks.length ? availableTasks.map(task => <NativeSelectOption key={task.task_id} value={task.task_id}>{task.title}{task.binding ? ' · 续申请' : ''}</NativeSelectOption>) : <NativeSelectOption value="">先创建任务</NativeSelectOption>}</NativeSelect></Field>
                <Field><FieldLabel htmlFor="remote-request-executor">执行器</FieldLabel><NativeSelect id="remote-request-executor" name="executor">{executors.map(([value, label]) => <NativeSelectOption key={value} value={value}>{label}</NativeSelectOption>)}</NativeSelect></Field>
                <Field><FieldLabel htmlFor="remote-request-purpose">申请用途</FieldLabel><NativeSelect id="remote-request-purpose" name="purpose"><NativeSelectOption value="execution">执行或为重绑申请新执行目标</NativeSelectOption><NativeSelectOption value="resolution">处理原执行的未知结果</NativeSelectOption><NativeSelectOption value="rebind">封存原 C 会话以重绑</NativeSelectOption></NativeSelect></Field>
                <Field><FieldLabel htmlFor="remote-request-expiry">申请有效期，本地时间</FieldLabel><Input id="remote-request-expiry" name="expires_at" type="datetime-local" required /></Field>
                <label className="flex min-h-11 items-center gap-2 text-sm"><input type="checkbox" name="for_rebind" />为已有任务的重绑申请新执行目标</label>
                <Button type="submit" disabled={busy || workspace.incomplete || !availableTasks.length}>生成受签执行申请</Button>
              </FieldGroup>
            </form>
            {request ? <div className="mt-3 space-y-3"><p className="text-sm">受签申请已生成，交给 C 管理者导入核验。</p><Button type="button" variant="outline" onClick={() => {
              const url = URL.createObjectURL(new Blob([request], { type: 'application/json' })), link = document.createElement('a')
              link.href = url; link.download = 'vibeface-execution-request.json'; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000)
            }}>下载受签申请</Button><Collapsible><CollapsibleTrigger asChild><Button type="button" variant="ghost">查看完整受签内容</Button></CollapsibleTrigger><CollapsibleContent><Textarea value={request} readOnly rows={7} className="font-mono text-xs" aria-label="交给 C 管理者的完整受签执行申请" /></CollapsibleContent></Collapsible></div> : null}
            <p className="mt-4 text-sm text-muted-foreground">首次联系 C 管理者时，通过另一个可信渠道交付 B 的独立信任卡。申请文件本身不包含根 pins。</p>
            <Button type="button" variant="outline" onClick={() => void workspace.run(async () => {
              const card = workspace.exportRemoteExecutionSourceTrust(), url = URL.createObjectURL(new Blob([card], { type: 'application/json' })), link = document.createElement('a')
              link.href = url; link.download = 'vibeface-source-trust.json'; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000)
            })}>下载独立来源信任卡</Button>
          </CardContent>
        </Card>
        <Card>
          <CardHeader><CardTitle>导入执行设备的有限授权</CardTitle></CardHeader>
          <CardContent>
            <form onSubmit={event => {
              event.preventDefault()
              const form = event.currentTarget
              if (!form.reportValidity()) return
              const values = new FormData(form)
              setBusy(true)
              void workspace.run(async () => {
                await workspace.importRemoteExecution(String(values.get("share") ?? ""), String(values.get("trust") ?? ""), String(values.get("cwd") ?? ""))
                form.reset()
              }).finally(() => setBusy(false))
            }}>
              <FieldGroup>
                <p className="text-sm text-muted-foreground">授权必须由执行设备所属账户授予当前项目 Node 的 Service，且只覆盖这一任务和会话。请通过另外的可信渠道确认账户和空间根公钥。</p>
                <p className="text-sm text-muted-foreground">执行分享和结果处理分享分别导入。Rebind 授权在原执行会话的重绑表单中使用。</p>
                <Field><FieldLabel htmlFor="remote-execution-share">执行授权分享</FieldLabel><Textarea id="remote-execution-share" name="share" rows={4} required spellCheck={false} /></Field>
                <Field><FieldLabel htmlFor="remote-execution-trust">独立确认的账户根信任 JSON</FieldLabel><Textarea id="remote-execution-trust" name="trust" rows={4} required spellCheck={false} /><p className="text-sm text-muted-foreground">填写可信设备提供的 kind、accountId、spaceId 和 pins。这里的根公钥不能直接取自上面的分享或未知 Node 响应。</p></Field>
                <Field><FieldLabel htmlFor="remote-execution-cwd">远端规范工作目录</FieldLabel><Input id="remote-execution-cwd" name="cwd" required maxLength={4096} placeholder="/workspace/project" /></Field>
                <Button type="submit" disabled={busy || workspace.incomplete || !workspace.writable} aria-busy={busy}>核验授权并保存根信任</Button>
              </FieldGroup>
            </form>
          </CardContent>
        </Card>
        {workspace.remoteExecutions.filter(remote => remoteExecutionProposal(remote).conversationId === workspace.conversationId).map(remote => <RemoteExecutionCard key={remote.share.selection.execution_grant} remote={remote} />)}
      </CollapsibleContent>
    </Collapsible>
  )
}

function RemoteExecutionCard({ remote }: { remote: TrustedRemoteExecution }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const proposed = remoteExecutionProposal(remote)
  const alreadyBound = workspace.data?.tasks.some(task => task.binding !== null && record(record(task.binding).binding).task_ref === proposed.taskRef)
  return (
    <Card>
      <CardHeader><CardTitle>{proposed.purpose === 'rebind' ? '原执行会话的 Rebind 授权' : proposed.purpose === 'resolution' ? '此远端会话的结果处理授权已导入' : alreadyBound ? '新执行目标的授权已导入' : '按已确认的远端授权打开执行会话'}</CardTitle></CardHeader>
      <CardContent>
        <DetailList pairs={[["执行设备公钥", remote.share.selection.target_public_key], ["执行空间", remote.share.selection.execution_resource.space], ["会话", proposed.sessionId], ["执行器", proposed.executorKind]]} />
        {proposed.purpose === 'rebind' ? <p className="mt-3 text-sm text-muted-foreground">在原执行会话中核对并准备重绑。此授权不能打开新执行或处理未知结果。</p> : proposed.purpose === 'resolution' ? <p className="mt-3 text-sm text-muted-foreground">此授权只用于原操作的结果核验和明确裁定。到下方未知操作中填写核查证据后使用。</p> : alreadyBound ? <p className="mt-3 text-sm text-muted-foreground">此任务已有执行绑定。请在原执行会话的重绑表单中选择这份新目标授权。</p> : <form className="mt-4" onSubmit={event => {
          event.preventDefault()
          const form = event.currentTarget
          if (!form.reportValidity()) return
          const cwd = String(new FormData(form).get("cwd") ?? "").trim()
          setBusy(true)
          void workspace.run(async () => {
            const target = workspace.remoteExecutionTarget(remote)
            await workspace.operate(target, { type: "open", executor: target.executor, cwd }, workspace.conversationId, remote)
          }).finally(() => setBusy(false))
        }}>
          <FieldGroup>
            <Field><FieldLabel htmlFor={`remote-cwd-${remote.share.selection.execution_grant}`}>远端规范工作目录</FieldLabel><Input id={`remote-cwd-${remote.share.selection.execution_grant}`} name="cwd" required maxLength={4096} /></Field>
            <Button type="submit" disabled={busy || workspace.incomplete || !workspace.writable} aria-busy={busy}>绑定任务并在远端打开</Button>
          </FieldGroup>
        </form>}
      </CardContent>
    </Card>
  )
}

function RemoteRebindControls({ session }: { session: SessionRecord }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const binding = record(session.binding.binding)
  const candidates = workspace.remoteExecutions.filter(remote => {
    const proposed = remoteExecutionProposal(remote)
    return proposed.purpose === 'execution' && proposed.taskRef === binding.task_ref && proposed.conversationId === binding.conversation_id
      && (proposed.sessionId !== session.session_id || nodeFingerprint(remote.share.selection.target_public_key) !== session.target_node)
  })
  const hasPending = workspace.pendingRemoteRebinds.some(item => record(item.plan.binding.binding).task_ref === binding.task_ref)
  const prefix = `rebind-${session.target_node}-${session.session_id}`
  return <Collapsible>
    <CollapsibleTrigger asChild><Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">重绑到新执行目标<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button></CollapsibleTrigger>
    <CollapsibleContent className="space-y-3 pt-3">
      <p className="text-sm text-muted-foreground">先核验原 C 的当前回执。向原 C 管理者取得单独的 Rebind 授权，并为已有任务申请、导入新目标的执行分享。保存计划不会封存或启动执行。</p>
      <form onSubmit={event => {
        event.preventDefault()
        const form = event.currentTarget
        if (!form.reportValidity()) return
        const data = new FormData(form)
        setBusy(true)
        void workspace.run(async () => {
          await workspace.prepareRemoteRebind(session, String(data.get('old_share') ?? ''), String(data.get('next_grant') ?? ''), String(data.get('cwd') ?? '').trim())
          form.reset()
        }).finally(() => setBusy(false))
      }}><FieldGroup>
        <Field><FieldLabel htmlFor={`${prefix}-old-share`}>原 C 为此会话批准的 Rebind 授权</FieldLabel><Textarea id={`${prefix}-old-share`} name="old_share" rows={4} required className="font-mono text-xs" /></Field>
        <Field><FieldLabel htmlFor={`${prefix}-target`}>已核验的新目标授权</FieldLabel><NativeSelect id={`${prefix}-target`} name="next_grant" required>
          {candidates.length ? candidates.map(candidate => {
            const proposed = remoteExecutionProposal(candidate)
            return <NativeSelectOption key={candidate.share.selection.execution_grant} value={candidate.share.selection.execution_grant}>{proposed.executorKind} · {candidate.share.selection.target_public_key.slice(0, 12)} · {proposed.sessionId}</NativeSelectOption>
          }) : <NativeSelectOption value="">先导入此任务新会话的执行分享</NativeSelectOption>}
        </NativeSelect></Field>
        <Field><FieldLabel htmlFor={`${prefix}-cwd`}>新执行端规范工作目录</FieldLabel><Input id={`${prefix}-cwd`} name="cwd" required maxLength={4096} placeholder="/workspace/project" /></Field>
        <SubmitButton busy={busy} disabled={!workspace.writable || hasPending || !candidates.length || session.state === 'unknown'}>保存重绑计划</SubmitButton>
      </FieldGroup></form>
    </CollapsibleContent>
  </Collapsible>
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
            if (reconcileDecision(next, operation) === "settled" && workspace.localWritable) return
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
  const remote = session.target_node !== workspace.projectNodeId
  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 flex-1 break-words">{session.executor} · {nodeName(session.target_node)}</CardTitle>
        <StatusBadge value={session.state} />
      </CardHeader>
      <CardContent>
        <p className="break-all text-sm">{session.cwd}</p>
        {remote ? <p className="text-sm text-muted-foreground">上方为项目保存的记录。当前远端状态以独立核验的 C 签名回执为准。</p> : null}
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-xs text-muted-foreground">{remote ? actual !== undefined ? "已核验 C 签名执行结果" : "尚未核验 C 当前回执" : actual !== undefined ? "已读取实际状态" : "尚未读取实际状态"}</p>
          <ActionButton disabled={!remote && !session.native_session_id} action={() => workspace.run(() => workspace.readSession(session))}>{remote ? "核验远端当前回执" : "读取实际状态"}</ActionButton>
        </div>
        {session.state === "unknown" ? <p role="status" className="rounded-lg bg-amber-500/10 p-3 text-sm">结果未知。执行器可能仍在运行，先读取实际状态，不要重复派发。</p> : null}
        {remote ? <RemoteRebindControls session={session} /> : null}
        {actual !== undefined ? <Approvals session={session} actual={actual} conversationId={conversationId} /> : null}
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">会话详情<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="grid gap-3 pt-3">
            <DetailList pairs={[["工单", session.task_id], ["会话 ID", session.session_id], ["版本", session.revision === undefined ? undefined : String(session.revision)]]} />
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
            <SubmitButton busy={busy} disabled={!workspace.writable} variant="secondary">提交操作</SubmitButton>
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
          <SubmitButton busy={busy} disabled={!workspace.writable}>发送人工决定</SubmitButton>
        </FieldGroup>
      </form>
    </section>
  )
}

function ResolveForm({ targetNode, operationId }: { targetNode: string; operationId: string }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const [outcome, setOutcome] = useState('failed')
  const remote = targetNode !== workspace.projectNodeId
  const signedState = workspace.remoteObservations[operationId]?.state
  const terminal = signedState && ['succeeded', 'failed', 'rejected'].includes(signedState) ? signedState : undefined
  const pending = workspace.pendingRemoteResolutions.some(item => item.request.operation_id === operationId)
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
        <p>{remote ? '使用独立结果处理授权核验 C 回执。确认原进程和外部结果后，才能处理 B 的未知状态。' : '只有你确认原进程和外部结果后，才能解除此操作占用的资源。'}</p>
        {terminal && <p role="status" className="text-sm">C 的完整签名结果已经核验。将按其真实终态处理项目记录。</p>}
        <Field>
          <FieldLabel htmlFor={`evidence-${operationId}`}>核查证据</FieldLabel>
          <Textarea id={`evidence-${operationId}`} name="evidence" rows={3} required minLength={12} />
        </Field>
        <Field>
          <FieldLabel htmlFor={`outcome-${operationId}`}>经本人核查的结果</FieldLabel>
          <NativeSelect id={`outcome-${operationId}`} name="outcome" value={terminal ?? outcome} onChange={event => setOutcome(event.target.value)}>
            <NativeSelectOption value="failed" disabled={Boolean(terminal && terminal !== 'failed')}>未完成，原执行已结束</NativeSelectOption>
            <NativeSelectOption value="succeeded" disabled={Boolean(terminal && terminal !== 'succeeded')}>已完成，结果已经核实</NativeSelectOption>
            {remote && <NativeSelectOption value="rejected" disabled={Boolean(terminal && terminal !== 'rejected')}>已拒绝，原执行未获准</NativeSelectOption>}
          </NativeSelect>
        </Field>
        <SubmitButton busy={busy} disabled={!workspace.writable || pending} variant="secondary">记录核查结果并解除占用</SubmitButton>
      </FieldGroup>
    </form>
  )
}
