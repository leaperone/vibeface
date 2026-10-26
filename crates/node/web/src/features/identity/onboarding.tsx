import { useState } from "react"
import { useNavigate } from "@tanstack/react-router"
import { Bot, MessageSquarePlus } from "lucide-react"
import { AuthPage, BusyIcon, InlineError, Panel, Steps } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { AiSettings } from "@/features/devices/ai-settings"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { botReadiness } from "@vibeface/client/workspace/model.ts"
import { finishOnboarding, setOnboardingStep, useOnboarding } from "./onboarding-flag"

/** Steps 4 and 5 after creating a network here. Skipping step 4 still offers step 5; 待处理 and the composer point back to the device page. */
export function Onboarding() {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const step = useOnboarding() ?? 4
  const [name, setName] = useState("我的 Bot")
  const [busy, setBusy] = useState(false)
  const [botOn, setBotOn] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const canReply = workspace.nodes.some((node) => botReadiness(node) === "ready")
  const done = () => { finishOnboarding(); void navigate({ to: "/" }) }
  async function create() {
    setBusy(true); setError(null)
    try {
      const bot = await workspace.createBot({ name: name.trim(), instructions: "" })
      if (!workspace.isCurrent()) return
      finishOnboarding()
      if (bot) void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } })
    } catch (cause) { if (workspace.isCurrent()) setError(cause) }
    finally { if (workspace.isCurrent()) setBusy(false) }
  }
  return <AuthPage header={<Steps step={step} />}>
    {step === 4 ? <Panel icon={Bot} title="让这台电脑运行 Bot">
      <AiSettings onboarding onBot={setBotOn} />
      <div className="flex justify-between gap-2 border-t pt-4">
        <Button type="button" variant="ghost" className="-ms-3" onClick={() => setOnboardingStep(5)}>稍后设置</Button>
        <Button type="button" disabled={!botOn} onClick={() => setOnboardingStep(5)}>下一步</Button>
      </div>
    </Panel> : <Panel key="bot" icon={MessageSquarePlus} title="创建第一个 Bot">
      <form className="grid gap-4" noValidate onSubmit={(event) => { event.preventDefault(); if (name.trim()) void create(); else setError(new Error("还没填名称")) }}>
        <label className="grid gap-1.5 text-meta font-medium">名称<Input required maxLength={512} value={name} disabled={busy} onChange={(event) => setName(event.target.value)} /></label>
        <InlineError error={error} />
        <div className="flex justify-between gap-2 border-t pt-4">
          <Button type="button" variant="ghost" className="-ms-3" disabled={busy} onClick={() => setOnboardingStep(4)}>上一步</Button>
          <div className="flex gap-2">
            <Button type="button" variant="ghost" disabled={busy} onClick={done}>跳过</Button>
            <Button type="submit" disabled={busy || !workspace.writable}><BusyIcon busy={busy} />{canReply ? "开始对话" : "完成"}</Button>
          </div>
        </div>
      </form>
    </Panel>}
  </AuthPage>
}
