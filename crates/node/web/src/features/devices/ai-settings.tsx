import { useEffect, useRef, useState, type ChangeEvent, type ComponentProps, type ReactNode } from "react"
import { Asterisk, Braces, ChevronRight, CircleAlert, CircleCheck, CircleDashed, Copy, Download, ExternalLink, FolderOpen, FolderPlus, List, LoaderCircle, Lock, Monitor, MousePointer2, Pencil, Pi, Play, PlugZap, Plus, Power, RotateCw, SquareTerminal, Trash2, Unplug, X, type LucideIcon } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, Details, FolderPath, InlineError, Segmented, Skeleton, toast, toneText, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { deviceCopy } from "@vibeface/client/identity/copy.ts"
import { folderLabels } from "@vibeface/client/workspace/bot-settings.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { terms } from "@/lib/glossary"
import { availability, summaryReadiness, botOnChange, folderSuggestions, type Availability, modelTyped, parseAppServerLink, parseNodeSettings, parseProvider, probeIssueText, effortOptions, type ModelChoice, providerGroups, providerNames, providerIds, type BackupPolicy, type AppServer, type AppServerLink, type AppServerMode, type ManagerSettings, type NodeSettings, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderStatus } from "@vibeface/client/lib/node.ts"

export const docs = "https://github.com/leaperone/vibeface/blob/main/docs/"
export const providerIcons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2, claude: Asterisk, pi: Pi, opencode: Braces }
const states: Record<Availability, { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: "可用", tone: "success", icon: CircleCheck },
  unverified: { label: "尚未确认", tone: "neutral", icon: CircleDashed },
  needs_setup: { label: "需设置", tone: "warning", icon: CircleAlert },
  starting: { label: "启动中", tone: "neutral", icon: LoaderCircle },
  missing: { label: "未安装", tone: "neutral", icon: CircleDashed },
}
/** Where each provider's program comes from, and how the device uses it. */
const install: Record<ProviderId, string> = {
  codex: "https://github.com/openai/codex", cursor: docs + "node.md#使用-cursor-sdk", claude: "https://docs.anthropic.com/en/docs/claude-code",
  pi: "https://www.npmjs.com/package/@earendil-works/pi-coding-agent", opencode: "https://opencode.ai",
}
const agents = docs + "node.md#使用-claude-codepi-或-opencode"
const guide: Record<ProviderId, string> = { codex: docs + "node.md#使用-codex-sdk", cursor: docs + "node.md#使用-cursor-sdk", claude: agents, pi: agents, opencode: agents }
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

const modes: { value: AppServerMode; label: string; icon: LucideIcon }[] = [{ value: "system", label: deviceCopy.appServer.system, icon: Monitor }, { value: "spawn", label: deviceCopy.appServer.spawn, icon: Play }]

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

const placeholders = { socket: "App Server socket 的绝对路径" }

