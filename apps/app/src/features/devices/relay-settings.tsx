import { useEffect, useRef, useState } from "react"
import { Check, CircleCheck, Copy, LoaderCircle, RadioTower, Share2, SlidersHorizontal, TriangleAlert } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, InlineError, SettingsDisclosure, SettingsGroup, SettingsRow, Skeleton } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { parseRelaySettings, parseShareLink, relayDefaults, type RelaySettings as Settings } from "@vibeface/client/lib/node.ts"

export const relayLabel = "公共转发"
const shareLabel = "节点链接"

/** The connected Node's node link, which another Node takes as `--bootstrap`. */
export function ShareLink() {
  const { ready } = useDevice()
  const [link, setLink] = useState("")
  const [copied, setCopied] = useState(false)
  const [error, setError] = useState<unknown>(null)
  useEffect(() => {
    if (!ready?.isCurrent()) return
    const connection = ready
    let live = true
    setLink(""); setError(null); setCopied(false)
    connection.request("share_link").then((raw) => { if (live && connection.isCurrent()) setLink(parseShareLink(raw)) }, (cause) => { if (live) setError(cause) })
    return () => { live = false }
  }, [ready])
  const copy = () => void navigator.clipboard.writeText(link).then(() => setCopied(true), () => {})
  return <SettingsGroup>
    <SettingsRow icon={Share2} label={shareLabel}>
      {link ? <Button type="button" variant="ghost" size="icon-sm" aria-label={copied ? "已复制" : "复制"} title={copied ? "已复制" : "复制"} onClick={copy}>
        {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
      </Button> : error ? null : <Skeleton className="h-6 w-11" />}
    </SettingsRow>
    {link ? <div className="px-4 py-3"><code className="block select-all break-all font-mono text-meta text-muted-foreground">{link}</code></div> : null}
    {error ? <div className="px-4 py-3"><InlineError error={error} /></div> : null}
  </SettingsGroup>
}
const fieldLabel = "-mb-1 text-meta font-medium text-muted-foreground"
/** Off shows the switch alone; the other states an icon, their words only on hover and for screen readers. */
const states = {
  checking: { icon: LoaderCircle, tone: "animate-spin text-info", text: "正在从外网检查" },
  announced: { icon: CircleCheck, tone: "text-success", text: "已开放" },
  unreachable: { icon: TriangleAlert, tone: "text-warning", text: "外网连不上" },
} as const
const limits = [["rate", "每个连接 B/s"], ["max_clients", "最多连接"], ["max_clients_per_ip", "同一 IP 最多"]] as const
type Draft = { url: string } & Record<typeof limits[number][0], string>
const draftOf = (settings: Settings): Draft => ({
  url: settings.url ?? "",
  rate: String(settings.rate || relayDefaults.rate), max_clients: String(settings.max_clients || relayDefaults.max_clients), max_clients_per_ip: String(settings.max_clients_per_ip || relayDefaults.max_clients_per_ip),
})

/** The connected Node's open relay for other people's devices, for a management device or a local session. */
export function RelaySettings() {
  const { ready } = useDevice()
  const [settings, setSettings] = useState<Settings | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [busy, setBusy] = useState("")
  const [error, setError] = useState<unknown>(null)
  const sequence = useRef(0)

  async function call(key: string, method: "relay_settings" | "configure_relay", params?: Record<string, unknown>) {
    if (!ready?.isCurrent()) return
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const next = parseRelaySettings(await connection.request(method, params))
      if (owner === sequence.current && connection.isCurrent()) { setSettings(next); setDraft(draftOf(next)) }
    } catch (cause) {
      if (owner === sequence.current) setError(cause)
    } finally {
      if (owner === sequence.current) setBusy("")
    }
  }
  useEffect(() => { setSettings(null); void call("load", "relay_settings"); return () => { sequence.current += 1 } }, [ready])
  // The outside check takes a while; its result shows without a reload. A poll never touches the form or an action.
  const checking = settings?.status === "checking"
  useEffect(() => {
    if (!checking || !ready) return
    const timer = setInterval(() => {
      const owner = sequence.current
      void ready.request("relay_settings").then((raw) => {
        const next = parseRelaySettings(raw)
        if (owner === sequence.current && ready.isCurrent()) setSettings(next)
      }, () => {})
    }, 3000)
    return () => clearInterval(timer)
  }, [checking, ready])

  const state = settings && settings.status !== "off" ? states[settings.status] : null
  const header = <SettingsRow icon={RadioTower} label={relayLabel}>
    {state ? <span role="status" title={state.text} className="inline-flex"><state.icon aria-hidden="true" className={cn("size-4", state.tone)} /><span className="sr-only">{state.text}</span></span> : null}
    {settings ? <input type="checkbox" role="switch" className="switch" aria-label={relayLabel} checked={settings.volunteer} disabled={Boolean(busy)}
      onChange={(event) => void call("switch", "configure_relay", { volunteer: event.target.checked })} />
      : error ? null : <Skeleton className="h-6 w-11" />}
  </SettingsRow>
  if (!settings || !draft) return <SettingsGroup>{header}{error ? <div className="px-4 py-3"><InlineError error={error} action={{ label: "重试", onClick: () => void call("load", "relay_settings") }} /></div> : null}</SettingsGroup>

  const saved = draftOf(settings)
  const unchanged = (Object.keys(saved) as (keyof Draft)[]).every((key) => draft[key].trim() === saved[key])
  const save = () => void call("save", "configure_relay", {
    volunteer: true, ...draft.url.trim() ? { url: draft.url.trim() } : {}, ...settings.listen ? { listen: settings.listen } : {},
    ...Object.fromEntries(limits.map(([key]) => [key, Number(draft[key])])),
  })
  return <SettingsGroup>
    {header}
    {settings.volunteer ? <SettingsDisclosure icon={SlidersHorizontal} label="地址和限额">
      <form className="grid gap-3" onSubmit={(event) => { event.preventDefault(); save() }}>
        <label htmlFor="relay-url" className={fieldLabel}>地址</label>
        <Input id="relay-url" type="url" className="font-mono" placeholder="http://" value={draft.url} disabled={Boolean(busy)} onChange={(event) => setDraft({ ...draft, url: event.target.value })} />
        <div className="grid gap-3 sm:grid-cols-3">
          {limits.map(([key, text]) => <div key={key} className="grid gap-2">
            <label htmlFor={`relay-${key}`} className={fieldLabel}>{text}</label>
            <Input id={`relay-${key}`} type="number" inputMode="numeric" min={1} step={1} required className="tabular-nums" value={draft[key]} disabled={Boolean(busy)} onChange={(event) => setDraft({ ...draft, [key]: event.target.value })} />
          </div>)}
        </div>
        <Button type="submit" variant="outline" className="w-fit" disabled={Boolean(busy) || unchanged}><BusyIcon busy={busy === "save"} />保存</Button>
      </form>
    </SettingsDisclosure> : null}
    {error || settings.error ? <div className="px-4 py-3"><InlineError error={error || settings.error} /></div> : null}
  </SettingsGroup>
}
