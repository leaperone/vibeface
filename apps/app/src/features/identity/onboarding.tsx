import { AuthPage,FolderPath,InlineError,Panel,Steps } from "@/components/arc/product-kit"
import { AiSettings } from "@/features/devices/ai-settings"
import { pendingBotCopy } from "@/features/workspace/bot-form"
import { useNavigate } from "@tanstack/react-router"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { parseNodeSettings } from "@vibeface/client/lib/node.ts"
import { folderLabels,projectChoices } from "@vibeface/client/workspace/bot-settings.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { botReadiness } from "@vibeface/client/workspace/model.ts"
import type { PendingBot } from "@vibeface/client/workspace/types.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Input } from "@vibeface/uiarc/input/input.tsx"
import { Textarea } from "@vibeface/uiarc/textarea/textarea.tsx"
import { cooperationExamples } from '@vibeface/client/chat/first-cooperation.ts'
import { Bot,FolderOpen,MessageSquarePlus } from "lucide-react"
import { useEffect,useRef,useState } from "react"
import { finishOnboarding,setOnboardingStep } from "./onboarding-flag"

/** Steps 4 and 5 after creating `networkId` here. Skipping step 4 still offers step 5; 待处理 and the composer point back to the device page. */
export function Onboarding({ networkId, step }: { networkId: string; step: 4 | 5 }) {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const [busy, setBusy] = useState(false)
  // Its own request records the creation as pending until answered, so the form keeps what it showed when it was sent.
  const [sent, setSent] = useState<PendingBot | null>()
  const pending = sent === undefined ? workspace.pendingBot : sent
  const [name, setName] = useState(pending?.name ?? "我的 Bot")
  const [instructions, setInstructions] = useState(pending?.instructions ?? '')
  const [nameError, setNameError] = useState("")
  const nameInput = useRef<HTMLInputElement>(null)
  const [error, setError] = useState<unknown>(null)
  // A creation whose answer was lost is retried as it was sent, under its ids; the workspace refuses any other content.
  useEffect(() => { if (pending) { setName(pending.name); setInstructions(pending.instructions); setNameError("") } }, [pending?.bot_id])
  const canReply = workspace.nodes.some((node) => botReadiness(node) === "ready" || botReadiness(node) === "unverified" || botReadiness(node) === "starting")
  // Also why the create button is disabled; the workspace names only some of the reasons it cannot write.
  const readOnly = workspace.writable ? "" : workspace.readOnlyNotice || workspace.writeNotice || "正在读取这个节点，稍后就能创建"
  // The first bot works in this computer's first folder; its panel changes it later.
  const project = pending ? pending.project : projectChoices(workspace.nodes.filter((node) => node.local), () => "")[0]?.project
  // The folder reads as Settings shows it, `~/…`; the device only says that form in its settings.
  const { ready } = useDevice()
  const [shown, setShown] = useState<Record<string, string> | null>(null)
  useEffect(() => {
    if (step !== 5 || !ready) return
    let live = true
    ready.request("node_settings").then((raw) => parseNodeSettings(raw).display_paths, () => ({})).then((paths) => { if (live) setShown(paths) })
    return () => { live = false }
  }, [step, ready])
  const done = () => { finishOnboarding(); void navigate({ to: "/" }) }
  async function create() {
    setBusy(true); setSent(pending); setError(null)
    try {
      const bot = await workspace.createBot(pending ? { name: pending.name, instructions: pending.instructions, project: pending.project } : { name: name.trim(), instructions, project })
      if (!workspace.isCurrent()) return
      finishOnboarding()
      if (bot) void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } })
    } catch (cause) { if (workspace.isCurrent()) setError(cause) }
    finally { if (workspace.isCurrent()) { setBusy(false); setSent(undefined) } }
  }
  return <AuthPage header={<Steps step={step} />}>
    {step === 4 ? <Panel icon={Bot} title="让这台电脑运行 Bot">
      <AiSettings onboarding onNext={() => setOnboardingStep(networkId, 5)} />
    </Panel> : <Panel key="bot" icon={MessageSquarePlus} title="创建第一个 Bot">
      {pending ? <p id="bot-create-pending" role="status" className="text-sm text-secondary">{pending.submission?.state === "rejected" ? pendingBotCopy.rejected : pendingBotCopy.unconfirmed}</p>
        : <p className="text-sm text-secondary">{canReply ? "给它起个名字，创建后会打开和它的对话，在那里交代第一件事。" : "给它起个名字，创建后会打开和它的对话。暂时没有设备能回复，也可以先创建，之后在「设置 → 我的设备」中连接或设置。"}</p>}
      <form className="grid gap-4" noValidate onSubmit={(event) => { event.preventDefault(); if (busy) return; if (pending || name.trim()) void create(); else { setNameError("还没填名称"); nameInput.current?.focus() } }}>
        {/* Held read-only, not disabled, while sending: a field or button that disables under focus drops it to the page. */}
        <div className="grid gap-1.5 text-sm font-medium"><Input label="名称" ref={nameInput} error={nameError} required maxLength={512} value={name} readOnly={Boolean(pending) || busy} aria-describedby={pending ? "bot-create-pending" : undefined} onChange={(event) => { setName(event.target.value); setNameError("") }} /></div>
        <div className="grid gap-2"><Textarea label="职责与合作约定" description="可选；创建只保存定义，不启动模型或授予权限" maxLength={8192} value={instructions} readOnly={Boolean(pending) || busy} onChange={event => setInstructions(event.target.value)} placeholder="例如：帮我跟进项目，需要我选择时再问。" /><div className="flex flex-wrap gap-2">{cooperationExamples.map(example => <Button key={example.label} type="button" variant="secondary" size="sm" disabled={Boolean(pending) || busy} onClick={() => setInstructions(current => `${current.trim()}${current.trim() ? '\n\n' : ''}${example.text}`)}>{example.label}</Button>)}</div></div>
        {project && shown ? <p className="grid gap-1 text-sm">
          <span className="font-medium">{folderLabels.project}</span>
          <span className="flex min-w-0 items-center gap-1.5"><FolderOpen aria-hidden="true" className="size-4 shrink-0 text-secondary" /><FolderPath path={project.cwd} shown={shown[project.cwd]} /></span>
          <span className="text-secondary">之后可在 Bot 的设置里改</span>
        </p> : null}
        <InlineError error={error} />
        {/* Always mounted so a reason that appears is read out; empty, it takes no room above the footer. */}
        <div>
          <p id="bot-create-status" role="status" className="pb-4 text-sm text-secondary empty:pb-0">{readOnly}</p>
          {/* Wraps on narrow screens, the primary action staying at the end of the last line. */}
          <div className="flex flex-wrap justify-between gap-2 border-t pt-4">
            <Button type="button" variant="ghost" className="-ms-3" disabled={busy} onClick={() => setOnboardingStep(networkId, 4)}>上一步</Button>
            <div className="ms-auto flex flex-wrap justify-end gap-2">
              <Button type="button" variant="ghost" disabled={busy} onClick={done}>{pending ? "稍后再试" : "稍后创建"}</Button>
              <Button type="submit" loading={busy} disabled={!workspace.writable && !busy} aria-describedby={readOnly ? "bot-create-status" : undefined}>{pending ? pendingBotCopy.retry : canReply ? "开始对话" : "完成"}</Button>
            </div>
          </div>
        </div>
      </form>
    </Panel>}
  </AuthPage>
}
