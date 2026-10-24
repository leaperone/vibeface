"use client"

import { createContext, useCallback, useContext, useEffect, useRef, useState, useSyncExternalStore, type CSSProperties, type ReactNode } from "react"
import { createPortal } from "react-dom"
import { Bot, ChevronRight, CircleAlert, CircleCheck, Info, LoaderCircle, TriangleAlert, Upload, X, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { Button } from "@/components/ui/button"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { rawError } from "@vibeface/client/lib/errors.ts"

export type Tone = "neutral" | "success" | "warning" | "danger" | "info"

const toneText: Record<Tone, string> = {
  neutral: "text-muted-foreground", success: "text-success", warning: "text-warning", danger: "text-danger", info: "text-info",
}
const toneSurface: Record<Tone, string> = {
  neutral: "border-border bg-card", success: "border-success-border bg-success-soft", warning: "border-warning-border bg-warning-soft",
  danger: "border-danger-border bg-danger-soft", info: "border-info-border bg-info-soft",
}
/** A card keeps the page's surface; the tone washes in from its top edge and tints the border. */
const toneCard: Record<Tone, string> = {
  neutral: "border-border bg-card", success: "border-success-border bg-linear-to-b from-success-soft to-card to-[5rem]",
  warning: "border-warning-border bg-linear-to-b from-warning-soft to-card to-[5rem]", danger: "border-danger-border bg-linear-to-b from-danger-soft to-card to-[5rem]",
  info: "border-info-border bg-linear-to-b from-info-soft to-card to-[5rem]",
}
const toneChip: Record<Tone, string> = {
  neutral: "bg-muted text-muted-foreground", success: "bg-success-soft text-success ring-1 ring-success-border", warning: "bg-warning-soft text-warning ring-1 ring-warning-border",
  danger: "bg-danger-soft text-danger ring-1 ring-danger-border", info: "bg-info-soft text-info ring-1 ring-info-border",
}

/** The Vibeface "V" in its rounded tile, as on the app icon. */
export function BrandMark({ className }: { className?: string }) {
  return <svg viewBox="0 0 64 64" aria-hidden="true" className={cn("size-6 shrink-0", className)}>
    <rect width="64" height="64" rx="16" className="fill-foreground" />
    <path d="M19 21l13 24 13-24" fill="none" strokeWidth="6.5" strokeLinecap="round" strokeLinejoin="round" className="stroke-background" />
  </svg>
}

export function Wordmark({ className }: { className?: string }) {
  return <span className={cn("inline-flex items-center gap-2 text-heading font-semibold tracking-tight", className)}><BrandMark />Vibeface</span>
}

/** Each bot keeps one hue, picked from its id, so the same bot looks the same in the list, the header and the chat. */
export function BotAvatar({ id, name, size = "md", className }: { id: string; name: string; size?: "sm" | "md" | "lg"; className?: string }) {
  let hash = 2166136261
  for (const char of id) hash = Math.imul(hash ^ char.charCodeAt(0), 16777619) >>> 0
  const hue = Math.round((hash * 0.618033988749895 % 1) * 360)
  // "我的" says whose bot it is, not which; a name left with only "Bot" shows the bot icon.
  const rest = name.trim().replace(/^我的\s*/, "")
  const letter = /^(bot)?$/i.test(rest) ? <Bot className="size-[1.2em]" /> : [...rest][0].toUpperCase()
  return <span aria-hidden="true" style={{ "--avatar-hue": hue } as CSSProperties} className={cn("bot-avatar grid shrink-0 place-items-center font-semibold select-none",
    size === "sm" ? "size-5 rounded-md text-[11px]" : size === "md" ? "size-8 rounded-lg text-meta" : "size-12 rounded-2xl text-title", className)}>{letter}</span>
}

/** Unread or waiting count, in the brand violet. */
export function CountBadge({ count, label, className }: { count: number; label?: string; className?: string }) {
  return <span aria-label={label} className={cn("grid h-5 min-w-5 shrink-0 place-items-center rounded-full bg-brand px-1.5 text-[11px] leading-none font-semibold text-brand-foreground tabular-nums", className)}>{count}</span>
}

/** Brand for news; warning for something that needs a look. */
export function Dot({ tone = "brand", label, className }: { tone?: "brand" | "warning"; label?: string; className?: string }) {
  return <span aria-label={label} className={cn("size-2 shrink-0 rounded-full", tone === "brand" ? "bg-brand" : "bg-warning", className)} />
}

/** The header's trailing slot, set by the shell. */
export const PageActionsSlot = createContext<HTMLElement | null>(null)
/** Puts a page's own actions on the right of the page header, next to the shell's buttons. */
export function PageActions({ children }: { children: ReactNode }) {
  const slot = useContext(PageActionsSlot)
  return slot ? createPortal(children, slot) : null
}

export function EmptyState({ icon: Icon, title, description, action, className }: { icon: LucideIcon; title: string; description?: string; action?: ReactNode; className?: string }) {
  return <div className={cn("mx-auto flex max-w-sm flex-col items-center gap-3 px-4 py-12 text-center animate-in fade-in-0 duration-200 ease-out", className)}>
    <span className="mb-1 grid size-12 place-items-center rounded-2xl bg-muted text-muted-foreground ring-1 ring-border"><Icon aria-hidden="true" className="size-6" /></span>
    <h2 className="text-heading font-semibold">{title}</h2>
    {description ? <p className="text-meta text-muted-foreground">{description}</p> : null}
    {action ? <div className="pt-1">{action}</div> : null}
  </div>
}

/** Progress through first-time setup, shared by the connection screen and onboarding. */
export function Steps({ step }: { step: number }) {
  return <div className="flex items-center gap-3 text-meta text-muted-foreground">
    <span className="flex gap-1" aria-hidden="true">{[1, 2, 3, 4].map((item) => <span key={item} className={cn("h-1 w-6 rounded-full transition-colors duration-200", item <= step ? "bg-brand" : "bg-border")} />)}</span>
    <span aria-hidden="true" className="tabular-nums">{step}/4</span>
    <span className="sr-only">第 {step} 步，共 4 步</span>
  </div>
}

/**
 * The page around connecting, setup and recovery. On wider screens the card sits on a tinted page at a fixed height,
 * so the brand and step bar stay put while cards of different heights follow one another.
 */
export function AuthPage({ header, children }: { header?: ReactNode; children: ReactNode }) {
  return <main className="h-[var(--app-height,100dvh)] overflow-y-auto overscroll-contain sm:bg-sidebar">
    <div className="mx-auto grid w-full max-w-md content-start gap-6 px-4 pt-10 pb-[max(2.5rem,env(safe-area-inset-bottom))] sm:pt-[min(14vh,8rem)] sm:pb-16">
      <header className="grid gap-3"><Wordmark />{header}</header>
      {children}
    </div>
  </main>
}

/** One step of first-time setup: a raised card under an icon title. */
export function Panel({ icon: Icon, title, busy = false, children }: { icon: LucideIcon; title: string; busy?: boolean; children: ReactNode }) {
  // A phone has no room for a frame inside the page's own margins, so there the card is only its content.
  return <section className="grid gap-4 animate-in fade-in-0 slide-in-from-bottom-1 duration-200 ease-(--ease-out-quint) sm:rounded-2xl sm:border sm:bg-card sm:p-6 sm:shadow-sm">
    <h2 className="flex items-center gap-3 text-title font-semibold"><span className="grid size-9 shrink-0 place-items-center rounded-xl bg-muted text-muted-foreground ring-1 ring-border"><Icon aria-hidden="true" className={cn("size-[1.125rem]", busy && "animate-spin")} /></span>{title}</h2>
    {children}
  </section>
}

/** A settings section: a short caption over one card of rows, with an optional note under it. */
export function SettingsGroup({ title, footer, children, className }: { title?: string; footer?: ReactNode; children: ReactNode; className?: string }) {
  return <section className={cn("grid gap-2", className)}>
    {title ? <h2 className="px-1 text-meta font-medium text-muted-foreground">{title}</h2> : null}
    <div className="divide-y overflow-hidden rounded-xl border bg-card shadow-xs">{children}</div>
    {footer ? <p className="px-1 text-meta text-muted-foreground">{footer}</p> : null}
  </section>
}

const rowTile: Record<"neutral" | "success" | "danger", string> = { neutral: "bg-muted text-muted-foreground", success: "bg-success-soft text-success", danger: "bg-danger-soft text-danger" }
const rowLayout = "flex min-h-14 w-full min-w-0 flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 text-start"

function RowLead({ icon: Icon, label, description, tone = "neutral" }: { icon: LucideIcon; label: ReactNode; description?: ReactNode; tone?: keyof typeof rowTile }) {
  return <>
    <span className={cn("grid size-8 shrink-0 place-items-center rounded-lg", rowTile[tone])}><Icon aria-hidden="true" className="size-4" /></span>
    <span className="grid max-w-fit min-w-0 flex-1 basis-28">
      <span className={cn("truncate", tone === "danger" && "text-danger")}>{label}</span>
      {description ? <span className="text-meta text-muted-foreground [overflow-wrap:anywhere]">{description}</span> : null}
    </span>
  </>
}

/** One settings row: icon, label, then its value or control at the end. With `onClick` the whole row is a button. */
export function SettingsRow({ icon, label, description, tone, children, onClick, expanded, className }: {
  icon: LucideIcon; label: ReactNode; description?: ReactNode; tone?: keyof typeof rowTile; children?: ReactNode; onClick?: () => void; expanded?: boolean; className?: string
}) {
  const end = children ? <span className="ms-auto flex max-w-full shrink-0 flex-wrap items-center justify-end gap-2 text-meta text-muted-foreground">{children}</span> : null
  if (onClick) return <button type="button" aria-expanded={expanded} onClick={onClick} className={cn(rowLayout, "interactive-row", className)}>
    <RowLead icon={icon} label={label} description={description} tone={tone} />{end}
    <ChevronRight aria-hidden="true" className={cn("size-4 shrink-0 text-muted-foreground transition-transform duration-200", !end && "ms-auto", expanded && "rotate-90")} />
  </button>
  return <div className={cn(rowLayout, className)}><RowLead icon={icon} label={label} description={description} tone={tone} />{end}</div>
}

/** A row that folds out what it holds below itself, inside the same card. */
export function SettingsDisclosure({ icon, label, value, open, children }: { icon: LucideIcon; label: ReactNode; value?: ReactNode; open?: boolean; children: ReactNode }) {
  return <details open={open} className="group/disclosure">
    <summary className={cn(rowLayout, "interactive-row cursor-pointer list-none flex-nowrap [&::-webkit-details-marker]:hidden")}>
      <RowLead icon={icon} label={label} />
      {value ? <span className="ms-auto shrink-0 text-meta text-muted-foreground">{value}</span> : null}
      <ChevronRight aria-hidden="true" className={cn("size-4 shrink-0 text-muted-foreground transition-transform duration-200 group-open/disclosure:rotate-90", !value && "ms-auto")} />
    </summary>
    <div className="grid gap-3 px-4 pb-4 sm:ps-15">{children}</div>
  </details>
}

/**
 * A short set of mutually exclusive options on a muted track.
 * On a phone, options with icons show only the icon; pair it with `SegmentedValue` so the choice still reads in words.
 * `labels` keeps the words on a phone too, for options an icon alone can't explain.
 */
export function Segmented<T extends string>({ label, value, options, onChange, disabled, labels = false }: {
  label: string; value: T; options: Array<{ value: T; label: string; icon?: LucideIcon }>; onChange(value: T): void; disabled?: boolean; labels?: boolean
}) {
  return <div role="radiogroup" aria-label={label} className="inline-flex max-w-full flex-wrap gap-0.5 rounded-lg bg-muted p-0.5">
    {options.map(({ value: option, label: text, icon: Icon }) => <button key={option} type="button" role="radio" aria-checked={value === option} aria-label={Icon && !labels ? text : undefined} title={Icon && !labels ? text : undefined} disabled={disabled}
      onClick={() => { if (option !== value) onChange(option) }}
      className={cn("inline-flex h-8 items-center justify-center gap-1.5 rounded-md px-2.5 text-meta font-medium whitespace-nowrap transition-[color,background-color,box-shadow] duration-150 outline-none focus-visible:ring-3 focus-visible:ring-ring/50 active:bg-card/70 disabled:opacity-50 max-md:h-10 max-md:px-3",
        Icon && !labels && "max-sm:w-10 max-sm:px-0",
        value === option ? "bg-card text-foreground shadow-xs ring-1 ring-border dark:bg-accent" : "text-muted-foreground hover:text-foreground")}>
      {Icon ? <><Icon aria-hidden="true" className="size-4" /><span className={labels ? undefined : "max-sm:hidden"}>{text}</span></> : text}
    </button>)}
  </div>
}

/** The chosen option of a `Segmented` in words, shown only where the control shrinks to icons. */
export function SegmentedValue<T extends string>({ value, options }: { value: T; options: Array<{ value: T; label: string }> }) {
  return <span aria-hidden="true" className="sm:hidden">{options.find((option) => option.value === value)?.label}</span>
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

/**
 * Shared by approvals, join requests and confirmations, in chat and in /inbox.
 * `trailing`: a card with a single action keeps it on the title row instead of a footer of its own.
 */
export function ActionCard({ icon: Icon, tone = "warning", title, meta, trailing, children, actions, className }: {
  icon: LucideIcon; tone?: Tone; title: ReactNode; meta?: ReactNode; trailing?: ReactNode; children?: ReactNode; actions?: ReactNode; className?: string
}) {
  return <section className={cn("grid gap-3 rounded-xl border p-4 shadow-xs", toneCard[tone], className)}>
    <header className="flex min-w-0 items-center gap-2.5">
      <span className={cn("grid size-7 shrink-0 place-items-center rounded-lg", toneChip[tone])}><Icon aria-hidden="true" className="size-4" /></span>
      <h3 className="min-w-0 flex-1 text-heading font-semibold">{title}</h3>
      {meta ? <span className="max-w-[40%] shrink-0 truncate text-meta text-muted-foreground">{meta}</span> : null}
      {trailing ? <div className="-my-1 shrink-0">{trailing}</div> : null}
    </header>
    {children ? <div className="grid min-w-0 gap-2">{children}</div> : null}
    {actions ? <div className="flex flex-wrap justify-end gap-2">{actions}</div> : null}
  </section>
}

export function SafetyCode({ code, label = "安全码" }: { code: string; label?: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <output aria-label={`${label} ${code.split("").join(" ")}`} className="flex flex-wrap justify-center gap-x-5 rounded-xl bg-muted/60 px-4 py-3 font-mono text-3xl font-semibold tracking-[0.12em] tabular-nums ring-1 ring-border">
    {groups.map((group, index) => <span key={index}>{group}</span>)}
  </output>
}

/** Waiting on another device: a soft brand ping, a still dot under reduced motion. */
export function WaitingDot() {
  return <span aria-hidden="true" className="relative grid size-4 shrink-0 place-items-center">
    <span className="absolute size-2 animate-ping rounded-full bg-brand/60 motion-reduce:hidden" />
    <span className="size-2 rounded-full bg-brand" />
  </span>
}

/** Collapsed by default. Jargon from docs/design/web-ux.md section 6 may only appear in here. */
export function Details({ children, label = "详细信息", className, open }: { children: ReactNode; label?: string; className?: string; open?: boolean }) {
  return <details open={open} className={cn("details text-meta", className)}>
    <summary className="hit-area relative inline-flex min-h-9 items-center gap-1 text-muted-foreground hover:text-foreground">
      <ChevronRight aria-hidden="true" className="size-4 transition-transform duration-150" />{label}
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

type ToastItem = { id: number; text: string; tone: Tone; icon?: LucideIcon; action?: { label: string; onClick: () => void } }
let toasts: ToastItem[] = []
let toastId = 0
const toastListeners = new Set<() => void>()
const emitToasts = () => { for (const listener of toastListeners) listener() }
export function dismissToast(id: number) { toasts = toasts.filter((item) => item.id !== id); emitToasts() }
/** `persist`: stays until closed, for a message the person must act on. Returns the id for `dismissToast`. */
export function toast(text: string, options: { tone?: Tone; icon?: LucideIcon; action?: ToastItem["action"]; persist?: boolean } = {}) {
  const item = { id: ++toastId, text, tone: options.tone ?? "neutral", icon: options.icon, action: options.action }
  toasts = [...toasts, item].slice(-2)
  emitToasts()
  if (!options.persist) setTimeout(() => dismissToast(item.id), item.action ? 8000 : 3000)
  return item.id
}
const toastIcon: Record<Tone, LucideIcon> = { neutral: Info, success: CircleCheck, warning: TriangleAlert, danger: CircleAlert, info: Info }

/**
 * The visible `[data-toast-anchor]` (the composer), if any. A `[data-toast-above]` inside it floats over the page and counts too.
 * While a dialog or sheet is open, only an anchor inside it counts: the composer behind it is covered.
 */
const toastAnchor = () => {
  const dialog = [...document.querySelectorAll<HTMLElement>("[role=dialog][data-state=open]")].at(-1)
  return [...(dialog ?? document).querySelectorAll<HTMLElement>("[data-toast-anchor]")].find((anchor) => !anchor.closest("[inert]") && anchor.getBoundingClientRect().height > 0)
}
function anchorSpace(anchor: HTMLElement | undefined) {
  if (!anchor) return 0
  const tops = [anchor, ...anchor.querySelectorAll<HTMLElement>("[data-toast-above]")].map((element) => element.getBoundingClientRect()).filter((rect) => rect.height > 0).map((rect) => rect.top)
  return Math.max(0, window.innerHeight - Math.min(...tops))
}

/** Toasts rise from the bottom edge, above the composer when one is on screen, so they never cover the header or the first card. */
export function Toaster() {
  const items = useSyncExternalStore((listener) => { toastListeners.add(listener); return () => { toastListeners.delete(listener) } }, () => toasts, () => toasts)
  const [space, setSpace] = useState(0)
  useEffect(() => {
    if (!items.length || typeof window === "undefined") return
    let anchor: HTMLElement | undefined
    const resize = new ResizeObserver(() => setSpace(anchorSpace(anchor)))
    // A sheet opening or closing changes which anchor counts, so the anchor is found again each time.
    function measure() {
      const next = toastAnchor()
      if (next !== anchor) {
        anchor = next
        resize.disconnect()
        for (const element of anchor ? [anchor, ...anchor.querySelectorAll("[data-toast-above]")] : []) resize.observe(element)
      }
      setSpace(anchorSpace(anchor))
    }
    measure()
    const dialogs = new MutationObserver(measure)
    dialogs.observe(document.body, { childList: true, subtree: true, attributeFilter: ["data-state"] })
    window.addEventListener("resize", measure)
    return () => { resize.disconnect(); dialogs.disconnect(); window.removeEventListener("resize", measure) }
  }, [items])
  return <div aria-live="polite" style={{ "--toast-space": `${space}px` } as CSSProperties} className="pointer-events-none fixed inset-x-0 bottom-[max(calc(var(--toast-space)+0.5rem),calc(env(safe-area-inset-bottom)+1rem))] z-[60] flex flex-col items-center gap-2 px-4 md:inset-x-auto md:right-6 md:items-end">
    {items.map((item) => {
      const Icon = item.icon ?? toastIcon[item.tone]
      return <div key={item.id} role="status" className="pointer-events-auto flex w-full max-w-sm items-center gap-2 rounded-xl border bg-popover px-3 py-2 text-popover-foreground shadow-lg animate-in fade-in-0 slide-in-from-bottom-2 zoom-in-[0.98] duration-200 ease-(--ease-out-quint)">
        <Icon aria-hidden="true" className={cn("size-4 shrink-0", toneText[item.tone])} />
        <span className="min-w-0 flex-1 text-meta">{item.text}</span>
        {item.action ? <Button type="button" size="sm" variant="ghost" className="-my-2 h-11 px-3 md:my-0 md:h-7 md:px-2.5" onClick={() => { item.action?.onClick(); dismissToast(item.id) }}>{item.action.label}</Button> : null}
        <button type="button" aria-label="关闭" className="-my-2 -me-2 grid size-11 shrink-0 md:-my-1.5 md:size-9 place-items-center rounded-md text-muted-foreground hover:text-foreground" onClick={() => dismissToast(item.id)}><X aria-hidden="true" className="size-4" /></button>
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
