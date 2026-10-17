"use client"

import { useState } from "react"
import { PlusIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Card, CardContent } from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { useWorkspace } from "./context"
import { SubmitButton } from "./present"
import { newId } from "./storage"
import { TaskActivity } from "./task-activity"

export function TasksScreen() {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const pending = workspace.pendingTask
  const tasks = workspace.data?.tasks ?? []

  return (
    <section className="workspace-screen space-y-5" aria-label="工单" hidden={workspace.view !== "tasks"}>
      <Collapsible open={open || Boolean(pending)} onOpenChange={setOpen}>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-lg font-semibold">工单 <span className="text-sm font-normal text-muted-foreground">{tasks.length}</span></h2>
          <CollapsibleTrigger asChild disabled={Boolean(pending) || !workspace.conversationId || !workspace.writable}>
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
                      autoFollowup: pending?.auto_followup ?? false,
                    })
                    form.reset()
                    setOpen(false)
                  }).finally(() => setBusy(false))
                }}
              >
                <fieldset disabled={!workspace.writable} className="min-w-0">
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
                    <SubmitButton busy={busy}>{pending ? "重试确认创建" : "创建工单"}</SubmitButton>
                  </FieldGroup>
                </fieldset>
              </form>
            </CardContent>
          </Card>
        </CollapsibleContent>
      </Collapsible>
      <div className="grid gap-4">
        {tasks.length ? tasks.map((task) => (
          <TaskActivity key={task.task_id} task={task} conversationId={workspace.conversationId} />
        )) : <p className="py-10 text-center text-sm text-muted-foreground">{workspace.conversationId ? "还没有工单，创建一个目标开始。" : "先选择或新建会话，再创建工单。"}</p>}
      </div>
    </section>
  )
}
