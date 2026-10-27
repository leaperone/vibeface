import { useEffect, useRef, useState, type ChangeEvent, type ComponentProps, type ReactNode } from "react"
import { ChevronRight, CircleAlert, CircleCheck, CircleDashed, CircleX, Download, ExternalLink, FolderOpen, LoaderCircle, Lock, Monitor, MousePointer2, Pencil, Play, PlugZap, Plus, Power, RotateCw, SquareTerminal, Trash2, Unplug, X, type LucideIcon } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, Details, InlineError, Segmented, Skeleton, StatusPill, toast, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { deviceCopy } from "@vibeface/client/identity/copy.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { terms } from "@/lib/glossary"
import { parseAppServerLink, parseNodeSettings, providerNames, type AppServer, type AppServerLink, type AppServerMode, type ManagerSettings, type NodeSettings, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderStatus } from "@vibeface/client/lib/node.ts"

export const docs = "https://github.com/leaperone/vibeface/blob/main/docs/"
export const providerIcons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2 }
const states: Record<ProviderStatus["state"], { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: "可用", tone: "success", icon: CircleCheck },
  needs_setup: { label: "需设置", tone: "warning", icon: CircleAlert },
  starting: { label: "启动中", tone: "neutral", icon: LoaderCircle },
  off: { label: "未使用", tone: "neutral", icon: CircleDashed },
}
/** Where each provider's program comes from, and how the device uses it. */
const install: Record<ProviderId, string> = { codex: "https://github.com/openai/codex", cursor: docs + "node.md#使用-cursor-sdk" }
const guide: Record<ProviderId, string> = { codex: docs + "node.md#使用-codex-sdk", cursor: docs + "node.md#使用-cursor-sdk" }
// Names come from runtime/settings.rs readiness.missing.
const checks: Record<ProviderId, { key: string; label: string }[]> = {
  codex: [{ key: "Codex CLI", label: "缺少 Codex 命令行" }],
  cursor: [{ key: "Cursor SDK Bridge", label: "缺少 Cursor 组件" }],
}
const services: Record<string, string> = { CURSOR_API_KEY: "Cursor", OPENAI_API_KEY: "OpenAI", ANTHROPIC_API_KEY: "Anthropic", GEMINI_API_KEY: "Gemini" }
/** A variable the provider needs, named after its service where there is one; the variable itself goes in a tooltip. */
const keyName = (name: string) => services[name] ? `${services[name]} ${terms.secret.label}` : name

function issueText(provider: ProviderId, issue: ProviderIssue): string {
  const name = providerNames[provider]
  switch (issue.code) {
    case "bin_missing": return `找不到 ${name} 程序`
    case "env_missing": return `缺少 ${keyName(issue.name)}`
    case "launch_failed": return `${name} 没能启动`
    case "config_invalid": return `${name} 的配置文件读不了`
    case "app_server_not_found": return "没找到这台电脑上运行的 Codex"
    case "app_server_unreachable": return "连不上这台电脑上的 Codex"
  }
}

const modes: { value: AppServerMode; label: string; icon: LucideIcon }[] = [{ value: "system", label: "用已在运行的", icon: Monitor }, { value: "spawn", label: "由 Vibeface 启动", icon: Play }]
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
    <span className="min-w-0 truncate" title={link.state === "connected" ? link.user_agent || undefined : undefined}>{prefix}{label}</span>
  </span>
}

/** How Settings shows paths under the home folder, from `display_paths`. */
type Display = Record<string, string>

/** A path input that reads `~/…` while it rests on the saved path and shows the full path to edit. */
function PathInput({ saved, display, value, className, ...props }: ComponentProps<"input"> & { saved: string; display: Display; value: string }) {
  const [editing, setEditing] = useState(false)
  const short = !editing && value === saved ? display[saved] : undefined
  return <Input {...props} className={cn("min-w-0 font-mono text-foreground placeholder:font-sans", className)} spellCheck={false} title={short ? saved : undefined} value={short ?? value}
    onFocus={() => setEditing(true)} onBlur={() => setEditing(false)} />
}

