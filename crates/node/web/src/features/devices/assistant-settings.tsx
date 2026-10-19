import { useEffect, useRef, useState } from "react"
import { CircleCheck, CircleX, FolderOpen, Plus, X } from "lucide-react"
import { cn } from "cn"
import { BusyIcon, Details, InlineError, Skeleton } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@/features/identity/context"
import { parseNodeSettings, type NodeSettings, type NodeSettingsResponse } from "@/lib/node"

type Provider = "disabled" | "codex_sdk" | "cursor_sdk"
type Draft = NodeSettingsResponse["defaults"] & { provider: Provider; model: string; roots: string[] }

const draftOf = (value: NodeSettingsResponse): Draft => ({
  ...value.defaults, ...value.settings.manager, provider: value.settings.manager?.provider ?? "disabled",
  workspace: value.settings.manager?.workspace ?? value.defaults.workspace, model: value.settings.manager?.model ?? "", roots: value.settings.roots,
})

// Names come from runtime/settings.rs readiness.missing.
const checks: Record<Exclude<Provider, "disabled">, { key: string; label: string; fix: string }[]> = {
  codex_sdk: [
    { key: "Node.js", label: "Node.js", fix: "从 nodejs.org 安装 20 或更高版本" },
    { key: "Codex CLI", label: "Codex 命令行", fix: "运行 npm install -g @openai/codex" },
    { key: "Codex SDK sidecar", label: "Codex 组件", fix: "重新安装 vibeface" },
    { key: "Codex SDK dependencies", label: "Codex 组件依赖", fix: "重新安装 vibeface" },
    { key: "Codex App Server", label: "Codex 后台服务", fix: "重启 vibeface" },
  ],
  cursor_sdk: [{ key: "Cursor SDK Bridge", label: "Cursor 组件", fix: "安装 cursor-sdk-bridge 并加入 PATH" }],
}

