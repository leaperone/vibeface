"use client"

import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react"
import { ChevronRight, CircleAlert, CircleCheck, Info, LoaderCircle, TriangleAlert, Upload, X, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { Button } from "@/components/ui/button"
import { messageOf } from "@/lib/node"
import { rawError } from "@/lib/errors"

export type Tone = "neutral" | "success" | "warning" | "danger" | "info"

const toneText: Record<Tone, string> = {
  neutral: "text-muted-foreground", success: "text-success", warning: "text-warning", danger: "text-danger", info: "text-info",
}
const toneSurface: Record<Tone, string> = {
  neutral: "border-border bg-card", success: "border-success-border bg-success-soft", warning: "border-warning-border bg-warning-soft",
  danger: "border-danger-border bg-danger-soft", info: "border-info-border bg-info-soft",
}

export function EmptyState({ icon: Icon, title, description, action, className }: { icon: LucideIcon; title: string; description?: string; action?: ReactNode; className?: string }) {
  return <div className={cn("mx-auto flex max-w-sm flex-col items-center gap-3 px-4 py-12 text-center", className)}>
    <Icon aria-hidden="true" className="size-8 text-muted-foreground" />
    <h2 className="text-heading font-semibold">{title}</h2>
    {description ? <p className="text-meta text-muted-foreground">{description}</p> : null}
    {action ? <div className="pt-1">{action}</div> : null}
  </div>
}

/** Same shape as the real layout; fades in only after 300ms to avoid flicker. */
export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden="true" className={cn("skeleton rounded-md bg-muted", className)} />
}

export function InlineError({ error, action, className }: { error: unknown; action?: { label: string; onClick: () => void }; className?: string }) {
  if (!error) return null
  const text = messageOf(error), raw = rawError(error)
  return <div role="alert" className={cn("flex flex-wrap items-start gap-2 text-meta text-danger", className)}>
    <CircleAlert aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
    <span className="min-w-0 flex-1 break-words">{text}</span>
    {action ? <button type="button" className="font-medium underline underline-offset-4" onClick={action.onClick}>{action.label}</button> : null}
    {raw ? <Details className="basis-full ps-6"><code className="font-mono">{raw}</code></Details> : null}
  </div>
}

export function StatusPill({ tone = "neutral", icon: Icon, children, className }: { tone?: Tone; icon?: LucideIcon; children: ReactNode; className?: string }) {
  return <span className={cn("inline-flex h-6 shrink-0 items-center gap-1 rounded-full border px-2 text-meta font-medium", toneSurface[tone], toneText[tone], className)}>
    {Icon ? <Icon aria-hidden="true" className="size-3.5" /> : null}{children}
  </span>
}

/** Shared by approvals, join requests and confirmations, in chat and in /inbox. */
export function ActionCard({ icon: Icon, tone = "warning", title, meta, menu, children, actions, className }: {
  icon: LucideIcon; tone?: Tone; title: ReactNode; meta?: ReactNode; menu?: ReactNode; children?: ReactNode; actions?: ReactNode; className?: string
}) {
  return <section className={cn("grid gap-3 rounded-xl border p-4", toneSurface[tone], className)}>
    <header className="flex min-w-0 flex-wrap items-center gap-2">
      <Icon aria-hidden="true" className={cn("size-5 shrink-0", toneText[tone])} />
      <h3 className="min-w-0 flex-1 text-heading font-semibold">{title}</h3>
      {meta ? <span className="max-w-[40%] shrink-0 truncate text-meta text-muted-foreground max-sm:order-last max-sm:max-w-full max-sm:basis-full max-sm:ps-7">{meta}</span> : null}
      {menu}
    </header>
    {children ? <div className="grid min-w-0 gap-2">{children}</div> : null}
    {actions ? <div className="flex flex-wrap justify-end gap-2">{actions}</div> : null}
  </section>
}

