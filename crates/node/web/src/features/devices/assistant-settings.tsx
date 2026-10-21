import { useEffect, useRef, useState } from "react"
import { CircleAlert, CircleCheck, CircleX, FolderOpen, LoaderCircle, Plus, X } from "lucide-react"
import { cn } from "cn"
import { BusyIcon, Details, InlineError, Skeleton } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@/features/identity/context"
import { parseNodeSettings, type NodeSettings, type NodeSettingsResponse } from "@/lib/node"
import { issueText, openProvider, providerNames } from "./provider-settings"

type Provider = "disabled" | "codex_sdk" | "cursor_sdk"
type Draft = NodeSettingsResponse["defaults"] & { provider: Provider; model: string; roots: string[] }

const draftOf = (value: NodeSettingsResponse): Draft => ({
  ...value.defaults, ...value.settings.manager, provider: value.settings.manager?.provider ?? "disabled",
  workspace: value.settings.manager?.workspace ?? value.defaults.workspace, model: value.settings.manager?.model ?? "", roots: value.settings.roots,
})

export const docs = "https://github.com/leaperone/vibeface/blob/main/docs/"
// Names come from runtime/settings.rs readiness.missing.
const checks: Record<Exclude<Provider, "disabled">, { key: string; label: string; doc: string }[]> = {
  codex_sdk: [
    { key: "Node.js", label: "缺少 Node.js 18 以上", doc: "node.md#使用-codex-sdk" },
    { key: "Codex CLI", label: "缺少 Codex 命令行", doc: "node.md#使用-codex-sdk" },
    { key: "Codex SDK sidecar", label: "随包组件不完整，需重新安装 vibeface", doc: "install.md" },
    { key: "Codex SDK dependencies", label: "随包组件的依赖还没装", doc: "install.md#npm-nightly" },
    { key: "Codex App Server", label: "Codex 后台服务没启动，重启 vibeface", doc: "node.md#使用-codex-sdk" },
  ],
  cursor_sdk: [{ key: "Cursor SDK Bridge", label: "缺少 Cursor 组件", doc: "node.md#使用-cursor-sdk" }],
}
const providers = [["codex_sdk", "Codex"], ["cursor_sdk", "Cursor"], ["disabled", "不用"]] as const

/**
 * Assistant checklist for the device this browser is connected to. Other devices can't be configured yet.
 * Choosing a service or changing folders saves right away, so the check below always describes the saved choice.
 */
