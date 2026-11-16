"use client"

import { terms } from "@/lib/glossary"
import { cn } from "@/lib/utils"
import { rawError } from "@vibeface/client/lib/errors.ts"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { Alert } from "@vibeface/uiarc/alert/alert.tsx"
import { Badge } from "@vibeface/uiarc/badge/badge.tsx"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { EmptyState as ArcEmptyState } from "@vibeface/uiarc/empty-state/empty-state.tsx"
import { Input } from "@vibeface/uiarc/input/input.tsx"
import SegmentedControl from "@vibeface/uiarc/segmented-control/segmented-control.tsx"
import { Skeleton as ArcSkeleton } from "@vibeface/uiarc/skeleton/skeleton.tsx"
import Stepper from "@vibeface/uiarc/stepper/stepper.tsx"
import { ChevronDown,ChevronRight,CircleAlert,CircleCheck,Eye,EyeOff,Info,LoaderCircle,TriangleAlert,X,type LucideIcon } from "lucide-react"
import { createContext,useCallback,useContext,useEffect,useRef,useState,useSyncExternalStore,type ComponentProps,type CSSProperties,type ReactNode,type Ref } from "react"
import { createPortal } from "react-dom"
import styles from "./product-kit.module.css"

export type Tone = "neutral" | "success" | "warning" | "danger" | "info"
export const toneText: Record<Tone, string> = { neutral: "text-secondary", success: "text-success", warning: "text-warning", danger: "text-danger", info: "text-info" }

