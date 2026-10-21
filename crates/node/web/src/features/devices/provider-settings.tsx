import { useEffect, useRef, useState } from "react"
import { Check, ChevronRight, CircleAlert, CircleCheck, LoaderCircle, MousePointer2, Pencil, Plus, Power, SquareTerminal, Trash2, X, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { BusyIcon, Details, InlineError, Skeleton, StatusPill, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@/features/identity/context"
import { parseNodeSettings, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderRole, type ProviderStatus } from "@/lib/node"

export const providerNames: Record<ProviderId, string> = { codex: "Codex", cursor: "Cursor" }
const icons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2 }
const roleLabels: Record<ProviderRole, string> = { agent: "助手", execution: "执行任务" }
const states: Record<ProviderStatus["state"], { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: "可用", tone: "success", icon: CircleCheck },
  needs_setup: { label: "需设置", tone: "warning", icon: CircleAlert },
  starting: { label: "启动中", tone: "neutral", icon: LoaderCircle },
  off: { label: "已关闭", tone: "neutral", icon: Power },
}

export function issueText(provider: ProviderId, issue: ProviderIssue): string {
  const name = providerNames[provider]
  switch (issue.code) {
    case "bin_missing": return `找不到 ${name} 程序`
    case "env_missing": return `缺少 ${issue.name}`
    case "launch_failed": return `${name} 没能启动`
    case "config_invalid": return `${name} 的配置文件读不了`
  }
}

/** Opens a provider's settings on this page and moves focus into them; every fix action lands here. */
export function openProvider(provider: ProviderId) {
  const card = document.getElementById(`provider-${provider}`)
  if (!(card instanceof HTMLDetailsElement)) return
  const behavior = matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth"
  const toggle = card.parentElement?.querySelector<HTMLInputElement>("[role=switch]")
  if (toggle && !toggle.checked) {
    toggle.scrollIntoView({ block: "center", behavior })
    toggle.focus({ preventScroll: true })
    return
  }
  card.open = true
  card.scrollIntoView({ block: "center", behavior })
  card.querySelector<HTMLElement>("[data-fix], input")?.focus({ preventScroll: true })
}

type Change = { provider: ProviderId; enabled?: boolean; roles?: ProviderRole[]; bin?: string; env?: Record<string, string | null> }

/**
 * Codex and Cursor on the device this browser is connected to: on or off, what each is used for,
 * its program and its variables. Values are write-only; the device only says which are set.
 */
export function ProviderSettings({ update, onChange }: { update?: NodeSettingsResponse | null; onChange?(value: NodeSettingsResponse): void }) {
  const { ready } = useDevice()
  const [response, setResponse] = useState<NodeSettingsResponse | null>(null)
  const [busy, setBusy] = useState("")
  const [error, setError] = useState<{ provider: ProviderId | null; cause: unknown } | null>(null)
  const sequence = useRef(0)

  async function call(key: string, change: Change | null) {
    if (!ready?.isCurrent()) return false
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const parsed = parseNodeSettings(await (change ? connection.request("configure_provider", change) : connection.request("node_settings")))
      if (owner !== sequence.current || !connection.isCurrent()) return false
      setResponse(parsed)
      if (change) onChange?.(parsed)
      return true
    } catch (cause) {
      if (owner === sequence.current) setError({ provider: change?.provider ?? null, cause })
      return false
    } finally {
      if (owner === sequence.current) setBusy("")
    }
  }
  useEffect(() => { setResponse(null); void call("load", null); return () => { sequence.current += 1 } }, [ready])
  useEffect(() => { if (update) setResponse(update) }, [update])
  const starting = response?.providers.some((item) => item.state === "starting")
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void call("load", null), 2000)
    return () => clearTimeout(timer)
  }, [response])

  if (!response) return error ? <InlineError error={error.cause} action={{ label: "重试", onClick: () => void call("load", null) }} /> : <div className="grid gap-2"><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
  return <ul className="grid gap-3">
    {response.providers.map((item) => <ProviderCard key={item.provider} item={item} busy={busy} error={error?.provider === item.provider ? error.cause : null} save={(key, change) => call(`${item.provider}:${key}`, { provider: item.provider, ...change })} />)}
  </ul>
}

