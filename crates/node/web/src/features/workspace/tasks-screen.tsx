"use client"

import { useState } from "react"
import { ChevronDownIcon, PlusIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useWorkspace } from "./context"
import { stateNames } from "./labels"
import { DetailList, StatusBadge, SubmitButton } from "./present"
import { newId } from "./storage"
import type { TaskRecord } from "./types"

const taskStatuses = ["open", "working", "blocked", "needs_review", "accepted", "cancelled"] as const

export function TasksScreen() {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [autoFollowup, setAutoFollowup] = useState(false)
  const pending = workspace.pendingTask
  const tasks = workspace.data?.tasks ?? []

  return (
    <section className="workspace-screen space-y-5" aria-label="工单" hidden={workspace.view !== "tasks"}>
      <Collapsible open={open || Boolean(pending)} onOpenChange={setOpen}>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-lg font-semibold">工单 <span className="text-sm font-normal text-muted-foreground">{tasks.length}</span></h2>
          <CollapsibleTrigger asChild disabled={Boolean(pending) || !workspace.conversationId}>
            <Button type="button"><PlusIcon />{pending ? "工单待确认" : open ? "收起表单" : "新建工单"}</Button>
          </CollapsibleTrigger>
        </div>
        <CollapsibleContent forceMount className="pt-4 data-[state=closed]:hidden">
          <Card>
            <CardContent className="space-y-4">
              {pending ? <p role="status" className="text-sm text-amber-700 dark:text-amber-400">创建结果待确认。保留原内容重试，不会新建重复工单。</p> : null}
              <form key={pending?.task_id ?? workspace.conversationId}
                onSubmit={(event) => {
                  event.preventDefault()
                  const form = event.currentTarget
                  if (!form.reportValidity()) return
                  setBusy(true)
                  void workspace.run(async () => {
                    await workspace.createTask({
                      taskId: pending?.task_id ?? newId(),
                      title: String(new FormData(form).get("title") ?? "").trim(),
                      goal: String(new FormData(form).get("goal") ?? "").trim(),
                      acceptance: String(new FormData(form).get("acceptance") ?? "").trim(),
                      autoFollowup: pending?.auto_followup ?? autoFollowup,
                    })
                    form.reset()
                    setAutoFollowup(false)
                    setOpen(false)
                  }).finally(() => setBusy(false))
                }}
              >
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="task-title">标题</FieldLabel>
                    <Input defaultValue={pending?.title ?? ""} readOnly={Boolean(pending)} id="task-title" name="title" required maxLength={200} />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="task-goal">目标</FieldLabel>
                    <Textarea defaultValue={pending?.goal ?? ""} readOnly={Boolean(pending)} id="task-goal" name="goal" rows={3} required maxLength={50000} />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="task-acceptance">验收条件</FieldLabel>
                    <Textarea defaultValue={pending?.acceptance ?? ""} readOnly={Boolean(pending)} id="task-acceptance" name="acceptance" rows={3} required maxLength={50000} />
                  </Field>
                  <Field>
                    <div className="flex items-center gap-2">
                      <Checkbox id="task-followup" disabled={Boolean(pending) || !workspace.canAutoFollowup} checked={pending?.auto_followup ?? autoFollowup} onCheckedChange={(checked: boolean | "indeterminate") => setAutoFollowup(checked === true)} />
                      <FieldLabel htmlFor="task-followup">完成执行后，让调度 Agent 继续检查</FieldLabel>
                    </div>
                    {!workspace.canAutoFollowup ? <p className="text-sm text-muted-foreground">当前入口未启用接入角色，无法自动检查。</p> : null}
                  </Field>
                  <SubmitButton busy={busy}>{pending ? "重试确认创建" : "创建工单"}</SubmitButton>
                </FieldGroup>
              </form>
            </CardContent>
          </Card>
        </CollapsibleContent>
      </Collapsible>
      <div className="grid gap-4">
        {tasks.length ? tasks.map((task) => (
          <TaskCard key={task.task_id} task={task} conversationId={workspace.conversationId} />
        )) : <p className="py-10 text-center text-sm text-muted-foreground">{workspace.conversationId ? "还没有工单，创建一个目标开始。" : "先选择或新建会话，再创建工单。"}</p>}
      </div>
    </section>
  )
}

function TaskCard({ task, conversationId }: { task: TaskRecord; conversationId: string }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const status = taskStatuses.find((value) => value === task.status) ?? "open"
  return (
    <Card>
      <CardHeader className="flex flex-wrap items-start justify-between gap-3">
        <CardTitle className="min-w-0 flex-1 break-words">{task.title || "未命名工单"}</CardTitle>
        <StatusBadge value={task.status} label={task.status === "accepted" ? "已验收" : undefined} />
      </CardHeader>
      <CardContent>
        {task.detail ? <p className="line-clamp-3 whitespace-pre-line break-words text-sm text-muted-foreground">{task.detail}</p> : null}
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">查看详情<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-3">
            <DetailList pairs={[["目标", task.goal], ["验收条件", task.acceptance], ["完整进展", task.detail || "暂无进展"]]} />
          </CollapsibleContent>
        </Collapsible>
        <Collapsible>
          <CollapsibleTrigger asChild>
            <Button type="button" variant="ghost" className="h-auto min-h-9 gap-2 px-0 text-muted-foreground [&[data-state=open]>svg]:rotate-180">更新工单状态<ChevronDownIcon className="size-4 shrink-0 transition-transform" /></Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="pt-3">
            <form
              onSubmit={(event) => {
                event.preventDefault()
                const form = event.currentTarget
                if (!form.reportValidity()) return
                const data = new FormData(form)
                setBusy(true)
                void workspace.run(async () => {
                  await workspace.updateTask(conversationId, task.task_id, String(data.get("status") ?? ""), String(data.get("detail") ?? ""))
                }).finally(() => setBusy(false))
              }}
            >
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor={`task-status-${task.task_id}`}>状态</FieldLabel>
                  <NativeSelect id={`task-status-${task.task_id}`} name="status" defaultValue={status}>
                    {taskStatuses.map((value) => (
                      <NativeSelectOption key={value} value={value}>{value === "accepted" ? "已验收" : stateNames[value]}</NativeSelectOption>
                    ))}
                  </NativeSelect>
                </Field>
                <Field>
                  <FieldLabel htmlFor={`task-detail-${task.task_id}`}>说明</FieldLabel>
                  <Textarea id={`task-detail-${task.task_id}`} name="detail" rows={3} required defaultValue={task.detail || ""} />
                </Field>
                <SubmitButton busy={busy} variant="secondary">保存状态</SubmitButton>
              </FieldGroup>
            </form>
          </CollapsibleContent>
        </Collapsible>
      </CardContent>
    </Card>
  )
}