function AppServerSettings({ server, home, sessions, display, working, blocked, busy, save, test, children }: {
  server: AppServer; home: string | null; sessions: string | null; display: Display; working: boolean; blocked: boolean; busy: string
  save(key: string, change: Omit<Change, "provider">): Promise<boolean>; test(): Promise<AppServerLink | null>; children?: ReactNode
}) {
  const [draft, setDraft] = useState({ mode: server.mode, socket: server.socket ?? "" })
  const [tested, setTested] = useState<AppServerLink | null>(null)
  const [testing, setTesting] = useState(false)
  useEffect(() => setDraft({ mode: server.mode, socket: server.socket ?? "" }), [server.mode, server.socket])
  useEffect(() => setTested(null), [server.mode, server.socket])
  const dirty = draft.mode !== server.mode || draft.socket.trim() !== (server.socket ?? "")
  const link = server.link
  const shown = tested ?? link
  // Opens by itself when a path needs attention and stays open after it is fixed, so a saved field does not fold away under the cursor.
  const needsPaths = blocked || (draft.mode === "spawn" && !home) || ["not_found", "handshake_failed", "launch_failed"].includes(link.state)
  const [pathsOpen, setPathsOpen] = useState(needsPaths)
  useEffect(() => { if (needsPaths) setPathsOpen(true) }, [needsPaths])
  const runTest = async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }

  return <div className="grid gap-3">
    <form className="grid gap-2" onSubmit={(event) => {
      event.preventDefault()
      if (dirty) void save("app_server", { app_server: { mode: draft.mode, socket: draft.socket.trim() || null } })
    }}>
      <p className="text-meta font-medium text-muted-foreground">连接方式</p>
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <Segmented label="连接方式" labels fill value={draft.mode} options={modes} disabled={working} onChange={(mode) => setDraft({ ...draft, mode })} />
        {dirty ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy.endsWith(":app_server")} />保存</Button> : null}
        <Button type="button" variant="ghost" disabled={working || testing || dirty || blocked || link.state === "off"} onClick={() => void runTest()}>
          <BusyIcon busy={testing} icon={PlugZap} />测试连接
        </Button>
      </div>
      {draft.mode === "system" ? <div className="grid gap-1">
        <label htmlFor="codex-socket" className="text-meta font-medium text-muted-foreground">连接路径</label>
        <Input id="codex-socket" name="socket" className="min-w-0 font-mono" placeholder={placeholders.socket} required pattern="/.*" maxLength={4096} spellCheck={false}
          value={draft.socket} disabled={working} onChange={(event) => setDraft({ ...draft, socket: event.target.value })} />
        <p className="text-meta text-muted-foreground">填写运行 Codex 的电脑上的绝对路径。</p>
        <p className="text-meta text-muted-foreground">共享所选外部 App Server 的会话，不隔离该服务的数据。</p>
      </div> : null}
      {dirty ? <p role="status" className="text-meta text-muted-foreground">保存后可测试连接</p>
        : blocked ? null : <LinkLine link={shown} prefix={tested ? "测试：" : ""} />}
      {!dirty && (shown.state === "handshake_failed" || shown.state === "launch_failed") ? <Details label="原因"><code className="font-mono break-all">{shown.reason}</code></Details> : null}
    </form>
    <Details label="路径" open={pathsOpen || needsPaths || undefined}>
      {draft.mode === "spawn" ? <p className="grid gap-1"><span className="font-medium">Node-owned Codex 目录</span><FolderPath path={home ?? ""} shown={home ? display[home] : "由 Node 创建"} /></p> : null}
      {sessions ? <p className="grid gap-1"><span className="font-medium">会话目录</span><span className="flex min-w-0 text-foreground"><FolderPath path={sessions} shown={display[sessions]} /></span></p> : null}
      {children}
    </Details>
  </div>
}

function ManualCodex({ onboarding, working, busy, save }: { onboarding: boolean; working: boolean; busy: string; save(key: string, change: Omit<Change, "provider">): Promise<boolean> }) {
  return <Details label="手动填写">
    <div className="grid gap-3">
      <p className="text-meta text-muted-foreground">Codex 运行目录由 Node 自动隔离管理。</p>
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

type Change = { provider: ProviderId; bot?: boolean; execution?: boolean; execution_model?: string; bin?: string; env?: Record<string, string | null>; app_server?: { mode: AppServerMode; socket: string | null } }
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
  // A provider gets installed outside this page, so coming back to the page reads the device again.
  useEffect(() => {
    const back = () => void reconcile()
    window.addEventListener("focus", back)
    return () => window.removeEventListener("focus", back)
  }, [ready])
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
  async function probe(provider: ProviderId) {
    if (!ready?.isCurrent()) return
    const connection = ready, owner = ++sequence.current
    setBusy(`${provider}:probe`); setError(null)
    try {
      const status = parseProvider(await connection.request("provider_probe", { provider }))
      if (status.provider !== provider) throw new Error("检查结果来自另一个 AI 服务，请重新读取。")
      if (owner === sequence.current && connection.isCurrent()) {
        setResponse((previous) => previous ? { ...previous, providers: previous.providers.map((item) => item.provider === provider ? status : item) } : previous)
        if (workspace.isCurrent()) await workspace.refresh()
      }
    } catch (cause) { if (owner === sequence.current && connection.isCurrent()) setError({ at: provider, cause }) }
    finally { if (owner === sequence.current) setBusy("") }
  }
  const starting = response?.providers.some((item) => item.state === "starting")
  const botOn = response?.providers.some((item) => item.roles.includes("agent")) ?? false
  useEffect(() => { onBot?.(botOn) }, [botOn])
  // Nothing pushes a device's bot readiness or folders; the device list, the inbox, onboarding and the new bot form read them from `nodes`, so read them again when they move.
  const botState = response?.providers.find((item) => item.roles.includes("agent"))?.state
  const folders = response?.settings.roots.join("\n")
  useEffect(() => { if (folders !== undefined && workspace.isCurrent()) void workspace.refresh().catch(() => {}) }, [botState, folders])
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void load(), 2000)
    return () => clearTimeout(timer)
  }, [response])

  if (!response) return error ? <InlineError error={error.cause} action={{ label: "重试", onClick: () => void load() }} /> : <div className="grid gap-2"><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
  const { roots, manager } = response.settings
  const { shown, folded } = providerGroups(response.providers)
  const card = (item: ProviderStatus) => <ProviderCard key={item.provider} item={item} response={response} onboarding={onboarding} busy={busy} models={item.probe.catalog.state === "available" ? item.probe.catalog.models : null} probe={() => probe(item.provider)}
    error={error?.at === item.provider ? error.cause : null} test={test} reconcile={reconcile}
    change={(key, value) => call(`${item.provider}:${key}`, item.provider, "configure_provider", { provider: item.provider, ...value })}
    saveBot={(next) => call(`${item.provider}:bot_details`, item.provider, "configure_node", { roots, manager: next })} />
  return <div className="grid gap-6">
    <div className="grid gap-2">
    <ul className="flex flex-wrap gap-x-4 gap-y-1 px-1 text-meta text-muted-foreground" aria-label="两种用法">
      {(["bot", "execution"] as const).map((role) => { const { label, note, icon: RoleIcon } = roles[role]; return <li key={role} className="flex items-center gap-1.5 whitespace-nowrap">
        <RoleIcon aria-hidden="true" className="size-4 shrink-0" /><span className="font-medium text-foreground">{label}</span><span aria-hidden="true">·</span>{note}
      </li> })}
    </ul>
    <ul className={cn("divide-y overflow-hidden rounded-xl border", !onboarding && "bg-card shadow-xs")}>
      {shown.map(card)}
      {folded.length ? <li><details className="details" open={!shown.length || undefined}>
        <summary className="flex min-h-11 items-center gap-1 px-4 text-meta text-muted-foreground hover:text-foreground">
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 transition-transform duration-200" />
          <span className="min-w-0 truncate">未安装 · {folded.map((item) => providerNames[item.provider]).join("、")}</span>
        </summary>
        <ul className="divide-y border-t">{folded.map(card)}</ul>
      </details></li> : null}
    </ul>
    </div>
    {onboarding && !botOn ? <BotHint /> : <Folders later={onboarding} roots={roots} suggested={folderSuggestions(response)} display={response.display_paths} busy={busy !== ""} save={(next) => void call("roots", "roots", "configure_node", { roots: next, manager })} />}
    {error && (error.at === "roots" || error.at === null) ? <InlineError error={error.cause} /> : null}
  </div>
}