/** A path the device uses, shown as it is saved; editing it shows 保存. An empty path lets the device look again. */
function PathField({ id, label, value, display, placeholder, working, busy, onSave }: {
  id: string; label: string; value: string | null; display: Display; placeholder: string; working: boolean; busy: boolean; onSave(path: string): unknown
}) {
  const [draft, setDraft] = useState(value ?? "")
  useEffect(() => setDraft(value ?? ""), [value])
  const dirty = draft.trim() !== (value ?? "")
  return <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); if (dirty) void onSave(draft.trim()) }}>
    <label htmlFor={id} className="text-meta font-medium text-muted-foreground">{label}</label>
    <div className="flex min-w-0 gap-2">
      <PathInput id={id} saved={value ?? ""} display={display} placeholder={placeholder} maxLength={4096} value={draft} disabled={working} onChange={(event) => setDraft(event.target.value)} />
      {dirty ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy} />保存</Button> : null}
    </div>
  </form>
}

const placeholders = { home: "例如 ~/.codex", socket: "例如 ~/.codex/app-server-control/app-server-control.sock" }

/**
 * Where Codex work runs: the App Server already running on this computer, at its socket, or one the device starts with the
 * Codex folder. The device fills both in from this computer when they are missing; the paths stay folded until one is missing or broken.
 * `children`: other paths the provider uses, folded with these.
 */
function AppServerSettings({ server, home, sessions, display, working, blocked, busy, save, test, children }: {
  server: AppServer; home: string | null; sessions: string | null; display: Display; working: boolean; blocked: boolean; busy: string
  save(key: string, change: Omit<Change, "provider">): Promise<boolean>; test(): Promise<AppServerLink | null>; children?: ReactNode
}) {
  const [tested, setTested] = useState<AppServerLink | null>(null)
  const [testing, setTesting] = useState(false)
  useEffect(() => setTested(null), [server.mode, server.socket])
  const link = server.link
  const shown = tested ?? link
  const setMode = (mode: AppServerMode) => void save("app_server", { app_server: { mode, socket: server.socket } })
  // Opens by itself when a path needs attention and stays open after it is fixed, so a saved field does not fold away under the cursor.
  const needsPaths = blocked || (server.mode === "system" ? !server.socket : !home) || ["not_found", "handshake_failed", "launch_failed"].includes(link.state)
  const [pathsOpen, setPathsOpen] = useState(needsPaths)
  useEffect(() => { if (needsPaths) setPathsOpen(true) }, [needsPaths])
  const runTest = async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }

  return <div className="grid gap-3">
    <div className="grid gap-2">
      <p className="text-meta font-medium text-muted-foreground">连接方式</p>
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <Segmented label="连接方式" labels fill value={server.mode} options={modes} disabled={working} onChange={setMode} />
        <Button type="button" variant="ghost" disabled={working || testing || blocked || link.state === "off"} onClick={() => void runTest()}>
          <BusyIcon busy={testing} icon={PlugZap} />测试连接
        </Button>
      </div>
      {blocked ? null : <LinkLine link={shown} prefix={tested ? "测试：" : ""} />}
      {shown.state === "handshake_failed" || shown.state === "launch_failed" ? <Details label="原因"><code className="font-mono break-all">{shown.reason}</code></Details> : null}
    </div>
    <Details label="路径" open={pathsOpen || needsPaths || undefined}>
      {server.mode === "system"
        ? <PathField id="codex-socket" label="连接路径" value={server.socket} display={display} placeholder={placeholders.socket} working={working} busy={busy.endsWith(":socket")}
            onSave={(path) => save("socket", { app_server: { mode: "system", socket: path || null } })} />
        : <PathField id="codex-home" label="Codex 目录" value={home} display={display} placeholder={placeholders.home} working={working} busy={busy.endsWith(":home")}
            onSave={(path) => save("home", { env: { CODEX_HOME: path || null } })} />}
      {sessions ? <p className="grid gap-1"><span className="font-medium">会话目录</span><span className="flex min-w-0 text-foreground"><FolderPath path={sessions} shown={display[sessions]} /></span></p> : null}
      {children}
    </Details>
  </div>
}

