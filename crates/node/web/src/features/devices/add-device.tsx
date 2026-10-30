import { useEffect, useRef, useState } from "react"
import { CircleAlert, CircleCheck, Clock, Copy, Download, Link2, Monitor, Plus, Smartphone, SquareTerminal } from "lucide-react"
import qrcode from "qrcode-generator"
import { BusyIcon, Details, InlineError, PageActions, Segmented } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Textarea } from "@/components/ui/textarea"
import { terms } from "@/lib/glossary"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import type { Invitation } from "@vibeface/client/identity/context.tsx"
import { addDeviceCopy, deviceCopy, joinCopy } from "@vibeface/client/identity/copy.ts"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { JoinRequests, useOpenJoins } from "./join"

/** `image`: the QR code of `qr`, which a phone's camera opens when the link is public, and the Vibeface app scans otherwise. */
type Share = Invitation & { image: string | null }

function share(invitation: Invitation): Share {
  let image: string | null = null
  try { const code = qrcode(0, "M"); code.addData(invitation.qr); code.make(); image = code.createDataURL(4, 16) } catch {}
  return { ...invitation, image }
}

const kinds = [{ value: "phone", label: addDeviceCopy.phone, icon: Smartphone }, { value: "computer", label: addDeviceCopy.computer, icon: Monitor }] as const
type Kind = (typeof kinds)[number]["value"]

