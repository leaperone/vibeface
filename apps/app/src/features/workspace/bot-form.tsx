"use client"

import { useEffect, useRef, useState, type FormEvent } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ChevronRight, Plus } from "lucide-react"
import { Button } from "@/components/ui/button"
import { BusyIcon, InlineError } from "@/components/app/kit"
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { folderLabels, folderSetup, projectChoices, projectKey } from "@vibeface/client/workspace/bot-settings.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"

type BotFormProps = {
  open: boolean
  onOpenChange(open: boolean): void
  restoreFocus(): void
}


export function BotForm({ open, onOpenChange, restoreFocus }: BotFormProps) {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const { directory, ready } = useDevice()
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
  const setup = ready ? folderSetup(workspace.nodes, ready.principal, ready.entry.device_id) : undefined
  const botDevice = setup?.add ?? false
  const setupDevice = pending ? undefined : setup?.device
  const projects = projectChoices(workspace.nodes, (node, index) => nodeName(node.device_id, { nodes: workspace.nodes, directory }, node.local ? "这台电脑" : `设备 ${index + 1}`))
  const savedProject = pending?.project
  const savedProjectLabel = savedProject
    ? projects.find((item) => item.key === projectKey(savedProject))?.label ?? `${nodeName(savedProject.target_node, { nodes: workspace.nodes, directory }, "原来的设备")} · ${savedProject.cwd}`
    : "不选"

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
    if (submitting.current) return
    if (!pending && !name.trim()) { setError("还没填名称"); return }
    const selected = selectedProject ? projects.find((item) => item.key === selectedProject) : undefined
    if (!pending && selectedProject && !selected) {
      setError("这个文件夹现在用不了，换一个或不选。")
      return
    }
    submitting.current = true; setBusy(true); setError("")
    try {
      const bot = await workspace.createBot(pending ? {
        name: pending.name, instructions: pending.instructions, project: pending.project,
      } : {
        name: name.trim(), instructions, project: selected?.project,
      })
      if (workspace.isCurrent()) { onOpenChange(false); if (bot) void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } }) }
    } catch (cause) { if (workspace.isCurrent()) setError(messageOf(cause)) }
    finally { if (workspace.isCurrent()) { submitting.current = false; setBusy(false) } }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="!w-full max-sm:!border-l-0 overflow-y-auto sm:!max-w-lg" onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }}>
        <SheetHeader className="pr-14">
          <SheetTitle>{pending ? "再试一次" : "新 Bot"}</SheetTitle>
          <SheetDescription className={pending ? undefined : "sr-only"}>{pending ? "上次没确认是否建好，内容保持不变。" : "起个名字，就能开始聊。"}</SheetDescription>
        </SheetHeader>
        <form className="space-y-6 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]" noValidate onSubmit={(event) => { void submit(event) }}>
          {pending && rejected && !busy ? <p role="status" className="rounded-lg bg-muted p-3 text-meta">上次没建成，确认后可以修改。</p> : null}
          <Field>
            <FieldLabel htmlFor="bot-name">名称</FieldLabel>
            <Input id="bot-name" name="bot-name" className="min-h-11" autoComplete="off" required maxLength={512} value={name} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setName(event.target.value)} placeholder="例如：项目 Bot" />
          </Field>
          <Field>
            <FieldLabel htmlFor="bot-instructions">它负责什么 <span className="font-normal text-muted-foreground">可选</span></FieldLabel>
            <Textarea id="bot-instructions" name="bot-instructions" className="min-h-36" maxLength={8192} value={instructions} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setInstructions(event.target.value)} placeholder="例如：帮我维护项目，回答问题并检查修改。" />
          </Field>
          <Field>
            <FieldLabel htmlFor="bot-project">{folderLabels.project} <span className="font-normal text-muted-foreground">可选</span></FieldLabel>
            {pending ? <Input id="bot-project" className="min-h-11" readOnly value={savedProjectLabel} aria-describedby={pending.project ? undefined : "bot-project-help"} /> : (
              <NativeSelect id="bot-project" name="bot-project" className="w-full [&>select]:min-h-11" value={selectedProject} disabled={busy} onChange={(event) => setSelectedProject(event.target.value)} aria-describedby={projects.length ? undefined : "bot-project-help"}>
                <NativeSelectOption value="">不选</NativeSelectOption>
                {selectedProject && !projects.some((item) => item.key === selectedProject) ? <NativeSelectOption value={selectedProject} disabled>原来的文件夹用不了</NativeSelectOption> : null}
                {projects.map((item) => <NativeSelectOption key={item.key} value={item.key}>{item.label}</NativeSelectOption>)}
              </NativeSelect>
            )}
            {projects.length || pending?.project ? null : <FieldDescription id="bot-project-help" className="flex flex-wrap items-center gap-x-2">
              {botDevice ? "还没有可用的文件夹" : "设备能运行 Bot 后就能选"}
              {setupDevice ? <span><Link to="/devices/$deviceId" params={{ deviceId: setupDevice }} className="hit-area relative inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11" onClick={() => onOpenChange(false)}>
                {botDevice ? "去添加" : "去设置"}<ChevronRight aria-hidden="true" className="size-3.5" />
              </Link></span> : pending || !ready ? null : <span>在管理设备上设置</span>}
            </FieldDescription>}
          </Field>
          <InlineError error={error} />
          <div className="flex flex-wrap justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy || !workspace.writable} aria-busy={busy || undefined}>
              <BusyIcon busy={busy} icon={Plus} />{pending ? "再试一次" : "创建"}
            </Button>
          </div>
          {!workspace.writable ? <p role="status" className="text-meta text-muted-foreground">{workspace.writeNotice || "连上后就能创建。"}</p> : null}
        </form>
      </SheetContent>
    </Sheet>
  )
}