export function Wordmark({ className, large = false }: { className?: string; large?: boolean }) {
  return <span className={cn(styles.wordmark, className)} data-large={large || undefined}>Vibeface</span>
}
export function PrimaryMark({ labeled = false }: { labeled?: boolean }) {
  const { label, icon: Icon } = terms.primaryBot
  return <span role={labeled ? undefined : "img"} aria-label={labeled ? undefined : label} title={label} className="inline-flex shrink-0 items-center gap-1 text-sm text-secondary"><Icon aria-hidden="true" className="size-4" />{labeled ? label : null}</span>
}
export function CountBadge({ count, label, className }: { count: number; label?: string; className?: string }) {
  return <Badge tone="info" aria-label={label} className={cn("tabular-nums", className)}>{count}</Badge>
}
export function Dot({ tone = "brand", label, className }: { tone?: "brand" | "warning"; label?: string; className?: string }) {
  return <span role={label ? "img" : undefined} aria-label={label} className={cn("size-2 shrink-0 rounded-pill", tone === "brand" ? "bg-accent" : "bg-warning", className)} />
}
export const PageActionsSlot = createContext<HTMLElement | null>(null)
export function PageActions({ children }: { children: ReactNode }) { const slot = useContext(PageActionsSlot); return slot ? createPortal(children, slot) : null }
export function EmptyState({ icon: Icon, title, description = "", action, className }: { icon: LucideIcon; title: string; description?: string; action?: ReactNode; className?: string }) {
  return <ArcEmptyState icon={<Icon aria-hidden="true" />} title={title} description={description} action={action} className={className} />
}
export function Steps({ step }: { step: number }) {
  return <Stepper current={step - 1} compact label={`第 ${step} 步，共 5 步`} steps={[1,2,3,4,5].map(value => ({ id: String(value), label: `第 ${value} 步` }))} />
}
export function AuthPage({ header, children }: { header?: ReactNode; children: ReactNode }) {
  return <main className={styles.auth}><div className={styles.authBody}><header><Wordmark large />{header}</header>{children}</div></main>
}
export function claimFocus(node: HTMLElement | null) {
  const active = node?.ownerDocument.activeElement
  if (node && (!active || active === node.ownerDocument.body)) node.focus({ preventScroll: true })
}
export function Panel({ icon: Icon, title, busy = false, children }: { icon: LucideIcon; title: string; busy?: boolean; children: ReactNode }) {
  return <section className={styles.panel} aria-busy={busy || undefined}><h2 ref={claimFocus} tabIndex={-1} className={styles.panelTitle}><Icon aria-hidden="true" className={cn("size-5", busy && "animate-spin")} />{title}</h2>{children}</section>
}
export function SettingsGroup({ title, footer, children, className }: { title?: string; footer?: ReactNode; children: ReactNode; className?: string }) {
  return <section className={cn(styles.group, className)}>{title ? <h2 className={styles.groupTitle}>{title}</h2> : null}<div className={styles.groupBody}>{children}</div>{footer ? <p className={styles.note}>{footer}</p> : null}</section>
}
function RowLead({ icon: Icon, label, description, tone = "neutral" }: { icon: LucideIcon; label: ReactNode; description?: ReactNode; tone?: "neutral" | "success" | "danger" }) {
  return <><Icon aria-hidden="true" className={cn(styles.rowIcon, toneText[tone])} /><span className={styles.rowLead}><span className={tone === "danger" ? "text-danger" : undefined}>{label}</span>{description ? <span className={styles.note}>{description}</span> : null}</span></>
}
export function SettingsRow({ id, icon, label, description, tone, children, onClick, expanded, className, stacked = false, ref }: {
  id?: string; icon: LucideIcon; label: ReactNode; description?: ReactNode; tone?: "neutral" | "success" | "danger"; children?: ReactNode; onClick?: () => void; expanded?: boolean; className?: string; stacked?: boolean; ref?: Ref<HTMLButtonElement>
}) {
  const content = <><RowLead icon={icon} label={label} description={description} tone={tone} />{children ? <span className={styles.rowEnd} data-stacked={stacked || undefined}>{children}</span> : null}{expanded === undefined ? null : <ChevronDown aria-hidden="true" className={cn("size-4 shrink-0 text-secondary", expanded && "rotate-180")} />}</>
  return onClick ? <button id={id} ref={ref} type="button" aria-expanded={expanded} onClick={onClick} className={cn(styles.row, className)}>{content}</button> : <div id={id} className={cn(styles.row, className)}>{content}</div>
}
export function SettingsDisclosure({ icon, label, value, open, children }: { icon: LucideIcon; label: ReactNode; value?: ReactNode; open?: boolean; children: ReactNode }) {
  return <details open={open} className={styles.disclosure}><summary className={styles.row}><RowLead icon={icon} label={label} />{value ? <span className={styles.rowEnd}>{value}</span> : null}<ChevronDown aria-hidden="true" className="size-4 shrink-0 text-secondary" /></summary><div className={styles.disclosureBody}>{children}</div></details>
}
export function Segmented<T extends string>({ label, value, options, onChange, disabled }: {
  label: string; value: T; options: Array<{ value: T; label: string; icon?: LucideIcon }>; onChange(value: T): void; disabled?: boolean; labels?: boolean; fill?: boolean
}) {
  return <div aria-disabled={disabled || undefined}><SegmentedControl label={label} value={value} options={options} onValueChange={next => { const option = options.find(option => option.value === next); if (!disabled && option) onChange(option.value) }} /></div>
}
export function SegmentedValue<T extends string>({ value, options }: { value: T; options: Array<{ value: T; label: string }> }) { return <span className="sr-only">{options.find(option => option.value === value)?.label}</span> }
export function Skeleton({ className }: { className?: string }) { return <ArcSkeleton label="正在读取" lines={1} className={className} /> }
export function InlineError({ error, action, className }: { error: unknown; action?: { label: string; onClick: () => void }; className?: string }) {
  if (!error) return null
  const raw = rawError(error)
  return <div className={cn("grid min-w-0 gap-2", className)}><Alert tone="danger" title={messageOf(error)} />{action ? <Button type="button" variant="secondary" className="justify-self-start" onClick={action.onClick}>{action.label}</Button> : null}{raw ? <Details><code>{raw}</code></Details> : null}</div>
}
export function StatusPill({ tone = "neutral", icon: Icon, children, className }: { tone?: Tone; icon?: LucideIcon; children: ReactNode; className?: string }) {
  return <Badge tone={tone} icon={Icon ? <Icon aria-hidden="true" /> : undefined} className={className}>{children}</Badge>
}
export function ActionCard({ icon: Icon, tone = "warning", title, meta, trailing, children, actions, className }: {
  icon: LucideIcon; tone?: Tone; title: ReactNode; meta?: ReactNode; trailing?: ReactNode; children?: ReactNode; actions?: ReactNode; className?: string
}) {
  return <section className={cn(styles.action, className)} data-tone={tone}><header className={styles.actionHeader}><Icon aria-hidden="true" className={toneText[tone]} /><h3>{title}</h3>{meta ? <span className={styles.actionMeta}>{meta}</span> : null}{trailing}</header>{children ? <div className={styles.actionBody}>{children}</div> : null}{actions ? <div className={styles.actionFooter}>{actions}</div> : null}</section>
}
export function SafetyCode({ code, label = "安全码" }: { code: string; label?: string }) {
  const groups = code.match(code.length === 6 ? /.{3}/g : /.{1,4}/g) ?? []
  return <output aria-label={`${label} ${code.split("").join(" ")}`} className={styles.safety}>{groups.map((group,index) => <span key={index}>{group}</span>)}</output>
}
export function WaitingDot() { return <span aria-hidden="true" className="size-2 shrink-0 rounded-pill bg-info" /> }
export function Details({ children, label = "详细信息", className, open }: { children: ReactNode; label?: string; className?: string; open?: boolean }) {
  return <details open={open} className={cn(styles.details, className)}><summary><ChevronRight aria-hidden="true" className="size-4" />{label}</summary><div className={styles.detailsBody}>{children}</div></details>
}
/** Long hex ids show their first 10 characters; a click reveals the rest. */
function LongId({ value }: { value: string }) {
  const [full, setFull] = useState(false)
  return <Button type="button" variant="ghost" className="w-fit text-left font-mono break-all" title={full ? undefined : value} aria-expanded={full} onClick={() => setFull(!full)}>{full ? value : `${value.slice(0, 10)}…`}</Button>
}

