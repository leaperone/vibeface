import { useEffect, useRef, useState } from "react"
import { Check, ChevronRight, CircleAlert, CircleCheck, CircleDashed, LoaderCircle, Monitor, MousePointer2, Pencil, Play, PlugZap, Plus, Power, SquareTerminal, Trash2, X, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { BusyIcon, Details, InlineError, Skeleton, StatusPill, toast, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { parseAppServerLink, parseNodeSettings, type AppServer, type AppServerLink, type AppServerMode, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderRole, type ProviderStatus } from "@vibeface/client/lib/node.ts"

export const providerNames: Record<ProviderId, string> = { codex: "Codex", cursor: "Cursor" }
const icons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2 }
const roleLabels: Record<ProviderRole, string> = { agent: "助手", execution: "执行任务" }
const states: Record<ProviderStatus["state"], { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: "可用", tone: "success", icon: CircleCheck },
  needs_setup: { label: "需设置", tone: "warning", icon: CircleAlert },
  starting: { label: "启动中", tone: "neutral", icon: LoaderCircle },
  unused: { label: "未使用", tone: "neutral", icon: CircleDashed },
  off: { label: "已关闭", tone: "neutral", icon: Power },
}

export function issueText(provider: ProviderId, issue: ProviderIssue): string {
  const name = providerNames[provider]
  switch (issue.code) {
    case "bin_missing": return `找不到 ${name} 程序`
    case "env_missing": return `缺少 ${issue.name}`
    case "launch_failed": return `${name} 没能启动`
    case "config_invalid": return `${name} 的配置文件读不了`
    case "app_server_not_found": return "没找到这台电脑上运行的 Codex"
    case "app_server_unreachable": return "连不上这台电脑上的 Codex"
  }
}

const modes: [AppServerMode, string, LucideIcon][] = [["system", "用已在运行的", Monitor], ["spawn", "自己启动", Play]]
const spawnServer = { mode: "spawn", socket: null } as const

/** One line for an App Server state, from the device or a fresh test. */
function LinkLine({ link, prefix = "" }: { link: AppServerLink; prefix?: string }) {
  const [tone, Icon, label] = {
    connected: ["text-success", CircleCheck, "已连接"],
    starting: ["text-muted-foreground", LoaderCircle, "连接中"],
    off: ["text-muted-foreground", Power, "未用于执行任务"],
    not_found: ["text-warning", CircleAlert, "未找到"],
    handshake_failed: ["text-warning", CircleAlert, "连不上"],
    launch_failed: ["text-warning", CircleAlert, "没能启动"],
  }[link.state] as [string, LucideIcon, string]
  return <span role="status" className={cn("flex min-w-0 items-center gap-1.5 text-meta", tone)}>
    <Icon aria-hidden="true" className={cn("size-4 shrink-0", link.state === "starting" && "animate-spin")} />
    <span className="min-w-0 truncate">{prefix}{label}{link.state === "connected" && link.user_agent ? ` · ${link.user_agent}` : ""}</span>
  </span>
}

/** Where Codex work runs: the App Server already running on this computer, or one the device starts. */
function AppServerSettings({ server, working, busy, save, test }: {
  server: AppServer; working: boolean; busy: string
  save(key: string, change: Omit<Change, "provider">): Promise<boolean>; test(): Promise<AppServerLink | null>
}) {
  const [socket, setSocket] = useState(server.socket ?? "")
  const [tested, setTested] = useState<AppServerLink | null>(null)
  const [testing, setTesting] = useState(false)
  useEffect(() => setSocket(server.socket ?? ""), [server.socket])
  useEffect(() => setTested(null), [server.mode, server.socket])
  const link = server.link
  const shown = tested ?? link
  const inUse = shown.state === "connected" || shown.state === "handshake_failed" ? [shown.socket] : shown.state === "not_found" ? shown.candidates : []
  const setMode = (mode: AppServerMode) => { if (mode !== server.mode) void save("app_server", { app_server: { mode, socket: server.socket } }) }
  const runTest = async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }

  return <div className="grid gap-2">
    <p className="text-meta font-medium text-muted-foreground">连接方式</p>
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <div role="radiogroup" aria-label="连接方式" className="flex w-fit gap-1 rounded-lg bg-muted p-1">
        {modes.map(([mode, text, Icon]) => <Button key={mode} type="button" role="radio" aria-checked={server.mode === mode} disabled={working}
          variant={server.mode === mode ? "outline" : "ghost"} className={cn(server.mode === mode && "shadow-xs")} onClick={() => setMode(mode)}>
          <Icon aria-hidden="true" />{text}
        </Button>)}
      </div>
      <Button type="button" variant="ghost" disabled={working || testing || link.state === "off"} onClick={() => void runTest()}>
        <BusyIcon busy={testing} icon={PlugZap} />测试连接
      </Button>
    </div>
    <LinkLine link={shown} prefix={tested ? "测试：" : ""} />
    {shown.state === "handshake_failed" || shown.state === "launch_failed" ? <code className="font-mono text-meta break-all text-muted-foreground">{shown.reason}</code> : null}
    {server.mode === "system" ? <Details label="手动指定">
      {inUse.map((path) => <code key={path} className="font-mono text-foreground">{path}</code>)}
      <form className="flex min-w-0 gap-2" onSubmit={(event) => { event.preventDefault(); void save("socket", { app_server: { mode: "system", socket: socket.trim() || null } }) }}>
        <Input aria-label="指定连接路径" className="min-w-0 font-mono" placeholder="自动查找" spellCheck={false} value={socket} disabled={working} onChange={(event) => setSocket(event.target.value)} />
        {socket.trim() !== (server.socket ?? "") ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy.endsWith(":socket")} />保存</Button> : null}
      </form>
    </Details> : null}
  </div>
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