function ProviderCard({ item, busy, error, save }: { item: ProviderStatus; busy: string; error: unknown; save(key: string, change: Omit<Change, "provider">): Promise<boolean> }) {
  const name = providerNames[item.provider], Icon = icons[item.provider], state = states[item.state]
  const [editing, setEditing] = useState<string | null>(null)
  const [value, setValue] = useState("")
  const [newName, setNewName] = useState("")
  const [newValue, setNewValue] = useState("")
  const [bin, setBin] = useState(item.bin ?? "")
  useEffect(() => setBin(item.bin ?? ""), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  const issue = item.issues[0]
  const fix = () => {
    if (issue?.code === "env_missing") { setEditing(issue.name); setValue("") }
    openProvider(item.provider)
  }
  const setVariable = (key: string, name: string, next: string | null) => save(key, { env: { [name]: next } })
  const addName = newName.trim().toUpperCase()

  return <li className="overflow-hidden rounded-xl border">
    <div className="flex min-h-14 items-center gap-3 px-3 py-2">
      <Icon aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
      <span className="grid min-w-0 flex-1">
        <span className="truncate font-medium">{name}</span>
        {item.enabled ? <span className="truncate text-meta text-muted-foreground">{item.roles.map((role) => roleLabels[role]).join("、") || "暂不使用"}{item.assistant && item.roles.includes("agent") ? " · 助手在用" : ""}</span> : null}
      </span>
      <StatusPill tone={state.tone} icon={state.icon} className={cn(item.state === "starting" && "[&>svg]:animate-spin")}>{state.label}</StatusPill>
      <input type="checkbox" role="switch" className="switch" aria-label={`启用 ${name}`} checked={item.enabled} disabled={working}
        onChange={(event) => void save("enabled", { enabled: event.target.checked })} />
    </div>
    {item.state === "needs_setup" && issue ? <p className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 px-3 pb-2 text-meta text-warning">
      <CircleAlert aria-hidden="true" className="size-4 shrink-0" />
      <span className="min-w-0">{issueText(item.provider, issue)}{item.issues.length > 1 ? `，还有 ${item.issues.length - 1} 项` : ""}</span>
      <button type="button" className="font-medium text-foreground underline underline-offset-4" onClick={fix}>去设置</button>
    </p> : null}
    <details id={`provider-${item.provider}`} className="details border-t">
      <summary className="flex min-h-11 items-center gap-1 px-3 text-meta text-muted-foreground hover:text-foreground">
        <ChevronRight aria-hidden="true" className="size-4 transition-transform" />设置
      </summary>
      <div className="grid gap-4 px-3 pb-3">
        {item.supports.length > 1 ? <fieldset disabled={working || !item.enabled} className="grid gap-2">
          <legend className="mb-2 text-meta font-medium text-muted-foreground">用于</legend>
          <div className="flex flex-wrap gap-2">
            {item.supports.map((role) => {
              const on = item.roles.includes(role)
              return <Button key={role} type="button" variant={on ? "outline" : "ghost"} aria-pressed={on} disabled={on && item.roles.length === 1}
                className={cn(on && "shadow-xs")} onClick={() => void save("roles", { roles: on ? item.roles.filter((r) => r !== role) : [...item.roles, role] })}>
                {on ? <Check aria-hidden="true" /> : <Plus aria-hidden="true" />}{roleLabels[role]}
              </Button>
            })}
          </div>
        </fieldset> : null}

        {item.bin !== null ? <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); void save("bin", { bin: bin.trim() }) }}>
          <label htmlFor={`${item.provider}-bin`} className="text-meta font-medium text-muted-foreground">程序</label>
          <div className="flex min-w-0 gap-2">
            <Input id={`${item.provider}-bin`} className="min-w-0 font-mono" placeholder="codex" value={bin} disabled={working}
              data-fix={issue?.code === "bin_missing" ? "" : undefined} onChange={(event) => setBin(event.target.value)} />
            {bin.trim() !== (item.bin ?? "") ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy === `${item.provider}:bin`} />保存</Button> : null}
          </div>
        </form> : null}

        <div className="grid gap-2">
          <p className="text-meta font-medium text-muted-foreground">环境变量</p>
          {item.env.length ? <ul className="grid gap-1">{item.env.map((row) => {
            const missing = !row.set && !row.inherited && row.required
            return <li key={row.name} className="grid gap-2 rounded-lg bg-muted/60 py-1 ps-3 pe-1">
              <div className="flex min-h-8 min-w-0 items-center gap-2">
                <span className="min-w-0 flex-1 truncate font-mono text-meta" title={row.name}>{row.name}</span>
                <span className={cn("shrink-0 text-meta", missing ? "text-warning" : "text-muted-foreground")}>{row.set ? "••••••" : row.inherited ? "沿用本机" : "未填"}</span>
                <Button type="button" variant="ghost" size="icon" aria-label={`${row.set ? "替换" : "填写"} ${row.name}`} title={row.set ? "替换" : "填写"} disabled={working}
                  data-fix={missing ? "" : undefined} onClick={() => { setEditing(row.name); setValue("") }}><Pencil aria-hidden="true" /></Button>
                {row.set ? <Button type="button" variant="ghost" size="icon" aria-label={`删除 ${row.name}`} title="删除" disabled={working}
                  onClick={() => void setVariable(`env:${row.name}`, row.name, null)}><Trash2 aria-hidden="true" /></Button> : null}
              </div>
              {editing === row.name ? <form className="flex min-w-0 gap-2" onSubmit={(event) => {
                event.preventDefault()
                void setVariable(`env:${row.name}`, row.name, value).then((ok) => { if (ok) { setEditing(null); setValue("") } })
              }}>
                <Input type="password" autoComplete="off" autoFocus aria-label={`${row.name} 的值`} className="min-w-0 bg-background" maxLength={4096} value={value} onChange={(event) => setValue(event.target.value)} />
                <Button type="submit" variant="outline" disabled={working || !value}><BusyIcon busy={busy === `${item.provider}:env:${row.name}`} />保存</Button>
                <Button type="button" variant="ghost" size="icon" aria-label="取消" title="取消" onClick={() => { setEditing(null); setValue("") }}><X aria-hidden="true" /></Button>
              </form> : null}
            </li>
          })}</ul> : null}
          <form className="grid min-w-0 gap-2 sm:grid-cols-[minmax(0,11rem)_minmax(0,1fr)_auto]" onSubmit={(event) => {
            event.preventDefault()
            void setVariable("add", addName, newValue).then((ok) => { if (ok) { setNewName(""); setNewValue("") } })
          }}>
            <Input aria-label="变量名" className="min-w-0 font-mono" placeholder="变量名" autoCapitalize="characters" spellCheck={false} maxLength={256} value={newName} disabled={working} onChange={(event) => setNewName(event.target.value)} />
            <div className="flex min-w-0 gap-2 sm:contents">
              <Input type="password" autoComplete="off" aria-label="值" className="min-w-0" placeholder="值" maxLength={4096} value={newValue} disabled={working} onChange={(event) => setNewValue(event.target.value)} />
              <Button type="submit" variant="outline" disabled={working || !addName || !newValue}><BusyIcon busy={busy === `${item.provider}:add`} icon={Plus} />添加</Button>
            </div>
          </form>
          <p className="text-meta text-muted-foreground">只存在这台电脑上，只给 {name} 用，保存后看不到原值</p>
        </div>
        {issue?.code === "launch_failed" ? <Details label="启动失败原因"><code className="font-mono">{issue.detail}</code></Details> : null}
      </div>
    </details>
    {error ? <InlineError error={error} className="px-3 pb-3" /> : null}
  </li>
}
