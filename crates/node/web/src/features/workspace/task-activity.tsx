"use client"

import { useId, useState, type ReactNode } from "react"
import { CircleQuestionMark, Ellipsis, Hand, TriangleAlert } from "lucide-react"
import { Popover } from "radix-ui"
import { ActionCard, Details } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { approvalKind, approvalRisk, nodeName } from "./labels"
import { ActionButton, SubmitButton } from "./present"
import type { HumanRequest, NativeSessionRecord, PendingCheck } from "./types"

const decisionOf = (response: unknown) => response && typeof response === "object" ? (response as { decision?: unknown }).decision : undefined

/** Shared by the chat stream, the task panel and /inbox; `stoppable` is off where the view has its own stop button. */
export function ApprovalCard({ session, request, meta, stoppable = true }: { session: NativeSessionRecord; request: HumanRequest; meta?: ReactNode; stoppable?: boolean }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === workspace.generation
  const node = workspace.nodes.find((item) => item.peer_id === session.target_node)
  const active = session.state === "running" || session.state === "waiting"
  const disabled = !workspace.writable || !node?.online || Boolean(node.revoked) || !active || session.freshness !== "observed" || !session.can_control
  const respond = (response: unknown) => async () => {
    if (disabled) return
    await workspace.respondNative(session.work_id, session.target_node, request.request_id, response)
  }
  const choice = (decision: string) => request.choices?.find((item) => decisionOf(item.response) === decision)
  const accept = choice("accept"), decline = choice("decline"), cancel = choice("cancel")
  const reject = decline ?? cancel
  const questions = request.questions ?? []
  const secret = questions.some((question) => question.is_secret)
  const single = questions.length === 1 && questions[0].options?.length ? questions[0] : null
  const risk = approvalRisk(request, node?.capabilities?.roots ?? [])
  const elsewhere = secret ? "到执行设备上填写" : !questions.length && !accept && !reject ? "到执行设备上处理" : ""

  return <ActionCard
    icon={Hand}
    title={`需要你同意 · ${approvalKind(request)}`}
    meta={meta ?? nodeName(session.target_node, { nodes: workspace.nodes, directory })}
    menu={stoppable && decline && cancel ? <Popover.Root>
      <Popover.Trigger asChild><Button type="button" variant="ghost" size="icon" className="-my-2" aria-label="更多" title="更多"><Ellipsis aria-hidden="true" /></Button></Popover.Trigger>
      <Popover.Portal><Popover.Content align="end" sideOffset={4} className="z-50 rounded-xl border bg-popover p-1 shadow-lg">
        <ActionButton variant="ghost" className="text-danger" disabled={disabled} action={respond(cancel.response)}>停止这个任务</ActionButton>
      </Popover.Content></Popover.Portal>
    </Popover.Root> : undefined}
    actions={elsewhere || questions.length ? undefined : <>
      {reject ? <ActionButton disabled={disabled} action={respond(reject.response)}>{decline ? "拒绝" : "拒绝并停止"}</ActionButton> : null}
      {accept ? <ActionButton variant="default" disabled={disabled} action={respond(accept.response)}>同意一次</ActionButton> : null}
    </>}
  >
    {request.command ? <code className="block whitespace-pre-wrap break-all rounded-lg bg-background/70 px-3 py-2 font-mono text-meta">{request.command}</code> : null}
    {request.cwd ? <p className="break-all text-meta text-muted-foreground">在 {request.cwd}</p> : null}
    {risk ? <p className="flex items-center gap-1.5 text-meta font-medium text-warning"><TriangleAlert aria-hidden="true" className="size-4 shrink-0" />{risk}</p> : null}
    {request.files?.length ? <Details label={`会修改 ${request.files.length} 个文件`}>{request.files.map((file) => <span key={file} className="font-mono">{file}</span>)}</Details> : null}
    {request.reason ? <p className="text-meta"><span className="text-muted-foreground">原因：</span>{request.reason}</p> : null}
    {session.freshness !== "observed" ? <p role="status" className="text-meta text-muted-foreground">正在确认最新状态</p> : null}
    {elsewhere ? <p className="text-meta text-muted-foreground">{elsewhere}</p> : single ? <div className="grid gap-2">
      <p>{single.question}</p>
      <div className="flex flex-wrap gap-2">{single.options!.map((option) => <ActionButton key={option.label} disabled={disabled} action={respond({ answers: { [single.id]: { answers: [option.label] } } })}>{option.label}</ActionButton>)}</div>
    </div> : questions.length ? <form onSubmit={(event) => {
      event.preventDefault()
      if (disabled || busy) return
      const form = event.currentTarget
      if (!form.reportValidity()) return
      const values = new FormData(form)
      const generation = workspace.generation
      setBusyGeneration(generation)
      void workspace.run(async () => {
        const answers = Object.fromEntries(questions.map((question, index) => {
          const answer = String(values.get(`answer-${index}`) ?? "").trim()
          if (!answer) throw new Error("每个问题都要回答。")
          return [question.id, { answers: [answer] }]
        }))
        await respond({ answers })()
      }).finally(() => setBusyGeneration((owner) => owner === generation ? null : owner))
    }}>
      <fieldset disabled={disabled || busy} className="grid min-w-0 gap-3">
        {questions.map((question, index) => <Field key={question.id}>
          <FieldLabel htmlFor={`${id}-answer-${index}`}>{question.question}</FieldLabel>
          {question.options?.length ? <NativeSelect id={`${id}-answer-${index}`} name={`answer-${index}`} defaultValue="" required>
            <NativeSelectOption value="" disabled>请选择</NativeSelectOption>
            {question.options.map((option) => <NativeSelectOption key={option.label} value={option.label}>{option.label}</NativeSelectOption>)}
          </NativeSelect> : <Input id={`${id}-answer-${index}`} name={`answer-${index}`} required maxLength={50000} />}
        </Field>)}
        <div className="flex justify-end"><SubmitButton busy={busy}>发送</SubmitButton></div>
      </fieldset>
    </form> : null}
  </ActionCard>
}