/**
 * When the device found no Codex on this computer: the folder or the running App Server's socket, typed in, turns on what was asked in one write.
 * `onboarding`: only the folder; the socket stays on the device page.
 */
function ManualCodex({ onboarding, working, busy, save }: { onboarding: boolean; working: boolean; busy: string; save(key: string, change: Omit<Change, "provider">): Promise<boolean> }) {
  return <Details label="手动填写">
    <div className="grid gap-3">
      <PathField id="manual-codex-home" label="Codex 目录" value={null} display={{}} placeholder={placeholders.home} working={working} busy={busy.endsWith(":home")}
        onSave={(path) => save("home", { app_server: { mode: "spawn", socket: null }, env: { CODEX_HOME: path } })} />
      {onboarding ? null : <PathField id="manual-codex-socket" label="连接路径" value={null} display={{}} placeholder={placeholders.socket} working={working} busy={busy.endsWith(":socket")}
        onSave={(path) => save("socket", { app_server: { mode: "system", socket: path } })} />}
    </div>
  </Details>
}

/** Opens a provider's settings on this page and moves focus into them; every fix action lands here. */
function openProvider(provider: ProviderId) {
  const card = document.getElementById(`provider-${provider}`)
  if (!card) return
  if (card instanceof HTMLDetailsElement) card.open = true
  card.scrollIntoView({ block: "center", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" })
  const target = card.querySelector<HTMLElement>("[data-fix]") ?? card.querySelector<HTMLElement>("input")
  for (let fold = target?.closest("details"); fold && fold !== card; fold = fold.parentElement?.closest("details")) fold.open = true
  target?.focus({ preventScroll: true })
}

type Change = { provider: ProviderId; bot?: boolean; execution?: boolean; bin?: string; env?: Record<string, string | null>; app_server?: { mode: AppServerMode; socket: string | null } }
type Failure = { at: ProviderId | "roots" | null; cause: unknown }

/**
 * The AI on the device this browser is connected to: which provider runs the bot, whether
 * Codex executes tasks, each provider's program and variables, and the folders it may use. Every
 * change saves right away. Variable values are write-only; the device only says which are set.
 */
export function AiSettings({ onboarding = false, onBot }: { onboarding?: boolean; onBot?(on: boolean): void }) {
  const { ready } = useDevice()
  const workspace = useWorkspace()
  const [response, setResponse] = useState<NodeSettingsResponse | null>(null)
  const [busy, setBusy] = useState("")
  const [error, setError] = useState<Failure | null>(null)
  const sequence = useRef(0)

  async function call(key: string, at: Failure["at"], method: string, params?: Change | NodeSettings) {
    if (!ready?.isCurrent()) return false
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const parsed = parseNodeSettings(await connection.request(method, params))
      if (owner !== sequence.current || !connection.isCurrent()) return false
      setResponse(parsed)
      return true
    } catch (cause) {
      if (owner === sequence.current && connection.isCurrent()) setError({ at, cause })
      return false
    } finally {
      if (owner === sequence.current) setBusy("")
    }
  }
  const load = () => call("load", null, "node_settings")
  /** After a write that failed, possibly after the device applied it: read the state again instead of resending; the error stays shown. */
  async function reconcile() {
    if (!ready?.isCurrent()) return
    const connection = ready, owner = sequence.current
    try {
      const parsed = parseNodeSettings(await connection.request("node_settings"))
      if (owner === sequence.current && connection.isCurrent()) setResponse(parsed)
    } catch { /* the shown error already says what failed */ }
  }
  useEffect(() => { setResponse(null); void load(); return () => { sequence.current += 1 } }, [ready])
  async function test(): Promise<AppServerLink | null> {
    if (!ready?.isCurrent()) return null
    try {
      const link = parseAppServerLink(await ready.request("test_codex_app_server"))
      void load()
      return link
    } catch (cause) {
      setError({ at: "codex", cause })
      return null
    }
  }
  const starting = response?.providers.some((item) => item.state === "starting")
  const botOn = response?.providers.some((item) => item.roles.includes("agent")) ?? false
  useEffect(() => { onBot?.(botOn) }, [botOn])
  // Nothing pushes a device's bot readiness; the device list, the inbox and onboarding read it from `nodes`, so read them again when it moves.
  const botState = response?.providers.find((item) => item.roles.includes("agent"))?.state
  useEffect(() => { if (botState && workspace.isCurrent()) void workspace.refresh().catch(() => {}) }, [botState])
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void load(), 2000)
    return () => clearTimeout(timer)
  }, [response])

  if (!response) return error ? <InlineError error={error.cause} action={{ label: "重试", onClick: () => void load() }} /> : <div className="grid gap-2"><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
  const { roots, manager } = response.settings
  return <div className="grid gap-6">
    <ul className="divide-y overflow-hidden rounded-xl border bg-card shadow-xs">
      {response.providers.map((item) => <ProviderCard key={item.provider} item={item} response={response} onboarding={onboarding} busy={busy}
        error={error?.at === item.provider ? error.cause : null} test={test} reconcile={reconcile}
        change={(key, value) => call(`${item.provider}:${key}`, item.provider, "configure_provider", { provider: item.provider, ...value })}
        saveBot={(next) => call(`${item.provider}:bot_details`, item.provider, "configure_node", { roots, manager: next })} />)}
    </ul>
    {onboarding ? botOn ? null : <BotHint /> : <Folders roots={roots} display={response.display_paths} busy={busy !== ""} save={(next) => void call("roots", "roots", "configure_node", { roots: next, manager })} />}
    {error && (error.at === "roots" || error.at === null) ? <InlineError error={error.cause} /> : null}
  </div>
}