const roles = {
  bot: { ...deviceCopy.roles.bot, icon: terms.bot.icon },
  execution: { ...deviceCopy.roles.execution, icon: terms.task.icon },
}

/**
 * A chip for one use of a provider: its glyph and label tell the two apart, and on, the chip is tinted so a phone without hover still reads it.
 * Only one provider runs the bot, so that chip draws a radio dot, picking it moves the bot here and picking it again turns it off; it stays a toggle
 * button for assistive tech for that reason. Executing tasks is a switch any provider can have on.
 * Never solid, so the page's own next step stays the only filled button.
 */
function RoleToggle({ role, name, pressed, busy, disabled, onClick }: { role: keyof typeof roles; name: string; pressed: boolean; busy: boolean; disabled: boolean; onClick(): void }) {
  const { label, hint } = roles[role]
  const one = role === "bot"
  return <Button type="button" size="sm" variant="outline" aria-pressed={pressed} aria-label={`${name} ${label}`} aria-description={hint(name)} title={hint(name)} disabled={disabled}
    className={cn("rounded-full", pressed ? "border-brand/30 bg-brand-soft hover:bg-brand-soft dark:border-brand/30 dark:bg-brand-soft dark:hover:bg-brand-soft" : "text-muted-foreground")} onClick={onClick}>
    {busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : one ? <span aria-hidden="true" className={cn("grid size-3.5 shrink-0 place-items-center rounded-full border-[1.5px] transition-colors", pressed ? "border-brand" : "border-muted-foreground/75")}>
      <span className={cn("size-1.5 rounded-full bg-brand transition-transform", !pressed && "scale-0")} />
    </span> : <span aria-hidden="true" className={cn("relative h-3.5 w-6 shrink-0 rounded-full transition-colors", pressed ? "bg-brand" : "bg-muted-foreground/75")}>
      <span className={cn("absolute top-0.5 left-0.5 size-2.5 rounded-full bg-background transition-transform", pressed && "translate-x-2.5")} />
    </span>}<span className="whitespace-nowrap">{label}</span>
  </Button>
}

