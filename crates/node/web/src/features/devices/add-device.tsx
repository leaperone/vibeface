import { useEffect, useRef, useState } from "react"
import { Link } from "@tanstack/react-router"
import { CircleCheck, Copy, Link2, Plus, Smartphone } from "lucide-react"
import qrcode from "qrcode-generator"
import { BusyIcon, Details, InlineError } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import type { Invitation } from "@vibeface/client/identity/context.tsx"
import { isLoopback } from "@vibeface/client/workspace/labels.ts"
import { messageOf } from "@vibeface/client/lib/node.ts"
import { JoinRequests, useOpenJoins } from "./join"

type Share = Invitation & { qr: string | null }

function share(invitation: Invitation): Share {
  let qr: string | null = null
  // A phone's camera opens a public HTTPS link only; any other address would scan into a dead page.
  if (invitation.publicEntry) try { const code = qrcode(0, "M"); code.addData(invitation.link); code.make(); qr = code.createDataURL(4, 16) } catch {}
  return { ...invitation, qr }
}

/** "添加设备": one link (QR for phones) the new device opens, then both sides compare the safety code. `opening`: arrived from a hint that asked for it. */
export function AddDevice({ opening = false, onOpened }: { opening?: boolean; onOpened?(): void }) {
  const device = useDevice()
  const { ready } = device
  const generation = device.connectionGeneration()
  const [open, setOpen] = useState(false)
  const [result, setResult] = useState<Share | null>(null)
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState("")
  const [status, setStatus] = useState("")
  // This page allowed the device, so it says so where the link was rather than only in a toast.
  const [added, setAdded] = useState<string | null>(null)
  // "已复制" shows on the button itself for a moment.
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  // A join request is the next thing to act on, so it takes the only solid button.
  const requests = useOpenJoins().length > 0
  // A link is single-use: once a device has answered it, it is spent.
  const [used, setUsed] = useState(false)
  useEffect(() => { if (requests && result) setUsed(true) }, [requests, result])
  useEffect(() => { setResult(null); setError(""); setStatus(""); setAdded(null) }, [ready])
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
  const generate = () => { setResult(null); setUsed(false); setAdded(null); void run(async () => {
    const connection = ready
    const invitation = await device.createInvitation()
    if (connection?.isCurrent()) setResult(share(invitation))
  }, "") }
  const copy = (value: string) => void run(async () => { await navigator.clipboard.writeText(value); if (ready?.isCurrent()) setCopied(true) }, "")
  const show = () => { setOpen(true); if (!result && !requests) generate() }
  useEffect(() => {
    if (!opening || !ready?.principal.can_manage) return
    show()
    onOpened?.()
  }, [opening, ready])
  const local = result ? isLoopback(result.link) : false
  if (!ready?.principal.can_manage) return null

  return <>
    <Button type="button" variant={requests ? "outline" : "default"} onClick={show}><Plus aria-hidden="true" />添加设备</Button>
    <Sheet open={open} onOpenChange={(next) => { setOpen(next); if (!next) setAdded(null) }}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-md">
        <SheetHeader className="pr-14"><SheetTitle>添加设备</SheetTitle><SheetDescription className="sr-only">在新设备上打开链接，安全码一致后允许</SheetDescription></SheetHeader>
        <div className="grid gap-5 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
          {added && !requests ? <div className="grid gap-3">
            <p role="status" className="flex min-w-0 items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" /><span className="truncate" title={added}>已添加「{added}」</span></p>
            <Button type="button" variant="outline" className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} icon={Plus} />再添加一台</Button>
          </div> : !result || (used && !requests) ? <Button type="button" variant={requests ? "outline" : "default"} className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} icon={Link2} />生成链接</Button> : null}
          <JoinRequests issuing={result !== null} onAllowed={(name) => { setAdded(name); setResult(null) }} />
          {result && !used ? <div className="grid gap-3">
            {result.qr ? <img className="size-56 justify-self-center rounded-lg bg-white p-2" src={result.qr} alt="用手机相机扫码打开" /> : <div className="grid gap-2 rounded-lg bg-muted p-3 text-meta">
              <p className="flex items-center gap-1.5 text-muted-foreground"><Smartphone aria-hidden="true" className="size-4 shrink-0" />{local ? "手机打不开本机地址" : "手机打不开这个地址"}</p>
              <Button asChild variant="outline" className="w-fit"><Link to="/devices/$deviceId" params={{ deviceId: ready.entry.device_id }} onClick={() => setOpen(false)}>设置访问地址</Link></Button>
            </div>}
            <Button type="button" variant={requests ? "outline" : "default"} className="w-fit" disabled={busy} onClick={() => copy(result.link)}>{copied ? <CircleCheck aria-hidden="true" /> : <Copy aria-hidden="true" />}{copied ? "已复制" : "复制链接"}</Button>
            <p className="text-meta text-muted-foreground">只发给你自己的设备，10 分钟内有效。</p>
            <Details label="链接原文">
              <Textarea readOnly className="min-h-24 font-mono text-meta" value={result.link} onFocus={(event) => event.target.select()} />
              <p className="text-meta text-muted-foreground">另一台电脑上运行 <code>vibeface join</code> 加这条链接。</p>
            </Details>
          </div> : null}
          {status ? <p role="status" className="text-meta text-success">{status}</p> : null}
          <InlineError error={error || null} />
        </div>
      </SheetContent>
    </Sheet>
  </>
}