/** Assistant checklist for the device this browser is connected to. Other devices can't be configured yet. */
export function AssistantSettings() {
  const { ready } = useDevice()
  const [response, setResponse] = useState<NodeSettingsResponse | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [folder, setFolder] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const [saved, setSaved] = useState(false)
  const sequence = useRef(0)

  async function load(save: Draft | null) {
    if (!ready?.isCurrent()) return
    const connection = ready, owner = ++sequence.current
    setBusy(true); setError(null); setSaved(false)
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
      setResponse(parsed); setDraft(draftOf(parsed)); setSaved(Boolean(save))
    } catch (cause) {
      if (owner === sequence.current && connection.isCurrent()) setError(cause)
    } finally {
      if (owner === sequence.current) setBusy(false)
    }
  }
  useEffect(() => { setResponse(null); setDraft(null); void load(null); return () => { sequence.current += 1 } }, [ready])

  if (!draft || !response) return error ? <InlineError error={error} action={{ label: "重试", onClick: () => void load(null) }} /> : <div className="grid gap-2"><Skeleton className="h-9 w-48" /><Skeleton className="h-24" /></div>
  const dirty = JSON.stringify(draft) !== JSON.stringify(draftOf(response))
  const savedProvider = response.settings.manager?.provider ?? "disabled"
  const set = (patch: Partial<Draft>) => { setDraft({ ...draft, ...patch }); setSaved(false) }
  const addFolder = () => { const path = folder.trim(); if (path && !draft.roots.includes(path)) set({ roots: [...draft.roots, path] }); setFolder("") }

  return <form className="grid gap-5" onSubmit={(event) => { event.preventDefault(); void load(draft) }}>
    <fieldset disabled={busy} className="grid gap-2">
      <legend className="mb-2 text-meta font-medium text-muted-foreground">AI 服务</legend>
      <div role="radiogroup" className="flex flex-wrap gap-2">
        {([["disabled", "不用"], ["codex_sdk", "Codex"], ["cursor_sdk", "Cursor"]] as const).map(([value, label]) => <Button key={value} type="button" role="radio" aria-checked={draft.provider === value} variant={draft.provider === value ? "default" : "outline"}
          onClick={() => set({ provider: value, model: value === "cursor_sdk" ? draft.model || "auto" : value === "codex_sdk" && draft.model === "auto" ? "" : draft.model })}>{label}</Button>)}
      </div>
    </fieldset>

    {draft.provider !== "disabled" ? <div className="grid gap-2">
      <p className="text-meta font-medium text-muted-foreground">程序检查</p>
      {response.readiness.manager === "role_unavailable" ? <p className="text-meta text-warning">这台设备没有运行助手的权限</p>
        : savedProvider !== draft.provider ? <p className="text-meta text-muted-foreground">保存后检查</p>
        : <ul className="grid gap-1.5">{checks[draft.provider].map((item) => {
          const ok = !response.readiness.missing.includes(item.key)
          return <li key={item.key} className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
            {ok ? <CircleCheck aria-label="已就绪" className="size-4 text-success" /> : <CircleX aria-label="缺少" className="size-4 text-danger" />}
            <span>{item.label}</span>
            {ok ? null : <span className="w-full ps-6 text-meta text-muted-foreground">{item.fix}</span>}
          </li>
        })}</ul>}
      <p className="text-meta text-muted-foreground">登录：首次对话时确认</p>
      <Details label="自定义路径">
        <label className="grid gap-1">模型<Input name="model" value={draft.model} maxLength={256} required={draft.provider === "cursor_sdk"} placeholder={draft.provider === "cursor_sdk" ? "auto" : "默认"} onChange={(event) => set({ model: event.target.value })} /></label>
        <label className="grid gap-1">助手工作文件夹<Input name="workspace" className="font-mono" value={draft.workspace} onChange={(event) => set({ workspace: event.target.value })} /></label>
        {draft.provider === "cursor_sdk"
          ? <label className="grid gap-1">Cursor 组件<Input name="cursor_sdk_bin" className="font-mono" value={draft.cursor_sdk_bin} onChange={(event) => set({ cursor_sdk_bin: event.target.value })} /></label>
          : <>
            <label className="grid gap-1">Node.js<Input name="node_bin" className="font-mono" value={draft.node_bin} onChange={(event) => set({ node_bin: event.target.value })} /></label>
            <label className="grid gap-1">Codex 组件<Input name="codex_sdk_script" className="font-mono" value={draft.codex_sdk_script} onChange={(event) => set({ codex_sdk_script: event.target.value })} /></label>
          </>}
      </Details>
    </div> : null}

    <div className="grid gap-2">
      <p className="text-meta font-medium text-muted-foreground">可用文件夹</p>
      {draft.roots.length ? <ul className="grid gap-1">{draft.roots.map((root) => <li key={root} className="flex min-h-9 items-center gap-2 rounded-lg bg-muted/60 ps-3">
        <FolderOpen aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-mono text-meta" title={root}>{root}</span>
        <Button type="button" variant="ghost" size="icon" aria-label={`移除 ${root}`} disabled={busy} onClick={() => set({ roots: draft.roots.filter((item) => item !== root) })}><X aria-hidden="true" /></Button>
      </li>)}</ul> : <p className="text-meta text-muted-foreground">还没有，助手只能在自己的文件夹里工作</p>}
      <div className="flex gap-2">
        <Input aria-label="文件夹路径" className="font-mono" placeholder="/Users/me/project" value={folder} disabled={busy} onChange={(event) => setFolder(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); addFolder() } }} />
        <Button type="button" variant="outline" disabled={busy || !folder.trim()} onClick={addFolder}><Plus aria-hidden="true" />添加</Button>
      </div>
    </div>

    <div className={cn("flex flex-wrap items-center gap-3", !dirty && !saved && !error && "hidden")}>
      {dirty ? <Button type="submit" disabled={busy}><BusyIcon busy={busy} />保存</Button> : null}
      {saved && !dirty ? <span role="status" className="flex items-center gap-1.5 text-meta text-success"><CircleCheck aria-hidden="true" className="size-4" />已保存，正在进行的任务不受影响</span> : null}
      <InlineError error={error} />
    </div>
  </form>
}