export function DetailRow({ label, value, mono = false }: { label: string; value: ReactNode; mono?: boolean }) {
  return <div className="grid gap-0.5"><span>{label}</span>{typeof value === "string" && /^[0-9a-f]{20,}$/.test(value) ? <LongId value={value} /> : <span className={cn("text-foreground", mono && "font-mono")}>{value}</span>}</div>
}

/** A long path gives up its parent folders first, so the folder's own name stays readable. `shown`: its `~/…` form. */
export function FolderPath({ path, shown = path }: { path: string; shown?: string }) {
  const cut = shown.replace(/[\\/]+$/, "").search(/[^\\/]*$/)
  return <span className="flex min-w-0 flex-1 font-mono text-sm" title={path}>
    <span className="min-w-0 truncate text-secondary">{shown.slice(0, cut)}</span><span className="max-w-full shrink-0 truncate">{shown.slice(cut)}</span>
  </span>
}


export function PasswordInput({ className, ...props }: Omit<ComponentProps<typeof Input>, "type">) {
  const [shown,setShown] = useState(false)
  return <div className={styles.password}><Input {...props} type={shown ? "text" : "password"} className={className} /><Button type="button" variant="ghost" aria-label={shown ? "隐藏密码" : "显示密码"} aria-pressed={shown} onClick={() => setShown(!shown)}>{shown ? <EyeOff aria-hidden="true" /> : <Eye aria-hidden="true" />}</Button></div>
}
export function FilePicker({ name, accept, required, disabled, label = "选择文件", onFile }: { name?: string; accept?: string; required?: boolean; disabled?: boolean; label?: string; onFile?(file: File): void }) {
  const [chosen,setChosen] = useState("")
  return <div className={styles.picker}><Input label={label} type="file" name={name} accept={accept} required={required} disabled={disabled} onChange={event => { const file = event.currentTarget.files?.item(0); setChosen(file?.name ?? ""); if (file && onFile) { event.currentTarget.value = ""; setChosen(""); onFile(file) } }} /><span className="min-w-0 break-words text-sm text-secondary">{chosen || (onFile ? "" : "未选择")}</span></div>
}
type ToastItem = { id: number; text: string; tone: Tone; icon?: LucideIcon; action?: { label: string; onClick: () => void } }
let toasts: ToastItem[] = []
let toastId = 0
const toastListeners = new Set<() => void>()
const emitToasts = () => { for (const listener of toastListeners) listener() }
function dismissToast(id: number) { toasts = toasts.filter(item => item.id !== id); emitToasts() }
export function toast(text: string, options: { tone?: Tone; icon?: LucideIcon; action?: ToastItem["action"]; persist?: boolean } = {}) {
  const item = { id: ++toastId, text, tone: options.tone ?? "neutral", icon: options.icon, action: options.action }
  toasts = [...toasts,item].slice(-2); emitToasts()
  if (!options.persist) setTimeout(() => dismissToast(item.id),item.action ? 8000 : 3000)
}
const toastIcon: Record<Tone, LucideIcon> = { neutral: Info, success: CircleCheck, warning: TriangleAlert, danger: CircleAlert, info: Info }
export function Toaster() {
  const items = useSyncExternalStore(listener => { toastListeners.add(listener); return () => { toastListeners.delete(listener) } },() => toasts,() => toasts)
  const [space,setSpace] = useState(0)
  const stack = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!items.length || typeof document === "undefined") return
    const root = document.documentElement
    let anchor: HTMLElement | undefined
    const resize = new ResizeObserver(update)
    function update() {
      const tops = anchor ? [anchor,...anchor.querySelectorAll<HTMLElement>("[data-toast-above]")].map(node => node.getBoundingClientRect()).filter(rect => rect.height > 0).map(rect => rect.top) : []
      const next = tops.length ? Math.max(0,window.innerHeight - Math.min(...tops)) : 0
      setSpace(next)
      const above = anchor ? next - (window.innerHeight - anchor.getBoundingClientRect().top) : 0
      root.style.setProperty("--toast-cover",anchor && stack.current ? `${stack.current.offsetHeight + 12 + above}px` : "0px")
    }
    function measure() {
      const dialog = [...document.querySelectorAll<HTMLElement>("[role=dialog][data-state=open]")].at(-1)
      const next = [...(dialog ?? document).querySelectorAll<HTMLElement>("[data-toast-anchor]")].find(node => !node.closest("[inert]") && node.getBoundingClientRect().height > 0)
      if (next !== anchor) {
        anchor = next
        resize.disconnect()
        if (stack.current) resize.observe(stack.current)
        if (anchor) { resize.observe(anchor); for (const floating of anchor.querySelectorAll("[data-toast-above]")) resize.observe(floating) }
      }
      update()
    }
    measure()
    if (stack.current) resize.observe(stack.current)
    const dialogs = new MutationObserver(measure)
    dialogs.observe(document.body,{ childList: true, subtree: true, attributes: true, attributeFilter: ["data-state"] })
    window.addEventListener("resize",measure)
    return () => { resize.disconnect(); dialogs.disconnect(); window.removeEventListener("resize",measure); root.style.removeProperty("--toast-cover") }
  },[items])
  return <div ref={stack} style={{ "--toast-space": `${space}px` } as CSSProperties} className={styles.toastStack} aria-live="polite">{items.map(item => { const Icon = item.icon ?? toastIcon[item.tone]; return <div key={item.id} className={styles.toast} role="status"><div className={styles.toastHeader}><Icon aria-hidden="true" className={cn("size-4 shrink-0",toneText[item.tone])} /><span>{item.text}</span><Button type="button" variant="ghost" aria-label="关闭" onClick={() => dismissToast(item.id)}><X aria-hidden="true" className="size-4" /></Button></div>{item.action ? <Button type="button" variant="secondary" onClick={() => { item.action?.onClick(); dismissToast(item.id) }}>{item.action.label}</Button> : null}</div> })}</div>
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
