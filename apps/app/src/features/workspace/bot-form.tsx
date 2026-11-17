import { Select } from "@vibeface/uiarc/select/select.tsx"
"use client"

import { FieldNote,FormField } from "@/components/arc/form-field"
import { BusyIcon,InlineError } from "@/components/arc/product-kit"
import { Link,useNavigate } from "@tanstack/react-router"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { folderLabels,folderSetup,projectChoices,projectKey } from "@vibeface/client/workspace/bot-settings.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Input } from "@vibeface/uiarc/input/input.tsx"
import { Textarea } from "@vibeface/uiarc/textarea/textarea.tsx"
import { ChevronRight,Plus } from "lucide-react"
import { useEffect,useRef,useState,type FormEvent } from "react"

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
  const [nameError, setNameError] = useState("")
  const nameInput = useRef<HTMLInputElement>(null)
  const submitting = useRef(false)
  const pendingId = useRef<string | null>(null)
  useEffect(() => { submitting.current = false; setBusy(false); setError(""); setNameError("") }, [workspace.generation])
  const setup = ready ? folderSetup(workspace.nodes, ready.principal, ready.entry.device_id) : undefined
  const botDevice = setup?.add ?? false
  const setupDevice = pending ? undefined : setup?.device
  const projects = projectChoices(workspace.nodes, (node, index) => nodeName(node.device_id, { nodes: workspace.nodes, directory }, node.local ? "这台电脑" : `设备 ${index + 1}`))
  const savedProject = pending?.project
  const savedProjectLabel = savedProject
    ? projects.find((item) => item.key === projectKey(savedProject))?.label ?? `${nodeName(savedProject.target_node, { nodes: workspace.nodes, directory }, "原来的设备")} · ${savedProject.cwd}`
    : "不选"

  useEffect(() => {
    const previous = pendingId.current
    pendingId.current = pending?.bot_id ?? null
    // Refresh can confirm creation while this drawer is closed. A rejected request keeps its editable fields.
    if (!pending && previous && workspace.bots.some((bot) => bot.bot_id === previous)) clearDraft()
  }, [pending?.bot_id, workspace.bots])

  useEffect(() => {
    if (!open) return
    // The shell retains unsent fields across close/settings; its network/principal key isolates drafts.
    if (pending) {
      setName(pending.name)
      setInstructions(pending.instructions)
      setSelectedProject(pending.project ? projectKey(pending.project) : "")
    }
    setError("")
    setNameError("")
  }, [open, pending?.bot_id, workspace.generation])

  function clearDraft() {
    setName("")
    setInstructions("")
    setSelectedProject("")
    setError("")
    setNameError("")
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (submitting.current) return
    if (!pending && !name.trim()) { setNameError("还没填名称"); nameInput.current?.focus(); return }
    const selected = selectedProject ? projects.find((item) => item.key === selectedProject) : undefined
    if (!pending && selectedProject && !selected) {
      setError("这个文件夹现在用不了，换一个或不选。")
      return
    }
    submitting.current = true; setBusy(true); setError(""); setNameError("")
    try {
      const bot = await workspace.createBot(pending ? {
        name: pending.name, instructions: pending.instructions, project: pending.project,
      } : {
        name: name.trim(), instructions, project: selected?.project,
      })
      if (workspace.isCurrent()) { if (bot) clearDraft(); onOpenChange(false); if (bot) void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } }) }
    } catch (cause) { if (workspace.isCurrent()) setError(messageOf(cause)) }
    finally { if (workspace.isCurrent()) { submitting.current = false; setBusy(false) } }
  }

  return (
    <Drawer open={open} onOpenChange={onOpenChange}>
      <DrawerContent title={pending ? "再试一次" : "新 Bot"} description={pending ? "上次没确认是否建好，内容保持不变。" : "给它起个名字，创建后再交代想完成的事。"} className="!w-full max-sm:!border-l-0 overflow-y-auto sm:!max-w-lg" onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }}>
        <form className="space-y-6 pb-[env(safe-area-inset-bottom)]" noValidate onSubmit={(event) => { void submit(event) }}>
          {pending && rejected && !busy ? <p role="status" className="rounded-control bg-surface-muted p-3 text-sm">上次没建成，确认后可以修改。</p> : null}
          <FormField>
            <Input label="名称" id="bot-name" name="bot-name" ref={nameInput} error={nameError} autoComplete="off" required maxLength={512} value={name} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => { setName(event.target.value); setNameError("") }} placeholder="例如：项目 Bot" />
          </FormField>
          <FormField>
            <Textarea label="它负责什么" description="可选" id="bot-instructions" name="bot-instructions" maxLength={8192} value={instructions} readOnly={Boolean(pending)} disabled={busy} onChange={(event) => setInstructions(event.target.value)} placeholder="例如：帮我维护项目，回答问题并检查修改。" />
          </FormField>
          <FormField>
            {pending ? <Input label={folderLabels.project} description="可选" id="bot-project" readOnly value={savedProjectLabel} aria-describedby={pending.project ? undefined : "bot-project-help"} /> : (
              <Select label={folderLabels.project} description="可选" id="bot-project" name="bot-project" className="w-full" disabled={busy} value={"arc:" + selectedProject} onValueChange={selected => { const value = selected.slice(4);  setSelectedProject(value)  }} options={[{ value: "arc:", label: "不选" }, ...(selectedProject && !projects.some((item) => item.key === selectedProject) ? [{ value: "arc:" + selectedProject, label: "原来的文件夹用不了", disabled: true }] : []), ...projects.map((item) => ({ value: "arc:" + item.key, label: item.label }))]} />
            )}
            {projects.length || pending?.project ? null : <FieldNote id="bot-project-help" className="flex flex-wrap items-center gap-x-2">
              {botDevice ? "还没有可用的文件夹" : "设备能运行 Bot 后就能选"}
              {setupDevice ? <span><Link to="/devices/$deviceId" params={{ deviceId: setupDevice }} className="hit-area relative inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11" onClick={() => onOpenChange(false)}>
                {botDevice ? "去添加" : "去设置"}<ChevronRight aria-hidden="true" className="size-3.5" />
              </Link></span> : pending || !ready ? null : <span>在管理设备上设置</span>}
            </FieldNote>}
          </FormField>
          <InlineError error={error} />
          <div className="flex flex-wrap justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => { if (!pending) clearDraft(); onOpenChange(false) }}>取消</Button>
            <Button type="submit" disabled={busy || !workspace.writable} aria-busy={busy || undefined}>
              <BusyIcon busy={busy} icon={Plus} />{pending ? "再试一次" : "创建"}
            </Button>
          </div>
          {!workspace.writable ? <p role="status" className="text-sm text-secondary">{workspace.writeNotice || "连上后就能创建。"}</p> : null}
        </form>
      </DrawerContent>
    </Drawer>
  )
}
