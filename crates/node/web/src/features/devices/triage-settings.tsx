import { useEffect, useRef, useState } from "react"
import { CircleCheck, CircleHelp, CircleX, Clock, Copy, FolderTree, Inbox, Layers, MessageCircleQuestionMark, Plus, Send, Trash2, X, Zap, type LucideIcon } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, Details, InlineError, Segmented, SettingsDisclosure, SettingsRow, Skeleton, toast } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { terms } from "@/lib/glossary"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { waitingCopy } from "@vibeface/client/identity/copy.ts"
import { parseTriageSettings } from "@vibeface/client/workspace/model.ts"
import { webhookTestResult } from "@vibeface/client/lib/errors.ts"
import type { Outlet, TriageSettings as Settings } from "@vibeface/client/workspace/types.ts"

const chat = terms.externalChat.label
const outlets: [Outlet, string, LucideIcon][] = [["record", waitingCopy.outlets.record, Inbox], ["both", waitingCopy.outlets.both, Layers]]
const outletOptions = outlets.map(([value, label, icon]) => ({ value, label, icon }))
const fieldLabel = "-mb-1 text-meta font-medium text-muted-foreground"
type Config = Pick<Settings, "default_outlet" | "projects" | "webhook_url" | "scan_codex_sessions" | "codex_sessions">