type Change = { provider: ProviderId; enabled?: boolean; roles?: ProviderRole[]; bin?: string; env?: Record<string, string | null>; app_server?: { mode: AppServerMode; socket: string | null } }

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
  async function test(): Promise<AppServerLink | null> {
    if (!ready?.isCurrent()) return null
    try {
      const link = parseAppServerLink(await ready.request("test_codex_app_server"))
      void call("load", null)
      return link
    } catch (cause) {
      setError({ provider: "codex", cause })
      return null
    }
  }
  useEffect(() => { if (update) setResponse(update) }, [update])
  const starting = response?.providers.some((item) => item.state === "starting")
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void call("load", null), 2000)
    return () => clearTimeout(timer)
  }, [response])

  if (!response) return error ? <InlineError error={error.cause} action={{ label: "重试", onClick: () => void call("load", null) }} /> : <div className="grid gap-2"><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
  return <ul className="grid gap-3">
    {response.providers.map((item) => <ProviderCard key={item.provider} item={item} busy={busy} error={error?.provider === item.provider ? error.cause : null} save={(key, change) => call(`${item.provider}:${key}`, { provider: item.provider, ...change })} test={test} />)}
  </ul>
}

function ProviderCard({ item, busy, error, save, test }: { item: ProviderStatus; busy: string; error: unknown; save(key: string, change: Omit<Change, "provider">): Promise<boolean>; test(): Promise<AppServerLink | null> }) {
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
    .then((ok) => { if (ok) toast(next === null ? `已删除 ${name}` : `已保存 ${name}`, { tone: "success" }); return ok })
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
      {issue.code === "app_server_not_found" && item.app_server ? <button type="button" className="font-medium text-foreground underline underline-offset-4" disabled={working}
        onClick={() => void save("app_server", { app_server: { ...spawnServer, socket: item.app_server?.socket ?? null } })}>改为自己启动</button>
        : <button type="button" className="font-medium text-foreground underline underline-offset-4" onClick={fix}>去设置</button>}
    </p> : null}
    <details id={`provider-${item.provider}`} className="details border-t">
      <summary className="flex min-h-11 items-center gap-1 px-3 text-meta text-muted-foreground hover:text-foreground">
        <ChevronRight aria-hidden="true" className="size-4 transition-transform" />设置
      </summary>
      <div className="grid gap-4 px-3 pb-3">
        {item.app_server ? <AppServerSettings server={item.app_server} working={working || !item.enabled} busy={busy} save={save} test={test} /> : null}
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
            const missing = item.state === "needs_setup" && !row.set && !row.inherited && row.required
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
          <p className="text-meta text-muted-foreground">只存在这台电脑上，保存后看不到原值</p>
          {item.app_server?.mode === "system" ? <p className="text-meta text-muted-foreground">用已在运行的 Codex 时，这些变量不生效</p> : null}
        </div>
        {issue?.code === "launch_failed" ? <Details label="启动失败原因"><code className="font-mono">{issue.detail}</code></Details> : null}
      </div>
    </details>
    {error ? <InlineError error={error} className="px-3 pb-3" /> : null}
  </li>
}