function ProviderCard({ item, response, onboarding, busy, models, error, change, saveBot, test, reconcile, probe }: {
  item: ProviderStatus; response: NodeSettingsResponse; onboarding: boolean; busy: string; models: ModelChoice[] | null; error: unknown
  change(key: string, value: Omit<Change, "provider">): Promise<boolean>; saveBot(manager: ManagerSettings): Promise<boolean>; test(): Promise<AppServerLink | null>; reconcile(): Promise<void>; probe(): Promise<void>
}) {
  const name = providerNames[item.provider], Icon = providerIcons[item.provider]
  const display = response.display_paths
  const bot = item.roles.includes("agent"), execution = item.roles.includes("execution")
  const botable = item.supports.includes("agent"), executable = item.supports.includes("execution")
  const manager = bot ? response.settings.manager : null
  const readiness = summaryReadiness(item)
  const usable = availability(item)
  const state = states[usable]
  const executionState = states[item.execution_readiness.state]
  const [editing, setEditing] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<string | null>(null)
  const [value, setValue] = useState("")
  const [newName, setNewName] = useState("")
  const [newValue, setNewValue] = useState("")
  const [bin, setBin] = useState(item.bin)
  useEffect(() => setBin(item.bin), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  const issue = item.state === "needs_setup" ? item.issues[0] : undefined
  const server = item.app_server
  // The use last asked for: when the device found no Codex folder, the folder typed in turns it on in the same write.
  const [asked, setAsked] = useState<"bot" | "execution" | null>(null)
  const notFound = (error as { code?: unknown } | null)?.code === "codex_missing"
  const turnOn = (role: "bot" | "execution") => { setAsked(role); void change(role, role === "bot" ? botOnChange(response, item) : { execution: true }).then((ok) => { if (!ok) void reconcile() }) }
  const saveKey = (name: string, value: string) => change(`env:${name}`, { env: { [name]: value } })
    .then((ok) => { if (ok) toast(`${keyName(name)} 已保存`, { tone: "success" }); return ok })
  const manual = (key: string, value: Omit<Change, "provider">) => change(key, { ...value, ...(asked ? { [asked]: true } : {}) })
    .then((ok) => { if (!ok) void reconcile(); return ok })
  const binForm = <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); void change("bin", { bin: bin.trim() }) }}>
      <label htmlFor={`${item.provider}-bin`} className="text-meta font-medium text-muted-foreground">程序</label>
      <div className="flex min-w-0 gap-2">
        <PathInput id={`${item.provider}-bin`} saved={item.bin} display={display} placeholder={item.bin.split(/[\\/]/).pop()} value={bin} disabled={working}
          data-fix={issue?.code === "bin_missing" ? "" : undefined} onChange={(event) => setBin(event.target.value)} />
        {bin.trim() !== item.bin ? <Button type="submit" variant="outline" disabled={working}><BusyIcon busy={busy === `${item.provider}:bin`} />保存</Button> : null}
      </div>
    </form>
  // Keyed by the roles, so switching one drops a test result that no longer matches what runs.
  const serverSettings = server ? <AppServerSettings key={item.roles.join()} server={server} home={item.home} sessions={item.sessions} display={display} working={working} blocked={issue?.code === "bin_missing"} busy={busy} save={change} test={test}>{binForm}</AppServerSettings> : binForm
  const setVariable = (key: string, name: string, next: string | null) => change(key, { env: { [name]: next } })
    .then((ok) => { if (ok) toast(next === null ? `已删除「${name}」` : `已保存「${name}」`, { tone: "success" }); return ok })
  const addName = newName.trim().toUpperCase()
  // The Codex folder shows with the App Server above.
  const env = item.env.filter((row) => row.name !== "CODEX_HOME")
  // The keys a provider needs show by default; the rest of its variables wait under 高级.
  const keys = env.filter((row) => row.required), others = env.filter((row) => !row.required)
  const advancedIssue = Boolean(issue && issue.code !== "env_missing")

  // `friendly`: a key the provider needs, named after its service in the default view; any other variable shows as it is named.
  const envRow = (row: (typeof env)[number], friendly: boolean) => {
    const missing = item.state === "needs_setup" && !row.set && !row.inherited && row.required
    return <li key={row.name} className="grid gap-2 rounded-lg bg-muted/60 py-1 ps-3 pe-1">
              <div className="flex min-h-8 min-w-0 flex-wrap items-center gap-x-2">
                {deleting === row.name && row.set ? <span className="order-first basis-full pt-2 text-meta text-foreground">删除后 Bot 用不到它</span> : null}
                {friendly ? <span className="flex min-w-0 flex-1 items-center gap-1.5 truncate text-meta" title={row.name}><terms.secret.icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />{keyName(row.name)}</span>
                  : <span className="min-w-0 flex-1 truncate font-mono text-meta" title={row.name}>{row.name}</span>}
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
  }
  return <li>
    <div className="flex min-h-14 items-center gap-3 px-4 py-2">
      <Icon aria-hidden="true" className={cn("size-5 shrink-0", usable === "missing" ? "text-muted-foreground/60" : "text-muted-foreground")} />
      <div className="grid min-w-0 flex-1">
        <span className={cn("truncate font-medium", usable === "missing" && "text-muted-foreground")}>{name}</span>
        <span className={cn("flex min-w-0 items-center gap-1 text-meta", toneText[state.tone])}>
          <state.icon aria-hidden="true" className={cn("size-3.5 shrink-0", usable === "starting" && "animate-spin")} /><span className="truncate">{state.label}</span>
        </span>
      </div>
      <div className="flex shrink-0 gap-2">
        {botable ? <RoleToggle role="bot" name={name} pressed={bot} busy={busy === `${item.provider}:bot`} disabled={busy !== ""}
          onClick={() => bot ? void change("bot", { bot: false }) : turnOn("bot")} /> : null}
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
          onClick={() => void change("app_server", { app_server: { mode: "spawn", socket: server.socket } })}>{deviceCopy.appServer.useSpawn}</button>
        : onboarding || issue.code === "bin_missing" ? <a className="hit-area relative font-medium text-foreground underline underline-offset-4" href={issue.code === "bin_missing" ? install[item.provider] : guide[item.provider]} target="_blank" rel="noopener noreferrer">{issue.code === "bin_missing" ? "怎么装" : "怎么设置"}</a>
        : <button type="button" className="hit-area relative font-medium text-foreground underline underline-offset-4" onClick={() => openProvider(item.provider)}>去设置</button>}
    </p> : null}
    <div className="grid gap-1 px-4 pb-3 sm:ps-12">
      {'issue' in readiness ? <p role="status" className="text-meta text-muted-foreground">{probeIssueText(item.provider, readiness.issue)}</p> : null}
      {item.setup && (bot || execution) && ["needs_setup", "unverified"].includes(readiness.state) ? <div className="grid gap-2 text-meta">
        <p>在运行 AI 的电脑上执行以下命令，为独立目录登录或配置，再点“检查可用性”。不会复制已有登录信息。</p>
        <FolderPath path={item.setup.directory} shown={display[item.setup.directory]} />
        <div className="flex min-w-0 gap-2">
          <Input readOnly aria-label={`${name} 私有目录登录命令`} className="min-w-0 font-mono" value={item.setup.command} onFocus={(event) => event.target.select()} />
          <Button type="button" variant="outline" onClick={() => { if (item.setup) void navigator.clipboard.writeText(item.setup.command).then(() => toast("已复制登录命令", { tone: "success" }), () => toast("请选中命令后复制", { tone: "warning" })) }}><Copy aria-hidden="true" />复制命令</Button>
        </div>
      </div> : null}
      <div className="flex flex-wrap items-center gap-2 text-meta text-muted-foreground">
        <span>{item.probe.checked_at_ms === null ? "尚未主动检查" : `检查于 ${new Date(item.probe.checked_at_ms).toLocaleString()}`}</span>
        <Button type="button" variant="ghost" size="sm" disabled={busy !== ""} onClick={() => void probe()}><BusyIcon busy={busy === `${item.provider}:probe`} icon={RotateCw} />检查可用性</Button>
      </div>
      {manager && item.probe.catalog.state === "available" && item.probe.catalog.coverage === "partial" ? <p className="text-meta text-muted-foreground">模型列表可能不完整，可手填其他模型。</p> : null}
      {manager && item.probe.catalog.state === "unavailable" ? <p className="text-meta text-muted-foreground">模型列表暂不可用，可继续手填。已保存的模型保持不变。</p> : null}
    </div>
    {onboarding ? null : <details id={`provider-${item.provider}`} className="details">
      <summary className="-mt-2 flex min-h-11 w-fit items-center gap-1 px-4 sm:ps-12 text-meta text-muted-foreground hover:text-foreground">
        <ChevronRight aria-hidden="true" className="size-4 transition-transform duration-200" />设置
      </summary>
      <div className="grid gap-4 px-4 sm:ps-12 pt-1 pb-4">
        {manager ? <Details label="运行 Bot">
          <BotDetails key={`${manager.provider}:${JSON.stringify([manager.model, manager.reasoning_effort, manager.backup])}`} fields={["model"]} manager={manager} models={models} display={display} working={working}
            busy={busy === `${item.provider}:bot_details`} save={saveBot} />
        </Details> : null}
        {execution ? <Details label="执行任务">
          <p role="status" className={cn("text-meta", toneText[executionState.tone])}>执行模型：{executionState.label}</p>
          {'issue' in item.execution_readiness ? <p role="status" className="text-meta text-muted-foreground">{probeIssueText(item.provider, item.execution_readiness.issue)}</p> : null}
          <ExecutionDetails key={`${item.execution_model}:${item.provider}`} model={item.execution_model} models={models} working={working}
            busy={busy === `${item.provider}:execution_model`} save={(model) => change("execution_model", { execution_model: model })} />
        </Details> : null}
        {keys.length ? <ul className="grid gap-1" aria-label={terms.secret.label}>{keys.map((row) => envRow(row, true))}</ul> : null}

        {/* Connection, folders, program and variables are for whoever needs them; a problem in them opens the fold. */}
        <Details label="高级" open={advancedIssue || undefined}>
        <div className="grid gap-4 pt-1 text-foreground">
        {serverSettings}

        {manager ? <BotDetails key={`${manager.provider}:${manager.workspace}`} fields={["workspace"]} manager={manager} display={display} working={working}
          busy={busy === `${item.provider}:bot_details`} save={saveBot} /> : null}

        <div className="grid gap-2">
          <p className="text-meta font-medium text-muted-foreground">环境变量</p>
          {others.length ? <ul className="grid gap-1">{others.map((row) => envRow(row, false))}</ul> : null}

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
        </div>
        </Details>
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
    <CircleAlert aria-hidden="true" className="size-4 shrink-0" />选一个<terms.bot.icon aria-hidden="true" className="size-4 shrink-0" />{deviceCopy.roles.bot.label}
  </p>
}

