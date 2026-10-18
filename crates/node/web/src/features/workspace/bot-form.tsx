"use client"

import { useEffect, useRef, useState, type FormEvent } from "react"
import { Button } from "@/components/ui/button"
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Spinner } from "@/components/ui/spinner"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@/features/identity/context"
import { messageOf } from "@/lib/node"
import { useWorkspace } from "./context"
import type { BotProject, ConversationSummary } from "./types"

type BotFormProps = {
  open: boolean
  onOpenChange(open: boolean): void
  restoreFocus(): void
  conversation?: ConversationSummary
}

const projectKey = (project: BotProject) => JSON.stringify([project.target_node, project.cwd])

export function BotForm({ open, onOpenChange, restoreFocus, conversation }: BotFormProps) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const pending = workspace.pendingBot
  const [name, setName] = useState("")
  const [instructions, setInstructions] = useState("")
  const [selectedProject, setSelectedProject] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  const submitting = useRef(false)
  const projects = workspace.nodes.flatMap((node, index) => {
    if (node.revoked || !node.online || !node.roles.includes("executor")) return []
    const deviceName = directory?.members.find((member) => member.subject_id === node.node_id)?.name || (node.local ? "本机" : `执行设备 ${index + 1}`)
    return (node.capabilities?.roots ?? []).map((cwd) => {
      const project = { target_node: node.node_id, cwd }
      return { key: projectKey(project), project, label: `${deviceName} · ${cwd}` }
    })
  })
  const source = pending ? workspace.conversations.find((item) => item.conversation_id === pending.conversation_id && !workspace.bots.some((bot) => bot.conversation_id === item.conversation_id)) : conversation
  const savedProject = pending?.project
  const savedProjectLabel = savedProject
    ? projects.find((item) => item.key === projectKey(savedProject))?.label ?? `${directory?.members.find((member) => member.subject_id === savedProject.target_node)?.name || "原执行设备"} · ${savedProject.cwd}`
    : "不关联项目"

  useEffect(() => {
    if (!open) return
    setName(pending?.name ?? conversation?.title ?? "")
    setInstructions(pending?.instructions ?? "")
    setSelectedProject(pending?.project ? projectKey(pending.project) : "")
    setError("")
  }, [open, pending?.bot_id, conversation?.conversation_id])

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
        name: pending.name, instructions: pending.instructions, project: pending.project, conversation_id: pending.conversation_id,
      } : {
        name, instructions, project: selected?.project, conversation_id: conversation?.conversation_id,
      })
      onOpenChange(false)
    } catch (cause) { setError(messageOf(cause)) }
    finally { submitting.current = false; setBusy(false) }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-lg" onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }}>
        <SheetHeader className="pr-14">
          <SheetTitle>{pending ? "核对机器人创建" : "创建机器人"}</SheetTitle>
          <SheetDescription>{pending ? "原配置已保存。重试会继续核对同一个机器人。" : "为机器人起个名字，告诉它需要帮你做什么。"}</SheetDescription>
        </SheetHeader>
        <form className="space-y-6 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]" onSubmit={(event) => { void submit(event) }}>
          {pending && !busy ? <p role="status" className="rounded-lg bg-muted p-3 text-sm">创建结果尚未确认，以下配置保持不变。恢复连接后可重试创建。</p> : null}
          <Field>
            <FieldLabel htmlFor="bot-name">名称</FieldLabel>
            <Input id="bot-name" name="bot-name" className="min-h-11" autoComplete="off" required maxLength={512} value={name} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setName(event.target.value)} placeholder="例如：项目助手" />
          </Field>
          <Field>
            <FieldLabel htmlFor="bot-instructions">职责说明 <span className="font-normal text-muted-foreground">可选</span></FieldLabel>
            <Textarea id="bot-instructions" name="bot-instructions" className="min-h-36" value={instructions} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setInstructions(event.target.value)} placeholder="例如：帮我维护项目，回答问题并检查修改。" />
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
            <FieldDescription id="bot-project-help">{projects.length || pending?.project ? "选择已授权设备上的目录，方便机器人处理项目。" : "还没有可用的项目目录，也可以先创建机器人。"}</FieldDescription>
          </Field>
          {source ? <div className="space-y-1 rounded-lg bg-muted p-3 text-sm"><p className="font-medium">保留旧聊天记录</p><p className="break-words">{source.title || "未命名聊天"}</p><p className="text-muted-foreground">创建后可在机器人中继续这段聊天。</p></div> : null}
          {error ? <p role="alert" className="text-sm text-destructive">{error}</p> : null}
          <div className="flex flex-wrap gap-3">
            <Button type="submit" className="min-h-11" disabled={busy || !workspace.writable} aria-busy={busy}>
              {busy ? <Spinner /> : null}{busy ? pending ? "正在核对…" : "正在创建…" : pending ? "重试创建" : "创建机器人"}
            </Button>
            <Button type="button" className="min-h-11" variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
          </div>
          {!workspace.writable ? <p role="status" className="text-sm text-muted-foreground">{workspace.writeNotice || "恢复设备连接后可以提交。"}</p> : null}
        </form>
      </SheetContent>
    </Sheet>
  )
}