const roles = {
  bot: { ...deviceCopy.roles.bot, icon: terms.bot.icon },
  execution: { ...deviceCopy.roles.execution, icon: terms.task.icon },
}

/**
 * A chip that turns one use of a provider on or off: its icon tells the two apart, a small switch says it is a control, not a status,
 * and on, the chip is also tinted so a phone without hover still reads it. A phone shows the icon and switch alone; the words stay in the name.
 * Never solid, so the page's own next step stays the only filled button.
 */
function RoleToggle({ role, name, pressed, busy, disabled, onClick }: { role: keyof typeof roles; name: string; pressed: boolean; busy: boolean; disabled: boolean; onClick(): void }) {
  const { label, hint, icon: Icon } = roles[role]
  return <Button type="button" size="sm" variant="outline" aria-pressed={pressed} aria-label={`${name} ${label}`} aria-description={hint(name)} title={hint(name)} disabled={disabled}
    className={cn("rounded-full max-sm:px-2.5", pressed ? "border-brand/30 bg-brand-soft hover:bg-brand-soft dark:border-brand/30 dark:bg-brand-soft dark:hover:bg-brand-soft" : "text-muted-foreground")} onClick={onClick}>
    {busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : <Icon aria-hidden="true" />}
    <span aria-hidden="true" className={cn("relative h-3.5 w-6 shrink-0 rounded-full transition-colors", pressed ? "bg-brand" : "bg-muted-foreground/75")}>
      <span className={cn("absolute top-0.5 left-0.5 size-2.5 rounded-full bg-background transition-transform", pressed && "translate-x-2.5")} />
    </span><span className="max-sm:sr-only">{label}</span>
  </Button>
}