const botFields = ["model", "workspace"] as const
/** The model option that opens the text field; no provider names a model this way. */
const other = "\u0000other"

/** The bot's model or folder, as `fields` picks, inside the settings of the provider it runs on. Keyed by its own saved fields, so saving the other one keeps its draft. */
function BotDetails({ fields, manager, models = null, display, working, busy, save }: { fields: (typeof botFields)[number][]; manager: ManagerSettings; models?: ModelChoice[] | null; display: Display; working: boolean; busy: boolean; save(manager: ManagerSettings): Promise<boolean> }) {
  const [draft, setDraft] = useState(manager)
  const copy = deviceCopy.botModel
  // A model the list does not have stays typed in; 其他 opens the same field.
  const [chosen, setTyping] = useState<boolean | null>(null)
  const typing = chosen ?? modelTyped(manager.model, models)
  const cursor = manager.provider === "cursor"
  const selected = models?.find((model) => draft.model.trim() ? model.id === draft.model.trim() : model.default)
  const reasoningEffort = selected?.effort.state === "provider_controlled" ? null : draft.reasoning_effort
  const dirty = fields.some((key) => draft[key] !== manager[key]) || fields.includes("model") && (reasoningEffort !== manager.reasoning_effort || JSON.stringify(draft.backup) !== JSON.stringify(manager.backup))
  const efforts = selected?.effort.state === "provider_controlled" ? [] : selected?.effort.state === "selectable" ? selected.effort.options : effortOptions(manager.provider)
  const field = (key: (typeof botFields)[number], label: string, extra: { placeholder?: string; required?: boolean; plain?: boolean } = {}) => {
    const props = { name: key, value: draft[key], disabled: working, placeholder: extra.placeholder, required: extra.required,
      onChange: (event: ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }) }
    return <label className="grid gap-1 text-meta">{label}{extra.plain ? <Input {...props} maxLength={256} /> : <PathInput {...props} saved={manager[key]} display={display} />}</label>
  }
  return <form className="grid gap-3" onSubmit={(event) => {
    event.preventDefault()
    void save({ ...manager, ...Object.fromEntries(fields.map((key) => [key, draft[key].trim()])),
      ...(fields.includes("model") ? { reasoning_effort: reasoningEffort, backup: { ...draft.backup, candidates: draft.backup.candidates.map((candidate) => ({ ...candidate, model: candidate.model.trim() })) } } : {}) })
  }}>
    {fields.includes("model") ? models && !typing ? <label className="grid gap-1 text-meta">{copy.label}
      <NativeSelect className="w-full" name="model" value={draft.model} disabled={working}
        onChange={(event) => { const model = event.target.value; if (model === other) { setTyping(true); setDraft({ ...draft, model: "" }) } else setDraft({ ...draft, model }) }}>
        <NativeSelectOption value="">{copy.default}</NativeSelectOption>
        {models.map((model) => <NativeSelectOption key={model.id} value={model.id}>{model.name}</NativeSelectOption>)}
        <NativeSelectOption value={other}>{copy.other}</NativeSelectOption>
      </NativeSelect>
    </label> : <div className="flex min-w-0 items-end gap-2">
      <div className="min-w-0 flex-1">{field("model", copy.label, { placeholder: cursor ? "auto" : copy.default, required: cursor, plain: true })}</div>
      {models ? <Button type="button" variant="ghost" size="icon" aria-label={copy.pick} title={copy.pick} disabled={working}
        onClick={() => { setTyping(false); setDraft({ ...draft, model: models.some((model) => model.id === draft.model) ? draft.model : "" }) }}><List aria-hidden="true" /></Button> : null}
    </div> : null}
    {fields.includes("model") ? efforts.length ? <label className="grid gap-1 text-meta">推理强度
      <NativeSelect value={reasoningEffort ?? ""} disabled={working} onChange={(event) => setDraft({ ...draft, reasoning_effort: event.target.value || null })}>
        <NativeSelectOption value="">由 AI 服务决定</NativeSelectOption>
        {reasoningEffort && !efforts.includes(reasoningEffort) ? <NativeSelectOption value={reasoningEffort}>{reasoningEffort}</NativeSelectOption> : null}
        {efforts.map((effort) => <NativeSelectOption key={effort} value={effort}>{effort}</NativeSelectOption>)}
      </NativeSelect>
      <span className="text-muted-foreground">用于下一回合，模型是否支持以 AI 服务的检查结果为准。</span>
    </label> : <p className="text-meta text-muted-foreground">推理强度由 AI 服务决定</p> : null}
    {fields.includes("model") ? <Details label="备用策略"><BackupPolicyEditor policy={draft.backup} disabled={working} change={(backup) => setDraft({ ...draft, backup })} /></Details> : null}
    {fields.includes("workspace") ? field("workspace", folderLabels.botRun) : null}
    {dirty ? <Button type="submit" variant="outline" className="w-fit" disabled={working}><BusyIcon busy={busy} />保存</Button> : null}
  </form>
}

