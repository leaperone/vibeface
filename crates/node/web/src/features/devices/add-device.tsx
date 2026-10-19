import { useEffect, useRef, useState } from "react"
import { Link } from "@tanstack/react-router"
import { Copy, Plus, TriangleAlert } from "lucide-react"
import qrcode from "qrcode-generator"
import { BusyIcon, Details, InlineError } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@/features/identity/context"
import type { Invitation } from "@/features/identity/context"
import { isLoopback } from "@/features/workspace/labels"
import { messageOf } from "@/lib/node"
import { JoinRequests, useOpenJoins } from "./join"

type Share = Invitation & { qr: string | null }

function share(invitation: Invitation): Share {
  let qr: string | null = null
  // A phone can only use a public HTTPS address; anything else would scan into a dead page.
  if (invitation.publicEntry) { try { const code = qrcode(0, "M"); code.addData(invitation.link); code.make(); qr = code.createDataURL(4, 16) } catch {} }
  return { ...invitation, qr }
}

/** "添加设备": one link (QR for phones) the new device opens, then both sides compare the safety code. */
export function AddDevice() {
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
  // A join request is the next thing to act on, so it takes the only solid button.
  const requests = useOpenJoins().length > 0
  useEffect(() => { setResult(null); setError(""); setStatus("") }, [ready])
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!result) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 3000)
    return () => clearInterval(timer)
  }, [result, ready])
  if (!ready?.principal.can_manage) return null

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
  const generate = () => { setResult(null); void run(async () => {
    const connection = ready
    const invitation = await device.createInvitation()
    if (connection?.isCurrent()) setResult(share(invitation))
  }, "") }
  const copy = (value: string) => void run(() => navigator.clipboard.writeText(value), "已复制")
  const local = result ? isLoopback(result.link) : false

  return <>
    <Button type="button" variant={requests ? "outline" : "default"} onClick={() => setOpen(true)}><Plus aria-hidden="true" />添加设备</Button>
    <Sheet open={open} onOpenChange={setOpen}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-md">
        <SheetHeader className="pr-14"><SheetTitle>添加设备</SheetTitle><SheetDescription>在新设备上打开链接，安全码一致后允许</SheetDescription></SheetHeader>
        <div className="grid gap-5 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
          <JoinRequests />
          {!result ? <Button type="button" variant={requests ? "outline" : "default"} className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} />生成链接</Button> : null}
          {result ? <div className="grid gap-3">
            {result.qr ? <img className="size-56 justify-self-center rounded-lg bg-white p-2" src={result.qr} alt="用新设备扫码打开" /> : <div className="grid gap-2 rounded-lg bg-muted p-3 text-meta">
              <p className="flex items-center gap-1.5 text-warning"><TriangleAlert aria-hidden="true" className="size-4" />{local ? "手机连不上本机地址" : "手机需要 HTTPS 访问地址"}</p>
              <Button asChild variant="outline" className="w-fit"><Link to="/devices/$deviceId" params={{ deviceId: ready.entry.device_id }} onClick={() => setOpen(false)}>设置访问地址</Link></Button>
            </div>}
            <Button type="button" variant={requests ? "outline" : "default"} className="w-fit" disabled={busy} onClick={() => copy(result.link)}><Copy aria-hidden="true" />复制链接</Button>
            <p className="text-meta text-muted-foreground">只发给你自己的设备，10 分钟内有效。另一台电脑上运行 <code>vibeface join</code> 加这条链接。</p>
            <Details label="链接原文">
              <Textarea readOnly className="min-h-24 font-mono text-meta" value={result.link} onFocus={(event) => event.target.select()} />
            </Details>
          </div> : null}
          {status ? <p role="status" className="text-meta text-success">{status}</p> : null}
          <InlineError error={error || null} />
        </div>
      </SheetContent>
    </Sheet>
  </>
}
