"use client"

import { useEffect, useRef, useState, type FormEvent } from "react"
import { Button } from "@/components/ui/button"
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { StatusOrb } from "@/components/status-orb"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@/features/identity/context"
import { messageOf } from "@/lib/node"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import type { BotProject } from "./types"

type BotFormProps = {
  open: boolean
  onOpenChange(open: boolean): void
  restoreFocus(): void
}

const projectKey = (project: BotProject) => JSON.stringify([project.target_node, project.cwd])

export function BotForm({ open, onOpenChange, restoreFocus }: BotFormProps) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const pending = workspace.pendingBot
  const rejected = pending?.submission?.state === "rejected"
  const [name, setName] = useState("")
  const [instructions, setInstructions] = useState("")
  const [selectedProject, setSelectedProject] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  const submitting = useRef(false)
  const wasOpen = useRef(false)
  useEffect(() => { submitting.current = false; setBusy(false); setError("") }, [workspace.generation])
  const projects = workspace.nodes.flatMap((node, index) => {
    if (node.revoked || !node.online || !node.capabilities?.manager) return []
    const deviceName = nodeName(node.peer_id, { nodes: workspace.nodes, directory }, node.local ? "本机" : `工作设备 ${index + 1}`)
    return (node.capabilities?.roots ?? []).map((cwd) => {
      const project = { target_node: node.peer_id, cwd }
      return { key: projectKey(project), project, label: `${deviceName} · ${cwd}` }
    })
  })
  const savedProject = pending?.project
  const savedProjectLabel = savedProject
    ? projects.find((item) => item.key === projectKey(savedProject))?.label ?? `${nodeName(savedProject.target_node, { nodes: workspace.nodes, directory }, "原工作设备")} · ${savedProject.cwd}`
    : "不关联项目"

  useEffect(() => {
    const initialize = open && (!wasOpen.current || Boolean(pending))
    wasOpen.current = open
    if (!initialize) return
    setName(pending?.name ?? "")
    setInstructions(pending?.instructions ?? "")
    setSelectedProject(pending?.project ? projectKey(pending.project) : "")
    setError("")
  }, [open, pending?.bot_id])

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (submitting.current || !event.currentTarget.reportValidity()) return
    const selected = selectedProject ? projects.find((item) => item.key === selectedProject) : undefined
    if (!pending && selectedProject && !selected) {
      setError("原项目当前不可用。请重新选择目录，或不关联项目。")
      return
    }
    submitting.current = true; setBusy(true); setError("")
    try {
      await workspace.createBot(pending ? {
        name: pending.name, instructions: pending.instructions, project: pending.project,
      } : {
        name, instructions, project: selected?.project,
      })
      if (workspace.isCurrent()) onOpenChange(false)
    } catch (cause) { if (workspace.isCurrent()) setError(messageOf(cause)) }
    finally { if (workspace.isCurrent()) { submitting.current = false; setBusy(false) } }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-lg" onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }}>
        <SheetHeader className="pr-14">
          <SheetTitle>{pending ? "核对机器人创建" : "创建机器人"}</SheetTitle>
          <SheetDescription>{pending ? "沿用这次的名称，再试一次。" : "起个名字。"}</SheetDescription>
        </SheetHeader>
        <form className="space-y-6 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]" onSubmit={(event) => { void submit(event) }}>
          {pending && !busy ? <p role="status" className="rounded-lg bg-muted p-3 text-sm">{rejected ? "原创建请求已被拒绝，核对没有创建后可以修改配置。" : "创建结果尚未确认，以下配置保持不变。恢复连接后可重试创建。"}</p> : null}
          <Field>
            <FieldLabel htmlFor="bot-name">名称</FieldLabel>
            <Input id="bot-name" name="bot-name" className="min-h-11" autoComplete="off" required maxLength={512} value={name} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setName(event.target.value)} placeholder="例如：项目助手" />
          </Field>
          <Field>
            <FieldLabel htmlFor="bot-instructions">职责说明 <span className="font-normal text-muted-foreground">可选</span></FieldLabel>
            <Textarea id="bot-instructions" name="bot-instructions" className="min-h-36" maxLength={8192} aria-describedby="bot-instructions-help" value={instructions} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setInstructions(event.target.value)} placeholder="例如：帮我维护项目，回答问题并检查修改。" />
            <FieldDescription id="bot-instructions-help">最多 8192 字节。</FieldDescription>
          </Field>
          <Field>
            <FieldLabel htmlFor="bot-project">关联项目 <span className="font-normal text-muted-foreground">可选</span></FieldLabel>
            {pending ? <Input id="bot-project" className="min-h-11" readOnly value={savedProjectLabel} aria-describedby="bot-project-help" /> : (
              <NativeSelect id="bot-project" name="bot-project" className="w-full [&>select]:min-h-11" value={selectedProject} disabled={busy} onChange={(event) => setSelectedProject(event.target.value)} aria-describedby="bot-project-help">
                <NativeSelectOption value="">不关联项目</NativeSelectOption>
                {selectedProject && !projects.some((item) => item.key === selectedProject) ? <NativeSelectOption value={selectedProject} disabled>原项目当前不可用，请重新选择</NativeSelectOption> : null}
                {projects.map((item) => <NativeSelectOption key={item.key} value={item.key}>{item.label}</NativeSelectOption>)}
              </NativeSelect>
            )}
            <FieldDescription id="bot-project-help">{projects.length || pending?.project ? "选择已授权设备上的目录，方便机器人处理项目。" : "还没有可用的项目目录。可在「设置」为节点启用 Agent 并添加项目目录，也可以先创建机器人。"}</FieldDescription>
          </Field>
          {error ? <p role="alert" className="text-sm text-destructive">{error}</p> : null}
          <div className="flex flex-wrap gap-3">
            <Button type="submit" className="min-h-11" disabled={busy || !workspace.writable} aria-busy={busy}>
              {busy ? <StatusOrb place="reconcile" size={20} /> : null}{busy ? pending ? "正在核对…" : "正在创建…" : pending ? rejected ? "核对创建结果" : "重试创建" : "创建机器人"}
            </Button>
            <Button type="button" className="min-h-11" variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
          </div>
          {!workspace.writable ? <p role="status" className="text-sm text-muted-foreground">{workspace.writeNotice || "恢复设备连接后可以提交。"}</p> : null}
        </form>
      </SheetContent>
    </Sheet>
  )
}