function ExecutionDetails({ model, models, working, busy, save }: { model: string; models: ModelChoice[] | null; working: boolean; busy: boolean; save(model: string): Promise<boolean> }) {
  const [draft, setDraft] = useState(model)
  const [chosen, setTyping] = useState<boolean | null>(null)
  useEffect(() => { setDraft(model); setTyping(null) }, [model])
  const typing = chosen ?? modelTyped(model, models)
  const dirty = draft.trim() !== model
  return <form className="grid gap-2" onSubmit={(event) => { event.preventDefault(); void save(draft.trim()) }}>
    {models && !typing ? <label className="grid gap-1 text-meta">{deviceCopy.executionModel.label}
      <NativeSelect className="w-full" name="execution_model" value={draft} disabled={working}
        onChange={(event) => { const value = event.target.value; if (value === other) { setTyping(true); setDraft("") } else setDraft(value) }}>
        <NativeSelectOption value="">{deviceCopy.executionModel.default}</NativeSelectOption>
        {models.map((choice) => <NativeSelectOption key={choice.id} value={choice.id}>{choice.name}</NativeSelectOption>)}
        <NativeSelectOption value={other}>{deviceCopy.executionModel.other}</NativeSelectOption>
      </NativeSelect>
    </label> : <div className="flex min-w-0 items-end gap-2">
      <label className="grid min-w-0 flex-1 gap-1 text-meta">{deviceCopy.executionModel.label}
        <Input name="execution_model" value={draft} disabled={working} placeholder={deviceCopy.executionModel.default} onChange={(event) => setDraft(event.target.value)} />
      </label>
      {models ? <Button type="button" variant="ghost" size="icon" aria-label={deviceCopy.executionModel.pick} title={deviceCopy.executionModel.pick} disabled={working}
        onClick={() => { setTyping(false); setDraft(models.some((choice) => choice.id === draft) ? draft : "") }}><List aria-hidden="true" /></Button> : null}
    </div>}
    {dirty ? <Button type="submit" variant="outline" className="w-fit" disabled={working}><BusyIcon busy={busy} />保存</Button> : null}
  </form>
}