/** Plain-language reason; the original text stays in the details. */
export function checkReason(check: PendingCheck): string {
  if (/restart/i.test(check.detail)) return "设备在助手工作时重启了，系统不会自动重做。"
  if (/stream closed|frame limit|disconnect|connection/i.test(check.detail)) return "和助手的连接断了，系统不会自动重做。"
  return "这次的结果没法自动确认，系统不会自动重做。"
}

function recheckSummary(result: Record<string, unknown>): string {
  if (result.pending === false) return "已经有最终结果了。"
  const groups = Array.isArray(result.process_groups) ? result.process_groups as { exited: boolean }[] : []
  if (!groups.length) return "还是查不到结果，请按实际情况选。"
  return groups.some((group) => !group.exited) ? "还有程序在运行，稍后再查。" : "相关程序都已结束，请按实际情况选。"
}

/** "需要确认": the person says whether interrupted work actually finished. Nothing is re-run. */
export function ConfirmCard({ check }: { check: PendingCheck }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const id = useId()
  const [note, setNote] = useState("")
  const [summary, setSummary] = useState("")
  const disabled = !workspace.writable || !workspace.selectedBot
  const native = check.kind === "native_turn"
  const answer = (choice: string, outcome: string) => async () => {
    const evidence = `用户在网页上选择：${choice}${note.trim() ? `。${note.trim()}` : ""}`
    await workspace.resolveCheck(check, evidence, native ? outcome : undefined)
  }
  return <ActionCard
    icon={CircleQuestionMark}
    title={`需要确认 · ${native ? "任务被打断" : "上次回复被打断"}`}
    meta={nodeName(check.target_node, { nodes: workspace.nodes, directory })}
    actions={<>
      <ActionButton variant="ghost" disabled={disabled} action={async () => { setSummary(""); setSummary(recheckSummary(await workspace.reconcileCheck(check))) }}>不确定，再查一次</ActionButton>
      <ActionButton disabled={disabled} action={answer("没做完", "interrupted")}>没做完</ActionButton>
      <ActionButton variant="default" disabled={disabled} action={answer("做完了", "completed")}>做完了</ActionButton>
    </>}
  >
    <p>{checkReason(check)}{native ? "那个任务实际做完了吗？" : "那次要做的事实际做完了吗？"}</p>
    {summary ? <p role="status" className="text-meta text-muted-foreground">{summary}</p> : null}
    <Details label="补充说明（可选）"><Textarea id={`${id}-note`} aria-label="补充说明" rows={2} maxLength={2000} value={note} disabled={disabled} onChange={(event) => setNote(event.target.value)} /></Details>
    {check.detail ? <Details><span>{check.detail}</span></Details> : null}
  </ActionCard>
}
