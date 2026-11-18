import { BusyIcon,Details,InlineError,PageActions,Segmented } from "@/components/arc/product-kit"
import { RoutedAction } from "@/components/arc/routed-action"
import { useClipboard } from "@/lib/use-clipboard"
import { terms } from "@/lib/glossary"
import { encodeQrCode,type QrCodeResult } from "@/lib/qr-code"
import type { Invitation } from "@vibeface/client/identity/context.tsx"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { addDeviceCopy,deviceCopy,joinCopy } from "@vibeface/client/identity/copy.ts"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Drawer,DrawerContent,DrawerTrigger } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Textarea } from "@vibeface/uiarc/textarea/textarea.tsx"
import { CircleAlert,CircleCheck,Clock,Copy,Download,Laptop,Link2,Plus,Smartphone,SquareTerminal } from "lucide-react"
import { useEffect,useMemo,useRef,useState } from "react"
import { JoinRequests,useOpenJoins,type Added } from "./join"

type Share = Invitation & { qr: QrCodeResult; asManager: boolean }

function share(invitation: Invitation, asManager: boolean): Share {
  return { ...invitation, qr: encodeQrCode(invitation.link), asManager }
}

const kinds = [{ value: "phone", label: addDeviceCopy.client, icon: Smartphone }, { value: "computer", label: addDeviceCopy.node, icon: Laptop }] as const
type Kind = (typeof kinds)[number]["value"]

/**
 * "添加设备": one link the new device opens (a QR for phones, a command for computers), then both sides compare the safety code.
 * `opening`: arrived from a hint that asked for it, for that kind of device; `manage`: that hint asked for another management device.
 */