/** "添加设备": one link the new device opens (a QR for phones, a command for computers), then both sides compare the safety code. `opening`: arrived from a hint that asked for it, for that kind of device. */
export function AddDevice({ opening, onOpened, onOpenChange }: { opening?: Kind; onOpened?(): void; onOpenChange?(open: boolean): void }) {
  const device = useDevice()
  const { ready } = device
  const generation = device.connectionGeneration()
  const [open, setOpen] = useState(false)
  useEffect(() => onOpenChange?.(open), [open])
  const [result, setResult] = useState<Share | null>(null)
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState("")
  const [status, setStatus] = useState("")
  // This page allowed the device, so it says so where the link was rather than only in a toast.
  const [added, setAdded] = useState<{ name: string; manage: boolean } | null>(null)
  // "已复制" shows for a moment on the button that copied.
  const [copied, setCopied] = useState<"link" | "command" | null>(null)
  // The browser refused the clipboard: the raw text opens, selected, for a manual copy.
  const [copyFailed, setCopyFailed] = useState(false)
  // What 链接原文 shows: the last thing someone tried to copy.
  const [shown, setShown] = useState<"link" | "command">("link")
  const [kind, setKind] = useState<Kind>("phone")
  const sheet = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(null), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  // A join request is the next thing to act on, so it takes the only solid button.
  const joins = useOpenJoins()
  const requests = joins.length > 0
  // A link is single-use: once a device has answered it, it is spent. Requests already open when it was made are not that answer.
  const [used, setUsed] = useState(false)
  const earlier = useRef<Set<string>>(new Set())
  const answered = result !== null && joins.some((request) => !earlier.current.has(request.join_id))
  useEffect(() => { if (answered) setUsed(true) }, [answered])
  useEffect(() => { setResult(null); setError(""); setStatus(""); setAdded(null); setCopyFailed(false) }, [ready])
  // A new request, an answer or a new link is a new state; an old failure would read as part of it.
  const joinIds = joins.map((request) => request.join_id).join()
  useEffect(() => { setError(""); setCopyFailed(false) }, [joinIds])
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!result) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [result, ready])

  // Bound to the connection generation so a late clipboard or invite can't release a newer action.
  async function run(action: () => Promise<void>, success: string) {
    if (pending.current === generation) return
    pending.current = generation
    const connection = ready
    setBusyGeneration(generation); setError(""); setStatus("")
    try { await action(); if (connection?.isCurrent()) setStatus(success) }
    catch (cause) { if (connection?.isCurrent()) setError(messageOf(cause)) }
    finally { if (pending.current === generation) pending.current = null; setBusyGeneration((owner) => owner === generation ? null : owner) }
  }
  const generate = () => { earlier.current = new Set(joins.map((request) => request.join_id)); setResult(null); setUsed(false); setAdded(null); setCopyFailed(false); void run(async () => {
    const connection = ready
    const invitation = await device.createInvitation()
    if (connection?.isCurrent()) setResult(share(invitation))
  }, "") }
  // A clipboard refusal is the browser's, not a file permission, so it skips the shared error wording.
  const copy = (what: "link" | "command", value: string) => { setCopyFailed(false); setCopied(null); setShown(what); void run(async () => {
    const connection = ready
    try { await navigator.clipboard.writeText(value) } catch { if (connection?.isCurrent()) setCopyFailed(true); return }
    if (connection?.isCurrent()) setCopied(what)
  }, "") }
  const raw = useRef<HTMLTextAreaElement>(null)
  useEffect(() => { if (copyFailed) { raw.current?.focus(); raw.current?.select() } }, [copyFailed])
  const copyButton = (what: "link" | "command", value: string, label: string, solid: boolean) => <Button type="button" variant={solid && !requests ? "default" : "outline"} className="w-fit" disabled={busy} onClick={() => copy(what, value)}>
    {copied === what ? <CircleCheck aria-hidden="true" /> : <Copy aria-hidden="true" />}{copied === what ? "已复制" : label}
  </Button>
  const show = () => { setOpen(true); if (!result && !requests) generate() }
  useEffect(() => {
    if (!opening || !ready?.principal.can_manage) return
    setKind(opening)
    show()
    onOpened?.()
  }, [opening, ready])
  if (!ready?.principal.can_manage) return null

  return <>
    <PageActions>
      <Button type="button" variant="ghost" size="icon" className="md:hidden" aria-label="添加设备" title="添加设备" onClick={show}><Plus aria-hidden="true" /></Button>
      <Button type="button" variant={requests ? "outline" : "default"} className="max-md:hidden" onClick={show}><Plus aria-hidden="true" />添加设备</Button>
    </PageActions>
    <Sheet open={open} onOpenChange={(next) => { setOpen(next); if (!next) setAdded(null) }}>
      {/* A waiting request leads the sheet; its first control is 设为管理设备, which should not take focus on its own. */}
      <SheetContent ref={sheet} className="!w-full max-sm:!border-l-0 overflow-y-auto sm:!max-w-md" onOpenAutoFocus={(event) => { if (requests) { event.preventDefault(); sheet.current?.focus() } }}>
        <SheetHeader className="pr-14"><SheetTitle tabIndex={-1} className="outline-none">添加设备</SheetTitle><SheetDescription className="sr-only">在新设备上打开链接，安全码一致后允许</SheetDescription></SheetHeader>
        <div className="grid gap-6 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
          {/* A device already asking is the next thing to act on, so it comes before a new link. */}
          <JoinRequests issuing={result !== null} onAllowed={(name, manage) => { setAdded({ name, manage }); setResult(null); setError(""); setCopyFailed(false) }} />
          {added && !requests ? <div className="grid gap-3">
            <p role="status" className="flex min-w-0 items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" /><span className="truncate" title={added.name}>{added.manage ? deviceCopy.addedManager(added.name) : `已添加「${added.name}」`}</span></p>
            <Button type="button" variant="outline" className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} icon={Plus} />再添加一台</Button>
          </div> : !result || (used && !requests) ? <Button type="button" variant={requests ? "outline" : "default"} className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} icon={Link2} />生成链接</Button> : null}
          {result && !used ? <div className="grid gap-3">
            <Segmented label="新设备是" labels fill value={kind} options={[...kinds]} onChange={(next) => { setKind(next); setCopied(null); setCopyFailed(false); setError(""); setShown("link") }} />
            {kind === "phone" ? <>
              {result.image ? <img className="size-56 justify-self-center rounded-lg bg-white p-2" src={result.image} alt={result.publicEntry ? "用手机相机扫码打开" : "用手机 App 扫码"} /> : null}
              {result.publicEntry ? null : <p className="flex items-center justify-center gap-1.5 text-meta text-muted-foreground"><Smartphone aria-hidden="true" className="size-4 shrink-0" />用手机 App 扫码</p>}
              {copyButton("link", result.link, "复制链接", true)}
            </> : <>
              <ol className="grid gap-3">
                <li className="flex min-w-0 items-center gap-3"><Step n={1} />{copyButton("link", result.link, "复制链接", true)}</li>
                <li className="flex min-w-0 items-start gap-3"><Step n={2} /><Monitor aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" /><span className="min-w-0 [overflow-wrap:anywhere]">{addDeviceCopy.openThere}</span></li>
              </ol>
              <p className="flex min-w-0 flex-wrap items-center gap-x-1.5 text-meta text-muted-foreground">
                <Download aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.installThere}
                <a className="hit-area relative font-medium text-foreground underline underline-offset-4" href={joinCopy.installGuide} target="_blank" rel="noopener noreferrer">{addDeviceCopy.installHow}</a>
              </p>
              <div className="grid gap-2 border-t pt-3">
                <p className="flex min-w-0 items-center gap-1.5 text-meta text-muted-foreground"><SquareTerminal aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.orCommand}</p>
                {copyButton("command", addDeviceCopy.command(result.link), addDeviceCopy.copyCommand, false)}
              </div>
            </>}
            <ul className="grid gap-1.5 text-meta text-muted-foreground">
              <li className="flex items-center gap-1.5"><terms.safetyCode.icon aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.compare}</li>
              <li className="flex items-center gap-1.5"><Clock aria-hidden="true" className="size-4 shrink-0" />只发给自己的设备，10 分钟内有效</li>
            </ul>
            {copyFailed ? <p role="alert" className="flex items-center gap-2 text-meta text-danger"><CircleAlert aria-hidden="true" className="size-4 shrink-0" />{addDeviceCopy.copyFailed}</p> : null}
            <Details key={copyFailed ? "open" : "folded"} label="链接原文" open={copyFailed || undefined}>
              <Textarea ref={raw} readOnly aria-label="链接原文" className="min-h-24 font-mono text-meta" value={shown === "command" ? addDeviceCopy.command(result.link) : result.link} onFocus={(event) => event.target.select()} />
            </Details>
          </div> : null}
          {status ? <p role="status" className="text-meta text-success">{status}</p> : null}
          <InlineError error={error || null} />
        </div>
      </SheetContent>
    </Sheet>
  </>
}

function Step({ n }: { n: number }) {
  return <span aria-hidden="true" className="grid size-6 shrink-0 place-items-center rounded-full bg-muted text-meta font-medium tabular-nums text-muted-foreground">{n}</span>
}