function Folders({ later, roots, suggested, display, busy, save }: { later: boolean; roots: string[]; suggested: { path: string; label: string }[]; display: Display; busy: boolean; save(roots: string[]): void }) {
  const [folder, setFolder] = useState("")
  const add = () => { const path = folder.trim(); setFolder(""); if (path && !roots.includes(path)) save([...roots, path]) }
  return <section className="grid gap-2">
    <div className="grid gap-0.5 px-1">
      <h3 className="text-meta font-medium text-muted-foreground">{terms.folders.label}</h3>
      <p className="text-meta text-muted-foreground">{deviceCopy.folders(later)}</p>
    </div>
    <div className="grid gap-2 rounded-xl border bg-card p-3 shadow-xs">
    {roots.length ? <ul className="grid gap-1">{roots.map((root) => <li key={root} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <FolderOpen aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      <FolderPath path={root} shown={display[root]} />
      <Button type="button" variant="ghost" size="icon" aria-label={`移除 ${root}`} title="移除" disabled={busy} onClick={() => save(roots.filter((item) => item !== root))}><X aria-hidden="true" /></Button>
    </li>)}</ul> : null}
    {suggested.length && !roots.length ? <div className="grid gap-1">{suggested.map(({ path, label }) => <Button key={path} type="button" variant="outline" className="h-auto min-h-11 justify-start border-dashed border-primary/60 bg-primary/5 hover:bg-primary/10" title={path} aria-label={`添加 ${label}`} disabled={busy} onClick={() => save([path])}>
      <FolderPlus aria-hidden="true" className="text-primary" /><FolderPath path={path} shown={label} /><span className="text-meta font-medium text-primary">添加</span>
    </Button>)}</div>
    : suggested.length ? <div className="flex flex-wrap gap-2">{suggested.map(({ path, label }) => <Button key={path} type="button" variant="outline" size="sm" className="font-mono" title={path} disabled={busy} onClick={() => save([...roots, path])}>
      <Plus aria-hidden="true" />{label}
    </Button>)}</div> : null}
    <div className="flex min-w-0 gap-2">
      <Input aria-label="文件夹路径" className="min-w-0 font-mono" placeholder={roots.length || suggested.length ? "其他文件夹" : "~/projects"} value={folder} disabled={busy} onChange={(event) => setFolder(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); add() } }} />
      <Button type="button" variant="outline" disabled={busy || !folder.trim()} onClick={add}><Plus aria-hidden="true" />添加</Button>
    </div>
    </div>
  </section>
}

