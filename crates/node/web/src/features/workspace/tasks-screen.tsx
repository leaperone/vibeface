"use client"

import { useState } from "react"
import { Checkbox } from "@/components/ui/checkbox"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { useWorkspace } from "./context"
import { stateNames } from "./labels"
import { DetailList, EmptyState, StatusBadge, SubmitButton } from "./present"
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
    <section aria-label="工单" hidden={workspace.view !== "tasks"}>
      <div>
        <h2>把目标写清楚</h2>
        <p>执行器结束一轮，工单仍需按验收条件确认。</p>
      </div>
      {tasks.length ? tasks.map((task) => (
        <TaskCard key={task.task_id} task={task} conversationId={workspace.conversationId} />
      )) : <EmptyState title="还没有工单" detail="写下目标和验收条件，再与调度 Agent 讨论。" />}
      <Collapsible open={open} onOpenChange={setOpen}>
        <CollapsibleTrigger>新建工单</CollapsibleTrigger>
        <CollapsibleContent>
          <form key={pending?.task_id ?? workspace.conversationId}
            onSubmit={(event) => {
              event.preventDefault()
              const form = event.currentTarget
              if (!form.reportValidity()) return
              setBusy(true)
              void workspace.run(async () => {
                await workspace.createTask({
                  taskId: newId(),
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
                {!workspace.canAutoFollowup ? <p>当前入口未启用接入角色，无法授权自动继续检查。</p> : null}
              </Field>
              <SubmitButton busy={busy}>创建工单</SubmitButton>
            </FieldGroup>
          </form>
        </CollapsibleContent>
      </Collapsible>
    </section>
  )
}

function TaskCard({ task, conversationId }: { task: TaskRecord; conversationId: string }) {
  const workspace = useWorkspace()
  const [busy, setBusy] = useState(false)
  const status = taskStatuses.find((value) => value === task.status) ?? "open"
  return (
    <Card>
      <CardHeader>
        <CardTitle>{task.title || "未命名工单"}</CardTitle>
        <StatusBadge value={task.status} label={task.status === "accepted" ? "已验收" : undefined} />
      </CardHeader>
      <CardContent>
        <DetailList pairs={[["目标", task.goal], ["验收条件", task.acceptance], ["进展", task.detail || "暂无进展"]]} />
        <Collapsible>
          <CollapsibleTrigger>更新工单状态</CollapsibleTrigger>
          <CollapsibleContent>
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