export function AddDevice({ opening, manage, onOpened, onOpenChange }: { opening?: Kind; manage?: boolean; onOpened?(): void; onOpenChange?(open: boolean): void }) {
  const device = useDevice()
  const { ready } = device
  const generation = device.connectionGeneration()
  const [open, setOpen] = useState(false)
  const opener = useRef<HTMLElement | null>(null)
  // Kept for this opening only: the hint's search is cleared once the sheet is up.
  const [{ asManager, result }, setSharing] = useState<{ asManager: boolean; result: Share | null }>({ asManager: false, result: null })
  useEffect(() => onOpenChange?.(open), [open])
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState("")
  // This page allowed the device, so it says so where the link was rather than only in a toast.
  const [added, setAdded] = useState<Added | null>(null)
  // What 链接原文 shows: the last thing someone tried to copy.
  const [shown, setShown] = useState<"link" | "command">("link")
  const [kind, setKind] = useState<Kind>("phone")
  // A join request is the next thing to act on, so it takes the only solid button.
  const joins = useOpenJoins()
  const requests = joins.length > 0
  const answered = result !== null && joins.some((request) => request.invite_id === result.inviteId)
  const [usedInviteId, setUsedInviteId] = useState<string | null>(null)
  const used = result !== null && (answered || usedInviteId === result.inviteId)
  useEffect(() => { if (answered && result) setUsedInviteId(result.inviteId) }, [answered, result])
  useEffect(() => { setSharing({ asManager: false, result: null }); setError(""); setAdded(null) }, [ready])
  // Rerender at the deadline. The displayed result owns the timer.
  const [, refreshExpiry] = useState(0)
  const expired = result !== null && Date.now() >= result.expiresAt
  const available = result !== null && !used && !expired
  useEffect(() => {
    if (!open || !result || expired) return
    const timer = setTimeout(() => refreshExpiry((tick) => tick + 1), result.expiresAt - Date.now())
    return () => clearTimeout(timer)
  }, [open, result, expired])
  const copyScope = useMemo(() => ({ ready, generation, open, result, kind, available }), [ready, generation, open, result, kind, available])
  const clipboard = useClipboard(copyScope, { isCurrent: () => available && Date.now() < result.expiresAt && ready?.isCurrent() === true && device.connectionGeneration() === generation })
  const copyPending = useRef<typeof copyScope | null>(null)
  const copied = clipboard.state.status === "copied" ? clipboard.state.key : null
  const copyFailed = clipboard.failed
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!open || !available) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [open, available, result, ready])

  // Bound to the connection generation so a late invite can't release a newer action.
  async function generate(manager = asManager) {
    if (pending.current === generation) return
    pending.current = generation
    const connection = ready
    setSharing(current => ({ ...current, result: null })); setAdded(null); setShown("link")
    setBusyGeneration(generation); setError("")
    try {
      const invitation = await device.createInvitation()
      if (connection?.isCurrent()) {
        const result = share(invitation, manager)
        setSharing(current => ({ ...current, result }))
      }
    }
    catch (cause) { if (connection?.isCurrent()) setError(messageOf(cause)) }
    finally { if (pending.current === generation) pending.current = null; setBusyGeneration((owner) => owner === generation ? null : owner) }
  }
  const copy = async (what: "link" | "command", value: string) => {
    if (!available || Date.now() >= result.expiresAt || copyPending.current === copyScope) return
    copyPending.current = copyScope
    setShown(what)
    try { await clipboard.copy(value, what) }
    finally { if (copyPending.current === copyScope) copyPending.current = null }
  }
  const raw = useRef<HTMLTextAreaElement>(null)
  useEffect(() => { if (copyFailed) { raw.current?.focus(); raw.current?.select() } }, [copyFailed, clipboard.state])
  const copyButton = (what: "link" | "command", value: string, label: string, solid: boolean) => <Button type="button" variant={solid && !requests ? "primary" : "secondary"} className="w-fit" disabled={busy || clipboard.state.status === "pending"} onClick={() => void copy(what, value)}>
    {copied === what ? <CircleCheck aria-hidden="true" /> : <Copy aria-hidden="true" />}{copied === what ? "已复制" : label}
  </Button>
  const show = (manager = asManager) => {
    // Capture before opening: programmatic search links need the same return path as the button.
    if (!open) opener.current = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null
    setOpen(true)
    if (!result && !requests) void generate(manager)
  }
  useEffect(() => {
    if (!opening || !ready?.principal.can_manage) return
    setKind(opening)
    setSharing(current => ({ ...current, asManager: Boolean(manage) }))
    show(Boolean(manage))
    onOpened?.()
  }, [opening, ready])
  if (!ready?.principal.can_manage) return null

  return <Drawer open={open} onOpenChange={(next) => { setOpen(next); if (!next) { clipboard.reset(); setAdded(null); setSharing(current => ({ ...current, asManager: false })) } }}>
    <PageActions>
      <DrawerTrigger asChild><Button type="button" variant={requests ? "secondary" : "primary"} onClick={() => show()}><Plus aria-hidden="true" />添加设备</Button></DrawerTrigger>
    </PageActions>
      {/* A waiting request leads the sheet; its first control is 设为管理设备, which should not take focus on its own. */}
      <DrawerContent title="添加设备" description="在新设备上打开链接，安全码一致后允许" className="!w-full max-sm:!border-l-0 overflow-y-auto sm:!max-w-md" onOpenAutoFocus={(event) => { if (requests) { event.preventDefault(); if (event.target instanceof HTMLElement) event.target.focus() } }} onCloseAutoFocus={(event) => {
        if (!opener.current) return
        event.preventDefault()
        const target = opener.current.isConnected ? opener.current : document.querySelector<HTMLElement>('main')
        target?.focus({ preventScroll: true })
      }}>
        <div className="grid gap-6 pb-[env(safe-area-inset-bottom)]">
          {/* A device already asking is the next thing to act on, so it comes before a new link. */}
          <JoinRequests inviteId={open ? result?.inviteId : undefined} manage={asManager && result?.asManager} onAllowed={(addedDevice, inviteId) => {
            if (!ready.isCurrent()) return
            setAdded(addedDevice)
            setSharing(current => current.result?.inviteId === inviteId ? { asManager: false, result: null } : current)
          }} />
          {added && !requests ? <div className="grid gap-3">
            <p role="status" className="flex min-w-0 items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" /><span className="truncate" title={added.name}>{added.manage ? deviceCopy.addedManager(added.name) : `已添加「${added.name}」`}</span></p>
            <div className="flex flex-wrap gap-2">
              {added.kind === "node" ? <RoutedAction to="/devices/$deviceId" params={{ deviceId: added.id }} onClick={() => setOpen(false)}><terms.ai.icon aria-hidden="true" />{addDeviceCopy.setUpAi}</RoutedAction> : null}
              {!result || (used && !expired) ? <Button type="button" variant={added.kind === "node" ? "secondary" : "primary"} className="w-fit" disabled={busy} onClick={() => void generate(false)}><BusyIcon busy={busy} icon={Plus} />{addDeviceCopy.addAnother}</Button> : null}
            </div>
          </div> : !result || (used && !requests && !expired) ? <Button type="button" variant={requests ? "secondary" : "primary"} className="w-fit" disabled={busy} onClick={() => void generate()}><BusyIcon busy={busy} icon={Link2} />生成链接</Button> : null}
          {expired ? <div className="grid gap-2">
            <p role="status" className="text-sm text-secondary">链接已过期</p>
            <Button type="button" variant={requests ? "secondary" : "primary"} className="w-fit" disabled={busy} onClick={() => void generate()}><BusyIcon busy={busy} icon={Link2} />重新生成链接</Button>
          </div> : null}
          {available ? <div className="grid gap-3">
            <Segmented label="新设备是" labels fill value={kind} options={[...kinds]} onChange={(next) => { setKind(next); clipboard.reset(); setError(""); setShown("link") }} />
            {kind === "phone" ? <>
              <p className="text-sm text-secondary">{addDeviceCopy.clientHint}</p>
              {result.qr.kind === "encoded" ? <>
                <img className="size-56 justify-self-center rounded-control bg-white p-2" src={result.qr.image} alt="用手机 App 扫码" />
                <p className="flex items-center justify-center gap-1.5 text-sm text-secondary"><Smartphone aria-hidden="true" className="size-4 shrink-0" />用手机 App 扫码</p>
              </> : <InlineError error="无法生成二维码，请复制链接。" />}
              {copyButton("link", result.link, "复制链接", true)}
            </> : <>
              <ol className="grid gap-3">
                <li className="flex min-w-0 items-center gap-3"><Step n={1} />{copyButton("link", result.link, "复制链接", true)}</li>
                <li className="flex min-w-0 items-start gap-3"><Step n={2} /><Laptop aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-secondary" /><span className="min-w-0 [overflow-wrap:anywhere]">{addDeviceCopy.openThere}</span></li>
              </ol>
              <p className="flex min-w-0 flex-wrap items-center gap-x-1.5 text-sm text-secondary">
                <Download aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.installThere}
                <a className="hit-area relative font-medium text-foreground underline underline-offset-4" href={joinCopy.installGuide} target="_blank" rel="noopener noreferrer">{addDeviceCopy.installHow}</a>
              </p>
              <div className="grid gap-2 border-t pt-3">
                <p className="flex min-w-0 items-center gap-1.5 text-sm text-secondary"><SquareTerminal aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.orCommand}</p>
                {copyButton("command", addDeviceCopy.command(result.link), addDeviceCopy.copyCommand, false)}
              </div>
            </>}
            <ul className="grid gap-1.5 text-sm text-secondary">
              <li className="flex items-center gap-1.5"><terms.safetyCode.icon aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.compare}</li>
              <li className="flex items-center gap-1.5"><Clock aria-hidden="true" className="size-4 shrink-0" />只发给自己的设备，10 分钟内有效</li>
            </ul>
            {copyFailed ? <p role="alert" className="flex items-center gap-2 text-sm text-danger"><CircleAlert aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.copyFailed}</p> : null}
            <Details key={copyFailed || result.qr.kind !== "encoded" ? "open" : "folded"} label="链接原文" open={copyFailed || result.qr.kind !== "encoded" || undefined}>
              <Textarea label="链接原文" ref={raw} readOnly aria-label="链接原文" className="font-mono" value={shown === "command" ? addDeviceCopy.command(result.link) : result.link} onFocus={(event) => event.target.select()} />
            </Details>
          </div> : null}
          <InlineError error={error || null} />
        </div>
      </DrawerContent>
  </Drawer>
}

function Step({ n }: { n: number }) {
  return <span aria-hidden="true" className="grid size-6 shrink-0 place-items-center rounded-pill bg-surface-muted text-sm font-medium tabular-nums text-secondary">{n}</span>
}