/** Settings → 会话等你时, one folded row: where this device sends sessions that need the person, plus the external chat (Grok Bot) channel both ways. Saves on each change. */
export function TriageSettings() {
  const { ready } = useDevice()
  const [settings, setSettings] = useState<Settings | null>(null)
  const [busy, setBusy] = useState("")
  const [error, setError] = useState<unknown>(null)
  const [prefix, setPrefix] = useState("")
  const [url, setUrl] = useState("")
  const [secret, setSecret] = useState("")
  const [label, setLabel] = useState("")
  const [token, setToken] = useState("")
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null)
  const [open, setOpen] = useState(false)
  const [confirming, setConfirming] = useState("")
  const sequence = useRef(0)

  async function call(key: string, method: string, params?: Record<string, unknown>) {
    if (!ready?.isCurrent()) return null
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const result = await connection.request(method, params)
      if (owner === sequence.current && connection.isCurrent()) {
        const next = parseTriageSettings(method === "triage_settings" || method === "configure_triage" ? result : await connection.request("triage_settings"))
        setSettings(next); setUrl(next.webhook_url)
      }
      return result
    } catch (cause) {
      if (owner === sequence.current) setError(cause)
      return null
    } finally {
      if (owner === sequence.current) setBusy("")
    }
  }
  useEffect(() => { setSettings(null); void call("load", "triage_settings"); return () => { sequence.current += 1 } }, [ready])

  const header = <SettingsRow icon={terms.externalChat.icon} label={waitingCopy.forward} expanded={open} onClick={() => setOpen(!open)}>{settings && !open ? settings.webhook_url ? "已连接" : "未连接" : null}</SettingsRow>
  if (!open) return header
  if (!settings) return <>{header}<div className="px-4 py-3">{error ? <InlineError error={error} action={{ label: "重试", onClick: () => void call("load", "triage_settings") }} /> : <Skeleton className="h-8" />}</div></>
  const config: Config = { default_outlet: settings.default_outlet, projects: settings.projects, webhook_url: settings.webhook_url, scan_codex_sessions: settings.scan_codex_sessions, codex_sessions: settings.codex_sessions }
  const save = (key: string, patch: Partial<Config>, extra: Record<string, unknown> = {}) => {
    const next = { ...config, ...patch }
    return call(key, "configure_triage", { ...next, webhook_url: next.webhook_url.trim() || null, ...extra })
  }
  const addProject = () => {
    const path = prefix.trim()
    setPrefix("")
    // A project is added to differ from the default.
    if (path && !config.projects.some((item) => item.prefix === path)) void save("projects", { projects: [...config.projects, { prefix: path, outlet: config.default_outlet === "both" ? "record" : "both" }] })
  }
  const { pending, delivered, dead } = settings.outbox
  // Until a chat is connected there is nothing to forward, so only connecting shows.
  const connected = Boolean(settings.webhook_url)

  const webhookForm = <form className="grid gap-2" onSubmit={(event) => { event.preventDefault(); void save("webhook", { webhook_url: url }, secret ? { webhook_secret: secret } : {}).then(() => setSecret("")) }}>
    <div className="grid gap-0.5">
      <p className="flex items-center gap-2 text-meta font-medium text-foreground"><Send aria-hidden="true" className="size-4" />把提醒发给{chat}
        <a href={waitingCopy.forwardHelp} target="_blank" rel="noopener noreferrer" aria-label="怎么接" title="怎么接" className="hit-area relative ms-auto text-muted-foreground hover:text-foreground"><CircleHelp aria-hidden="true" className="size-4" /></a>
      </p>
      <p className="text-meta text-muted-foreground">{waitingCopy.forwardHint}</p>
    </div>
    <label htmlFor="webhook-url" className={fieldLabel}>地址<span className="font-normal"> · {waitingCopy.urlHint}</span></label>
    <Input id="webhook-url" type="url" className="font-mono" placeholder="https://" value={url} onChange={(event) => setUrl(event.target.value)} />
    <label htmlFor="webhook-secret" className={fieldLabel}>密钥<span className="font-normal"> · {waitingCopy.secretHint}</span></label>
    {confirming === "secret" ? <p className="text-meta text-foreground">清除后提醒发不出去</p> : null}
    <div className="flex min-w-0 gap-2">
      <Input id="webhook-secret" type="password" autoComplete="new-password" minLength={16} maxLength={512} className="min-w-0" placeholder={settings.webhook_secret_set ? "已设置" : "至少 16 个字符"} value={secret} onChange={(event) => setSecret(event.target.value)} />
      {!settings.webhook_secret_set ? null : confirming === "secret" ? <>
        <Button type="button" size="sm" variant="ghost" autoFocus onClick={() => setConfirming("")}>取消</Button>
        <Button type="button" size="sm" variant="destructive" disabled={Boolean(busy)} onClick={() => { setConfirming(""); void save("webhook", {}, { webhook_secret: null }) }}>清除</Button>
      </> : <Button type="button" variant="ghost" size="icon" aria-label="清除密钥" title="清除密钥" disabled={Boolean(busy)} onClick={() => setConfirming("secret")}><Trash2 aria-hidden="true" /></Button>}
    </div>
    <div className="flex flex-wrap items-center gap-2">
      <Button type="submit" variant="outline" disabled={Boolean(busy) || (url.trim() === settings.webhook_url && !secret)}><BusyIcon busy={busy === "webhook"} />保存</Button>
      <Button type="button" variant="ghost" disabled={Boolean(busy) || !settings.webhook_url || !settings.webhook_secret_set} onClick={() => {
        setTest(null)
        void call("test", "test_webhook").then((result) => {
          const data = result as { ok?: boolean; status?: number | null; error?: string | null } | null
          if (data) setTest(webhookTestResult(data))
        })
      }}><BusyIcon busy={busy === "test"} icon={Zap} />测试</Button>
      {test ? <span role="status" className={cn("flex items-center gap-1 text-meta", test.ok ? "text-success" : "text-danger")}>{test.ok ? <CircleCheck aria-hidden="true" className="size-4" /> : <CircleX aria-hidden="true" className="size-4" />}{test.text}</span> : null}
      {pending + delivered + dead ? <ul aria-label="发送记录" className="ms-auto flex items-center gap-3 text-meta tabular-nums text-muted-foreground">
        {([[delivered, "已送达", CircleCheck, "text-success"], [pending, "待重试", Clock, ""], [dead, "失败", CircleX, "text-danger"]] as const).map(([count, label, Icon, tone]) =>
          count ? <li key={label} className="flex items-center gap-1" title={label}><Icon aria-hidden="true" className={cn("size-4", tone)} /><span className="sr-only">{label} </span>{count}</li> : null)}
      </ul> : null}
    </div>
  </form>

  return <>
    {header}
    <div role="status" aria-label="分诊状态" className="grid gap-1 px-4 py-3 text-meta text-muted-foreground sm:ps-15">
      <p className="tabular-nums">分诊今日已用 {settings.status.budget.used} / {settings.status.budget.limit} 次</p>
      <p>{settings.status.budget.paused_reason ? "今日额度已用完，分诊暂停，" : "额度重置时间："}{new Date(settings.status.budget.retry_at_ms).toLocaleString("zh-CN")}{settings.status.budget.paused_reason ? "后恢复。" : "。"}</p>
      <p>待审阅 {settings.status.pending_events} · 审阅中 {settings.status.inflight_events} · 待确认 {settings.status.unknown_events}</p>
      {settings.status.unknown_events ? <p>结果待确认的事件不会自动重试。</p> : null}
    </div>
    {connected ? <>
    <SettingsRow icon={MessageCircleQuestionMark} label="默认去向" stacked>
      <Segmented label="默认去向" labels fill value={config.default_outlet} disabled={Boolean(busy)} options={outletOptions} onChange={(value) => void save("default", { default_outlet: value })} />
    </SettingsRow>

    <SettingsDisclosure icon={FolderTree} label="按文件夹另设" value={config.projects.length || undefined} open={config.projects.length > 0}>
      {config.projects.length ? <ul className="grid gap-1">{config.projects.map((project) => <li key={project.prefix} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3">
        <span className="min-w-0 flex-1 truncate font-mono text-meta" title={project.prefix}>{project.prefix.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || project.prefix}</span>
        <NativeSelect size="sm" aria-label={`${project.prefix} 的去向`} value={project.outlet} disabled={Boolean(busy)}
          onChange={(event) => void save("projects", { projects: config.projects.map((item) => item.prefix === project.prefix ? { ...item, outlet: event.currentTarget.value as Outlet } : item) })}>
          {outlets.map(([value, text]) => <NativeSelectOption key={value} value={value}>{text}</NativeSelectOption>)}
        </NativeSelect>
        <Button type="button" variant="ghost" size="icon" aria-label={`移除 ${project.prefix}`} title="移除" disabled={Boolean(busy)} onClick={() => void save("projects", { projects: config.projects.filter((item) => item.prefix !== project.prefix) })}><X aria-hidden="true" /></Button>
      </li>)}</ul> : null}
      <div className="flex min-w-0 gap-2">
        <Input aria-label="文件夹路径" className="min-w-0 font-mono" placeholder="~/projects" value={prefix} disabled={Boolean(busy)} onChange={(event) => setPrefix(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); addProject() } }} />
        <Button type="button" variant="outline" disabled={Boolean(busy) || !prefix.trim()} onClick={addProject}><Plus aria-hidden="true" />添加</Button>
      </div>
    </SettingsDisclosure>
    </> : null}

    {/* Until a chat is connected the header opens straight onto connecting it. */}
    {connected ? <SettingsDisclosure icon={terms.externalChat.icon} label={chat} value="已连接">
      {webhookForm}

      <Details label={`让${chat}代你回答`}>
        <p className="flex items-center gap-2 text-meta text-foreground"><terms.secret.icon aria-hidden="true" className="size-4" />{chat}用这个地址和授权码回话</p>
        <span className={fieldLabel}>地址</span>
        <div className="flex min-w-0 items-center gap-1 rounded-lg bg-muted/60 ps-3">
          <code className="min-w-0 flex-1 truncate font-mono text-meta" title={settings.decision_url}>{settings.decision_url}</code>
          <Button type="button" variant="ghost" size="icon" aria-label="复制地址" title="复制地址" onClick={() => void navigator.clipboard?.writeText(settings.decision_url).then(() => toast("已复制", { tone: "success" }))}><Copy aria-hidden="true" /></Button>
        </div>
        {token ? <div role="status" className="flex min-w-0 items-center gap-2 rounded-lg border border-warning-border bg-warning-soft px-3 py-2">
          <code className="min-w-0 flex-1 break-all font-mono text-meta">{token}</code>
          <Button type="button" variant="ghost" size="icon" aria-label="复制授权码" title="只显示这一次" onClick={() => void navigator.clipboard?.writeText(token).then(() => toast("已复制", { tone: "success" }))}><Copy aria-hidden="true" /></Button>
          <Button type="button" variant="ghost" size="icon" aria-label="收起" title="收起" onClick={() => setToken("")}><X aria-hidden="true" /></Button>
        </div> : null}
        {settings.decision_tokens.length ? <ul className="grid gap-1">{settings.decision_tokens.map((item) => <li key={item.token_id} className="flex min-h-9 min-w-0 flex-wrap items-center gap-x-2 rounded-lg bg-muted/60 ps-3">
          {confirming === item.token_id ? <span className="order-first basis-full pt-2 text-meta text-foreground">撤销后它不能再代你回答</span> : null}
          <span className="min-w-0 flex-1 truncate text-meta">{item.label || "未命名"}</span>
          <span className="shrink-0 text-meta text-muted-foreground tabular-nums">{new Date(item.created_ms).toLocaleDateString('zh-CN', { dateStyle: 'long' })}</span>
          {confirming === item.token_id ? <>
            <Button type="button" size="sm" variant="ghost" autoFocus onClick={() => setConfirming("")}>取消</Button>
            <Button type="button" size="sm" variant="destructive" disabled={Boolean(busy)} onClick={() => { setConfirming(""); void call("revoke", "revoke_decision_token", { token_id: item.token_id }) }}>撤销</Button>
          </> : <Button type="button" variant="ghost" size="icon" aria-label={`撤销 ${item.label || "授权码"}`} title="撤销" disabled={Boolean(busy)} onClick={() => setConfirming(item.token_id)}><Trash2 aria-hidden="true" /></Button>}
        </li>)}</ul> : null}
        <div className="flex min-w-0 gap-2">
          <Input aria-label="授权码名称" className="min-w-0" placeholder={chat} maxLength={64} value={label} disabled={Boolean(busy)} onChange={(event) => setLabel(event.target.value)} />
          <Button type="button" variant="outline" disabled={Boolean(busy)} onClick={() => void call("token", "create_decision_token", { label: label.trim() || chat }).then((result) => {
            const value = (result as { token?: unknown } | null)?.token
            if (typeof value === "string") { setToken(value); setLabel("") }
          })}><BusyIcon busy={busy === "token"} icon={Plus} />生成授权码</Button>
        </div>
      </Details>
    </SettingsDisclosure> : <div className="grid gap-3 px-4 py-3 sm:ps-15">
      {webhookForm}
    </div>}
    {error ? <div className="px-4 py-3"><InlineError error={error} /></div> : null}
  </>
}