function ProviderCard({ item, response, onboarding, busy, error, change, saveBot, test, reconcile }: {
  item: ProviderStatus; response: NodeSettingsResponse; onboarding: boolean; busy: string; error: unknown
  change(key: string, value: Omit<Change, "provider">): Promise<boolean>; saveBot(manager: ManagerSettings): Promise<boolean>; test(): Promise<AppServerLink | null>; reconcile(): Promise<void>
}) {
  const name = providerNames[item.provider], Icon = providerIcons[item.provider]
  const display = response.display_paths
  const bot = item.roles.includes("agent"), execution = item.roles.includes("execution")
  const executable = item.supports.includes("execution") && !onboarding
  const manager = bot ? response.settings.manager : null
  const missing = bot ? checks[item.provider].filter((check) => response.readiness.missing.includes(check.key)) : []
  const state = states[item.state === "ready" && missing.length ? "needs_setup" : item.state]
  const [editing, setEditing] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<string | null>(null)
  const [value, setValue] = useState("")
  const [newName, setNewName] = useState("")
  const [newValue, setNewValue] = useState("")
  const [bin, setBin] = useState(item.bin ?? "")
  useEffect(() => setBin(item.bin ?? ""), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  const issue = item.state === "needs_setup" ? item.issues[0] : undefined
  const server = item.app_server
  // The use last asked for: when the device found no Codex folder, the folder typed in turns it on in the same write.
  const [asked, setAsked] = useState<"bot" | "execution" | null>(null)
  const notFound = (error as { code?: unknown } | null)?.code === "codex_missing"
  const turnOn = (role: "bot" | "execution") => { setAsked(role); void change(role, { [role]: true }).then((ok) => { if (!ok) void reconcile() }) }
  const saveKey = (name: string, value: string) => change(`env:${name}`, { env: { [name]: value } })
    .then((ok) => { if (ok) toast(`${keyName(name)} 已保存`, { tone: "success" }); return ok })
  const manual = (key: string, value: Omit<Change, "provider">) => change(key, { ...value, ...(asked ? { [asked]: true } : {}) })
    .then((ok) => { if (!ok) void reconcile(); return ok })
  const binForm = item.bin !== null ? <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); void change("bin", { bin: bin.trim() }) }}>
      <label htmlFor={`${item.provider}-bin`} className="text-meta font-medium text-muted-foreground">程序</label>
      <div className="flex min-w-0 gap-2">
        <PathInput id={`${item.provider}-bin`} saved={item.bin ?? ""} display={display} placeholder="codex" value={bin} disabled={working}
          data-fix={issue?.code === "bin_missing" ? "" : undefined} onChange={(event) => setBin(event.target.value)} />
        {bin.trim() !== (item.bin ?? "") ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy === `${item.provider}:bin`} />保存</Button> : null}
      </div>
    </form> : null
  const serverSettings = server ? <AppServerSettings server={server} home={item.home} sessions={item.sessions} display={display} working={working} blocked={issue?.code === "bin_missing"} busy={busy} save={change} test={test}>{binForm}</AppServerSettings> : binForm
  const setVariable = (key: string, name: string, next: string | null) => change(key, { env: { [name]: next } })
    .then((ok) => { if (ok) toast(next === null ? `已删除「${name}」` : `已保存「${name}」`, { tone: "success" }); return ok })
  const addName = newName.trim().toUpperCase()
  // The Codex folder shows with the App Server above.
  const env = item.env.filter((row) => row.name !== "CODEX_HOME")

  return <li>
    <div className="flex min-h-14 flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2">
      <div className="flex items-center gap-3">
        <Icon aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
        <span className="font-medium whitespace-nowrap">{name}</span>
        {item.state === "off" ? null : <StatusPill tone={state.tone} icon={state.icon} className={cn(item.state === "starting" && "[&>svg]:animate-spin")}>{state.label}</StatusPill>}
      </div>
      <div className="ms-auto flex shrink-0 gap-2">
        <RoleToggle role="bot" name={name} pressed={bot} busy={busy === `${item.provider}:bot`} disabled={busy !== ""}
          onClick={() => bot ? void change("bot", { bot: false }) : turnOn("bot")} />
        {executable ? <RoleToggle role="execution" name={name} pressed={execution} busy={busy === `${item.provider}:execution`} disabled={busy !== ""}
          onClick={() => execution ? void change("execution", { execution: false }) : turnOn("execution")} /> : null}
      </div>
    </div>
    {notFound ? <div className="grid gap-2 px-4 sm:ps-12 pb-2 text-meta">
      <span role="status" className="flex min-w-0 items-center gap-1.5 text-warning"><Unplug aria-hidden="true" className="size-4 shrink-0" />没找到这台电脑上的 Codex</span>
      <div className="flex flex-wrap items-center gap-2">
        <Button asChild variant="outline" size="sm"><a href={install.codex} target="_blank" rel="noopener noreferrer"><Download aria-hidden="true" />怎么装</a></Button>
        {asked ? <Button type="button" variant="ghost" size="sm" disabled={working} onClick={() => turnOn(asked)}><BusyIcon busy={busy === `${item.provider}:${asked}`} icon={RotateCw} />再找一次</Button> : null}
      </div>
      <ManualCodex onboarding={onboarding} working={working} busy={busy} save={manual} />
    </div> : issue?.code === "env_missing" ? <KeyField key={issue.name} name={issue.name} working={working} busy={busy === `${item.provider}:env:${issue.name}`} more={item.issues.length - 1} save={saveKey} />
    : issue ? <p className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 px-4 sm:ps-12 pb-2 text-meta text-warning">
      <CircleAlert aria-hidden="true" className="size-4 shrink-0" />
      <span className="min-w-0">{issueText(item.provider, issue)}{item.issues.length > 1 ? `，还有 ${item.issues.length - 1} 项` : ""}</span>
      {issue.code === "app_server_not_found" && server ? <button type="button" className="hit-area relative font-medium text-foreground underline underline-offset-4" disabled={working}
          onClick={() => void change("app_server", { app_server: spawnServer })}>改由 Vibeface 启动</button>
        : onboarding || issue.code === "bin_missing" ? <a className="hit-area relative font-medium text-foreground underline underline-offset-4" href={issue.code === "bin_missing" ? install[item.provider] : guide[item.provider]} target="_blank" rel="noopener noreferrer">{issue.code === "bin_missing" ? "怎么装" : "怎么设置"}</a>
        : <button type="button" className="hit-area relative font-medium text-foreground underline underline-offset-4" onClick={() => openProvider(item.provider)}>去设置</button>}
    </p> : missing.length ? <ul className="grid gap-1.5 px-4 sm:ps-12 pb-2">{missing.map((check) => <li key={check.key} className="flex min-w-0 items-center gap-2 text-meta">
      <CircleX aria-hidden="true" className="size-4 shrink-0 text-danger" />
      <span className="min-w-0 truncate">{check.label}</span>
      <a className="hit-area relative shrink-0 font-medium underline underline-offset-4" href={install[item.provider]} target="_blank" rel="noopener noreferrer">怎么装</a>
    </li>)}</ul> : null}
    {onboarding ? null : <details id={`provider-${item.provider}`} className="details">
      <summary className="-mt-2 flex min-h-10 w-fit items-center gap-1 px-4 sm:ps-12 pe-4 text-meta text-muted-foreground hover:text-foreground">
        <ChevronRight aria-hidden="true" className="size-4 transition-transform" />设置
      </summary>
      <div className="grid gap-5 px-4 sm:ps-12 pt-1 pb-4">
        {serverSettings}

        {manager ? <BotDetails key={JSON.stringify(manager)} manager={manager} display={display} fixBin={issue?.code === "bin_missing"} working={working}
          busy={busy === `${item.provider}:bot_details`} save={saveBot} /> : null}

        <div className="grid gap-2">
          <p className="text-meta font-medium text-muted-foreground">环境变量</p>
          {env.length ? <ul className="grid gap-1">{env.map((row) => {
            const missing = item.state === "needs_setup" && !row.set && !row.inherited && row.required
            return <li key={row.name} className="grid gap-2 rounded-lg bg-muted/60 py-1 ps-3 pe-1">
              <div className="flex min-h-8 min-w-0 flex-wrap items-center gap-x-2">
                {deleting === row.name && row.set ? <span className="order-first basis-full pt-2 text-meta text-foreground">删除后 Bot 用不到它</span> : null}
                <span className="min-w-0 flex-1 truncate font-mono text-meta" title={row.name}>{row.name}</span>
                <span className={cn("shrink-0 text-meta", missing ? "text-warning" : "text-muted-foreground")}>{row.set ? "••••••" : row.inherited ? "沿用本机" : "未填"}</span>
                {deleting === row.name && row.set ? null : <Button type="button" variant="ghost" size="icon" aria-label={`${row.set ? "替换" : "填写"} ${row.name}`} title={row.set ? "替换" : "填写"} disabled={working}
                  data-fix={missing ? "" : undefined} onClick={() => { setEditing(row.name); setValue("") }}><Pencil aria-hidden="true" /></Button>}
                {!row.set ? null : deleting === row.name ? <>
                  <Button type="button" size="sm" variant="ghost" autoFocus onClick={() => setDeleting(null)}>取消</Button>
                  <Button type="button" size="sm" variant="destructive" disabled={working} onClick={() => { setDeleting(null); void setVariable(`env:${row.name}`, row.name, null) }}>删除</Button>
                </> : <Button type="button" variant="ghost" size="icon" aria-label={`删除 ${row.name}`} title="删除" disabled={working}
                  onClick={() => setDeleting(row.name)}><Trash2 aria-hidden="true" /></Button>}
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
    </details>}
    {error && !notFound ? <InlineError error={error} className="px-4 sm:ps-12 pb-3" /> : null}
  </li>
}

/**
 * The key a provider still needs, pasted in where it is asked for. It goes only to this device's own variables file,
 * like 环境变量 in Settings; the field is cleared once saved and the value never comes back.
 */
function KeyField({ name, working, busy, more, save }: { name: string; working: boolean; busy: boolean; more: number; save(name: string, value: string): Promise<boolean> }) {
  const [value, setValue] = useState("")
  const label = keyName(name)
  return <form className="grid gap-2 px-4 sm:ps-12 pb-3" onSubmit={(event) => {
    event.preventDefault()
    if (value) void save(name, value).then((ok) => { if (ok) setValue("") })
  }}>
    <label htmlFor={`key-${name}`} title={name} className="flex min-w-0 items-center gap-1.5 text-meta text-warning">
      <terms.secret.icon aria-hidden="true" className="size-4 shrink-0" />缺少 {label}{more > 0 ? `，还有 ${more} 项` : ""}
    </label>
    <div className="flex min-w-0 gap-2">
      <Input id={`key-${name}`} type="password" autoComplete="new-password" data-1p-ignore="" spellCheck={false} className="min-w-0" placeholder={`粘贴 ${label}`} maxLength={4096}
        value={value} disabled={working} onChange={(event) => setValue(event.target.value)} />
      <Button type="submit" variant="outline" disabled={working || !value}><BusyIcon busy={busy} />保存</Button>
    </div>
    <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><Lock aria-hidden="true" className="size-4 shrink-0" />只存在这台电脑上</p>
    {name === "CURSOR_API_KEY" ? <a className="hit-area relative inline-flex w-fit items-center gap-1 text-meta text-muted-foreground underline-offset-4 hover:text-foreground hover:underline" href="https://cursor.com/dashboard" target="_blank" rel="noopener noreferrer">
      获取{terms.secret.label}<ExternalLink aria-hidden="true" className="size-3.5" />
    </a> : null}
  </form>
}

/** Why setup cannot move on yet: no provider runs the bot. A bot that still needs setup can move on; its status pill says so. */
function BotHint() {
  return <p role="status" className="flex items-center gap-1.5 text-meta text-muted-foreground">
    <CircleAlert aria-hidden="true" className="size-4 shrink-0" />打开「{deviceCopy.roles.bot.label}」
  </p>
}

const botFields = ["model", "workspace", "cursor_sdk_bin"] as const

/** The bot's model, folder and programs, inside the settings of the provider it runs on. */
function BotDetails({ manager, display, fixBin, working, busy, save }: { manager: ManagerSettings; display: Display; fixBin: boolean; working: boolean; busy: boolean; save(manager: ManagerSettings): Promise<boolean> }) {
  const [draft, setDraft] = useState(manager)
  const cursor = manager.provider === "cursor_sdk"
  const dirty = botFields.some((key) => draft[key] !== manager[key])
  const field = (key: (typeof botFields)[number], label: string, extra: { placeholder?: string; required?: boolean; fix?: boolean; plain?: boolean } = {}) => {
    const props = { name: key, value: draft[key], disabled: working, placeholder: extra.placeholder, required: extra.required, "data-fix": extra.fix ? "" : undefined,
      onChange: (event: ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }) }
    return <label className="grid gap-1 text-meta">{label}{extra.plain ? <Input {...props} maxLength={256} /> : <PathInput {...props} saved={manager[key]} display={display} />}</label>
  }
  return <form className="grid gap-3" onSubmit={(event) => {
    event.preventDefault()
    void save({ ...draft, model: draft.model.trim(), workspace: draft.workspace.trim(), cursor_sdk_bin: draft.cursor_sdk_bin.trim() })
  }}>
    <p className="text-meta font-medium text-muted-foreground">{terms.bot.label}</p>
    {field("model", "模型", { placeholder: cursor ? "auto" : "默认", required: cursor, plain: true })}
    {field("workspace", "Bot 工作文件夹")}
    {cursor ? field("cursor_sdk_bin", "Cursor 组件", { fix: fixBin }) : null}
    {dirty ? <Button type="submit" variant="outline" className="w-fit" disabled={working}><BusyIcon busy={busy} />保存</Button> : null}
  </form>
}

function Folders({ roots, display, busy, save }: { roots: string[]; display: Display; busy: boolean; save(roots: string[]): void }) {
  const [folder, setFolder] = useState("")
  const add = () => { const path = folder.trim(); setFolder(""); if (path && !roots.includes(path)) save([...roots, path]) }
  return <section className="grid gap-2">
    <h3 className="px-1 text-meta font-medium text-muted-foreground">{terms.folders.label}</h3>
    <div className="grid gap-2 rounded-xl border bg-card p-3 shadow-xs">
    {roots.length ? <ul className="grid gap-1">{roots.map((root) => <li key={root} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <FolderOpen aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      <FolderPath path={root} shown={display[root]} />
      <Button type="button" variant="ghost" size="icon" aria-label={`移除 ${root}`} title="移除" disabled={busy} onClick={() => save(roots.filter((item) => item !== root))}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    <div className="flex min-w-0 gap-2">
      <Input aria-label="文件夹路径" className="min-w-0 font-mono" placeholder="~/projects" value={folder} disabled={busy} onChange={(event) => setFolder(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); add() } }} />
      <Button type="button" variant="outline" disabled={busy || !folder.trim()} onClick={add}><Plus aria-hidden="true" />添加</Button>
    </div>
    </div>
  </section>
}

/** A long path gives up its parent folders first, so the folder's own name stays readable. `shown`: its `~/…` form. */
function FolderPath({ path, shown = path }: { path: string; shown?: string }) {
  const cut = shown.replace(/[\\/]+$/, "").search(/[^\\/]*$/)
  return <span className="flex min-w-0 flex-1 font-mono text-meta" title={path}>
    <span className="min-w-0 truncate text-muted-foreground">{shown.slice(0, cut)}</span><span className="max-w-full shrink-0 truncate">{shown.slice(cut)}</span>
  </span>
}