export function SafetyCode({ code, label = "安全码" }: { code: string; label?: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <div className="flex flex-wrap items-center gap-3">
    <span className="sr-only">{label}</span>
    <output aria-label={`${label} ${code.split("").join(" ")}`} className="font-mono text-2xl tracking-[0.1em] tabular-nums">{groups.join(" ")}</output>
  </div>
}

/** Collapsed by default. Jargon from docs/design/web-ux.md section 6 may only appear in here. */
export function Details({ children, label = "详细信息", className, open }: { children: ReactNode; label?: string; className?: string; open?: boolean }) {
  return <details open={open} className={cn("details text-meta", className)}>
    <summary className="inline-flex min-h-9 items-center gap-1 text-muted-foreground hover:text-foreground">
      <ChevronRight aria-hidden="true" className="size-4 transition-transform" />{label}
    </summary>
    <div className="grid gap-2 break-all pt-2 text-muted-foreground">{children}</div>
  </details>
}

/** Long hex ids show their first 10 characters; a click reveals the rest. */
function LongId({ value }: { value: string }) {
  const [full, setFull] = useState(false)
  return <button type="button" className="w-fit text-left font-mono text-foreground break-all" title={full ? undefined : value} aria-expanded={full} onClick={() => setFull(!full)}>{full ? value : `${value.slice(0, 10)}…`}</button>
}

export function DetailRow({ label, value, mono = false }: { label: string; value: ReactNode; mono?: boolean }) {
  return <div className="grid gap-0.5"><span>{label}</span>{typeof value === "string" && /^[0-9a-f]{20,}$/.test(value) ? <LongId value={value} /> : <span className={cn("text-foreground", mono && "font-mono")}>{value}</span>}</div>
}

/** File input with a Chinese button; the native control's label follows the browser language. */
export function FilePicker({ name, accept, required, disabled, label = "选择文件", onFile }: { name?: string; accept?: string; required?: boolean; disabled?: boolean; label?: string; onFile?(file: File): void }) {
  const [chosen, setChosen] = useState("")
  return <span className="flex min-w-0 items-center gap-3">
    <label className={cn("relative inline-flex h-9 shrink-0 cursor-pointer items-center gap-1.5 rounded-lg border bg-background px-3 text-sm font-medium hover:bg-muted max-md:h-11 has-[:focus-visible]:ring-3 has-[:focus-visible]:ring-ring/50", disabled && "pointer-events-none opacity-50")}>
      <Upload aria-hidden="true" className="size-4" />{label}
      <input className="absolute inset-0 size-full cursor-pointer opacity-0" type="file" name={name} accept={accept} required={required} disabled={disabled} onChange={(event) => {
        const file = event.currentTarget.files?.item(0)
        setChosen(file?.name ?? "")
        if (file && onFile) { event.currentTarget.value = ""; setChosen(""); onFile(file) }
      }} />
    </label>
    <span className="min-w-0 truncate text-meta text-muted-foreground">{chosen || (onFile ? "" : "未选择")}</span>
  </span>
}

type ToastItem = { id: number; text: string; tone: Tone; action?: { label: string; onClick: () => void } }
let toasts: ToastItem[] = []
let toastId = 0
const toastListeners = new Set<() => void>()
const emitToasts = () => { for (const listener of toastListeners) listener() }
export function dismissToast(id: number) { toasts = toasts.filter((item) => item.id !== id); emitToasts() }
/** `persist`: stays until closed, for a message the person must act on. */
export function toast(text: string, options: { tone?: Tone; action?: ToastItem["action"]; persist?: boolean } = {}) {
  const item = { id: ++toastId, text, tone: options.tone ?? "neutral", action: options.action }
  toasts = [...toasts, item].slice(-2)
  emitToasts()
  if (!options.persist) setTimeout(() => dismissToast(item.id), item.action ? 8000 : 3000)
}
const toastIcon: Record<Tone, LucideIcon> = { neutral: Info, success: CircleCheck, warning: TriangleAlert, danger: CircleAlert, info: Info }

export function Toaster() {
  const items = useSyncExternalStore((listener) => { toastListeners.add(listener); return () => { toastListeners.delete(listener) } }, () => toasts, () => toasts)
  return <div aria-live="polite" className="pointer-events-none fixed inset-x-0 top-[max(0.75rem,env(safe-area-inset-top))] z-[60] flex flex-col items-center gap-2 px-4 md:inset-x-auto md:top-auto md:right-6 md:bottom-6 md:items-end">
    {items.map((item) => {
      const Icon = toastIcon[item.tone]
      return <div key={item.id} role="status" className="pointer-events-auto flex w-full max-w-sm items-center gap-2 rounded-xl border bg-popover px-3 py-2 text-popover-foreground shadow-lg">
        <Icon aria-hidden="true" className={cn("size-4 shrink-0", toneText[item.tone])} />
        <span className="min-w-0 flex-1 text-meta">{item.text}</span>
        {item.action ? <Button type="button" size="sm" variant="ghost" onClick={() => { item.action?.onClick(); dismissToast(item.id) }}>{item.action.label}</Button> : null}
        <button type="button" aria-label="关闭" className="-my-1.5 -me-2 grid size-9 shrink-0 place-items-center rounded-md text-muted-foreground hover:text-foreground" onClick={() => dismissToast(item.id)}><X aria-hidden="true" className="size-4" /></button>
      </div>
    })}
  </div>
}

/**
 * Nearby feedback for async actions: the button shows progress, the caller renders `error` right below it.
 * Only one action of a component runs at a time, so a double click never sends a write twice.
 */
export function useAction() {
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<{ key: string; cause: unknown } | null>(null)
  const running = useRef<string | null>(null)
  const alive = useRef(true)
  useEffect(() => { alive.current = true; return () => { alive.current = false } }, [])
  const run = useCallback(async (key: string, work: () => Promise<unknown>, success?: string) => {
    if (running.current) return
    running.current = key
    setBusy(key); setError(null)
    try {
      await work()
      if (success) toast(success, { tone: "success" })
    } catch (cause) {
      if (alive.current) setError({ key, cause })
    } finally {
      running.current = null
      if (alive.current) setBusy(null)
    }
  }, [])
  return { busy, run, error: error?.cause ?? null, errorKey: error?.key ?? null, clear: () => setError(null) }
}

export function BusyIcon({ busy, icon: Icon }: { busy: boolean; icon?: LucideIcon }) {
  if (busy) return <LoaderCircle aria-hidden="true" className="animate-spin" />
  return Icon ? <Icon aria-hidden="true" /> : null
}