function BackupPolicyEditor({ policy, disabled, change }: { policy: BackupPolicy; disabled: boolean; change(policy: BackupPolicy): void }) {
  const update = (index: number, value: BackupPolicy['candidates'][number]) => change({ ...policy, candidates: policy.candidates.map((candidate, at) => at === index ? value : candidate) })
  return <div className="grid gap-3">
    <label className="flex min-h-11 items-center gap-2 text-meta"><input type="checkbox" disabled={disabled || !policy.candidates.length} checked={policy.automatic} onChange={(event) => change({ ...policy, automatic: event.target.checked })} />自动尝试备用模型</label>
    <p className="text-meta text-muted-foreground">仅在原回合明确未执行时按顺序尝试一次。使用运行设备对应 AI 服务的登录账户，可能产生该账户的费用。</p>
    {policy.candidates.map((candidate, index) => <div key={index} className="grid gap-2 rounded-lg border p-2">
      <label className="grid gap-1 text-meta">备用 AI 服务 {index + 1}<NativeSelect disabled={disabled} value={candidate.provider} onChange={(event) => { const provider = providerIds.find((provider) => provider === event.target.value); if (provider) update(index, { provider, model: '', reasoning_effort: null }) }}>{providerIds.map((provider) => <NativeSelectOption key={provider} value={provider}>{providerNames[provider]}</NativeSelectOption>)}</NativeSelect></label>
      <Input aria-label={`备用模型 ${index + 1}`} placeholder="模型名称" required value={candidate.model} disabled={disabled} onChange={(event) => update(index, { ...candidate, model: event.target.value })} />
      {effortOptions(candidate.provider).length ? <label className="grid gap-1 text-meta">备用推理强度<NativeSelect disabled={disabled} value={candidate.reasoning_effort ?? ''} onChange={(event) => update(index, { ...candidate, reasoning_effort: event.target.value || null })}><NativeSelectOption value="">由 AI 服务决定</NativeSelectOption>{effortOptions(candidate.provider).map((effort) => <NativeSelectOption key={effort} value={effort}>{effort}</NativeSelectOption>)}</NativeSelect></label> : <p className="text-meta text-muted-foreground">推理强度由 AI 服务决定</p>}
      <Button type="button" variant="ghost" disabled={disabled} onClick={() => { const candidates = policy.candidates.filter((_, at) => at !== index); change({ automatic: policy.automatic && candidates.length > 0, candidates }) }}>移除备用模型</Button>
    </div>)}
    <Button type="button" variant="outline" disabled={disabled || policy.candidates.length >= 5} onClick={() => change({ ...policy, candidates: [...policy.candidates, { provider: 'codex', model: '', reasoning_effort: null }] })}>添加备用模型</Button>
  </div>
}
