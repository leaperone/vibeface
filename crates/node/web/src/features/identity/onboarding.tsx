import { useState } from "react"
import { useNavigate } from "@tanstack/react-router"
import { Bot, MessageSquarePlus } from "lucide-react"
import { BusyIcon, InlineError } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { AiSettings } from "@/features/devices/ai-settings"
import { useWorkspace } from "@/features/workspace/context"
import { finishOnboarding, setOnboardingStep, useOnboarding } from "./onboarding-flag"

function Dots({ step }: { step: number }) {
  return <div className="flex items-center gap-3 text-meta text-muted-foreground">
    <span className="flex gap-1.5" aria-hidden="true">{[1, 2, 3, 4].map((item) => <span key={item} className={`size-2 rounded-full ${item <= step ? "bg-foreground" : "bg-muted-foreground/30"}`} />)}</span>
    <span className="sr-only">第 {step} 步，共 4 步</span>
  </div>
}

/** Steps 3 and 4 after creating a network here. Skipping step 3 still offers step 4; 待处理 and the composer point back to the device page. */
export function Onboarding() {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const step = useOnboarding() ?? 3
  const [name, setName] = useState("我的 Bot")
  const [busy, setBusy] = useState(false)
  const [botOn, setBotOn] = useState(false)
  const [error, setError] = useState<unknown>(null)
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
  return <main className="h-[var(--app-height,100dvh)] overflow-y-auto overscroll-contain">
    <div className="mx-auto grid w-full max-w-md gap-6 px-4 py-10 pb-[max(2.5rem,env(safe-area-inset-bottom))] sm:py-16">
      <header className="grid gap-3"><span className="text-heading font-semibold tracking-tight">Vibeface</span><Dots step={step} /></header>
      {step === 3 ? <section className="grid gap-4 rounded-xl border bg-card p-5">
        <h2 className="flex items-center gap-2 text-heading font-semibold"><Bot aria-hidden="true" className="size-5 text-muted-foreground" />让这台电脑运行 Bot</h2>
        <AiSettings onboarding onBot={setBotOn} />
        <div className="flex justify-between gap-2 border-t pt-4">
          <Button type="button" variant="ghost" className="-ms-3" onClick={() => setOnboardingStep(4)}>稍后设置</Button>
          <Button type="button" disabled={!botOn} onClick={() => setOnboardingStep(4)}>下一步</Button>
        </div>
      </section> : <section className="grid gap-4 rounded-xl border bg-card p-5">
        <h2 className="flex items-center gap-2 text-heading font-semibold"><MessageSquarePlus aria-hidden="true" className="size-5 text-muted-foreground" />创建第一个 Bot</h2>
        <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); void create() }}>
          <label className="grid gap-1.5 text-meta font-medium">名称<Input required maxLength={512} value={name} disabled={busy} onChange={(event) => setName(event.target.value)} /></label>
          <InlineError error={error} />
          <div className="flex justify-between gap-2">
            <Button type="button" variant="ghost" className="-ms-3" disabled={busy} onClick={done}>跳过</Button>
            <Button type="submit" disabled={busy || !workspace.writable}><BusyIcon busy={busy} />开始对话</Button>
          </div>
        </form>
      </section>}
    </div>
  </main>
}