export function AssistantSettings({ onboarding = false, update, onChange }: { onboarding?: boolean; update?: NodeSettingsResponse | null; onChange?(value: NodeSettingsResponse): void }) {
  const { ready } = useDevice()
  const [response, setResponse] = useState<NodeSettingsResponse | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [folder, setFolder] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const sequence = useRef(0)
  const latest = useRef<NodeSettingsResponse | null>(null)

  async function load(save: Draft | null) {
    if (!ready?.isCurrent()) return
    const connection = ready, owner = ++sequence.current
    setBusy(true); setError(null)
    if (save) setDraft(save)
    try {
      const settings: NodeSettings | null = save && {
        roots: save.roots,
        manager: save.provider === "disabled" ? null : {
          provider: save.provider, workspace: save.workspace.trim(), model: save.model.trim(),
          cursor_sdk_bin: save.cursor_sdk_bin.trim(), node_bin: save.node_bin.trim(), codex_sdk_script: save.codex_sdk_script.trim(),
        },
      }
      const parsed = parseNodeSettings(await (settings ? connection.request("configure_node", settings) : connection.request("node_settings")))
      if (owner !== sequence.current || !connection.isCurrent()) return
      latest.current = parsed; setResponse(parsed); setDraft(draftOf(parsed))
      if (save) onChange?.(parsed)
    } catch (cause) {
      if (owner === sequence.current && connection.isCurrent()) {
        setError(cause)
        const last = latest.current
        if (last) setDraft((value) => value && { ...value, provider: draftOf(last).provider, roots: last.settings.roots })
      }
    } finally {
      if (owner === sequence.current) setBusy(false)
    }
  }
  useEffect(() => { setResponse(null); setDraft(null); void load(null); return () => { sequence.current += 1 } }, [ready])
  useEffect(() => { if (update) { latest.current = update; setResponse(update); setDraft((value) => value && { ...draftOf(update), ...value, provider: draftOf(update).provider, roots: update.settings.roots }) } }, [update])

  if (!draft || !response) return error ? <InlineError error={error} action={{ label: "重试", onClick: () => void load(null) }} /> : <div className="grid gap-2"><Skeleton className="h-9 w-48" /><Skeleton className="h-24" /></div>
  const saved = draftOf(response)
  const pathsDirty = (["model", "workspace", "cursor_sdk_bin", "node_bin", "codex_sdk_script"] as const).some((key) => draft[key] !== saved[key])
  const savedProvider = saved.provider
  const set = (patch: Partial<Draft>) => setDraft({ ...draft, ...patch })
  const choose = (provider: Provider) => {
    if (provider === draft.provider) return
    void load({ ...draft, provider, model: provider === "cursor_sdk" ? draft.model || "auto" : provider === "codex_sdk" && draft.model === "auto" ? "" : draft.model })
  }
  const addFolder = () => { const path = folder.trim(); setFolder(""); if (path && !draft.roots.includes(path)) void load({ ...draft, roots: [...draft.roots, path] }) }
  const missing = savedProvider === "disabled" ? [] : checks[savedProvider].filter((item) => response.readiness.missing.includes(item.key))
  const service = savedProvider === "disabled" ? undefined : response.providers.find((item) => item.provider === (savedProvider === "codex_sdk" ? "codex" : "cursor"))
  const serviceIssue = service?.state === "needs_setup" ? service.issues[0] : undefined

  return <form className="grid gap-5" onSubmit={(event) => { event.preventDefault(); void load(draft) }}>
    <fieldset disabled={busy} className="grid gap-2">
      <legend className="mb-2 text-meta font-medium text-muted-foreground">用哪个</legend>
      <div role="radiogroup" aria-label="助手用哪个 AI 服务" className="flex w-fit flex-wrap gap-1 rounded-lg bg-muted p-1">
        {providers.filter(([value]) => !onboarding || value !== "disabled").map(([value, label]) => <Button key={value} type="button" role="radio" aria-checked={draft.provider === value}
          variant={draft.provider === value ? "outline" : "ghost"} className={cn(draft.provider === value && "shadow-xs")} onClick={() => choose(value)}>{label}</Button>)}
      </div>
    </fieldset>

    {draft.provider !== "disabled" ? <div className="grid gap-2">
      {busy && draft.provider !== savedProvider ? <p role="status" className="flex items-center gap-2 text-meta text-muted-foreground"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />正在检查</p>
        : draft.provider !== savedProvider ? null
        : service && response.readiness.manager === "provider_off" ? <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-meta text-warning">
          <CircleAlert aria-hidden="true" className="size-4 shrink-0" />{providerNames[service.provider]} 已关闭，助手暂停
          {onboarding ? null : <button type="button" className="font-medium text-foreground underline underline-offset-4" onClick={() => openProvider(service.provider)}>去开启</button>}
        </p>
        : service && serviceIssue ? <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-meta text-warning">
          <CircleAlert aria-hidden="true" className="size-4 shrink-0" />{issueText(service.provider, serviceIssue)}
          {onboarding ? null : <button type="button" className="font-medium text-foreground underline underline-offset-4" onClick={() => openProvider(service.provider)}>去设置</button>}
        </p>
        : missing.length ? <ul className="grid gap-1.5">{missing.map((item) => <li key={item.key} className="flex min-w-0 items-center gap-2 text-meta">
          <CircleX aria-hidden="true" className="size-4 shrink-0 text-danger" />
          <span className="min-w-0 truncate">{item.label}</span>
          <a className="shrink-0 font-medium underline underline-offset-4" href={docs + item.doc} target="_blank" rel="noopener noreferrer">怎么装</a>
        </li>)}</ul>
        : <p className="flex items-center gap-2 text-meta text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />已就绪，登录在首次对话时确认</p>}
      <Details label="自定义路径">
        <label className="grid gap-1">模型<Input name="model" value={draft.model} maxLength={256} required={draft.provider === "cursor_sdk"} placeholder={draft.provider === "cursor_sdk" ? "auto" : "默认"} onChange={(event) => set({ model: event.target.value })} /></label>
        <label className="grid gap-1">助手工作文件夹<Input name="workspace" className="font-mono" value={draft.workspace} onChange={(event) => set({ workspace: event.target.value })} /></label>
        {draft.provider === "cursor_sdk"
          ? <label className="grid gap-1">Cursor 组件<Input name="cursor_sdk_bin" className="font-mono" value={draft.cursor_sdk_bin} onChange={(event) => set({ cursor_sdk_bin: event.target.value })} /></label>
          : <>
            <label className="grid gap-1">Node.js<Input name="node_bin" className="font-mono" value={draft.node_bin} onChange={(event) => set({ node_bin: event.target.value })} /></label>
            <label className="grid gap-1">Codex 组件<Input name="codex_sdk_script" className="font-mono" value={draft.codex_sdk_script} onChange={(event) => set({ codex_sdk_script: event.target.value })} /></label>
          </>}
        {pathsDirty ? <Button type="submit" variant="outline" className="w-fit" disabled={busy}><BusyIcon busy={busy} />保存路径</Button> : null}
      </Details>
    </div> : null}

    <div className="grid gap-2">
      <p className="text-meta font-medium text-muted-foreground">可用文件夹</p>
      {draft.roots.length ? <ul className="grid gap-1">{draft.roots.map((root) => <li key={root} className="flex min-h-9 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3">
        <FolderOpen aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-mono text-meta" title={root}>{root}</span>
        <Button type="button" variant="ghost" size="icon" aria-label={`移除 ${root}`} title="移除" disabled={busy} onClick={() => void load({ ...draft, roots: draft.roots.filter((item) => item !== root) })}><X aria-hidden="true" /></Button>
      </li>)}</ul> : <p className="text-meta text-muted-foreground">还没有，助手只能在自己的文件夹里工作</p>}
      <div className="flex min-w-0 gap-2">
        <Input aria-label="文件夹路径" className="min-w-0 font-mono" placeholder="/Users/me/project" value={folder} disabled={busy} onChange={(event) => setFolder(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); addFolder() } }} />
        <Button type="button" variant="outline" disabled={busy || !folder.trim()} onClick={addFolder}><Plus aria-hidden="true" />添加</Button>
      </div>
    </div>
    <InlineError error={error} />
  </form>
}
