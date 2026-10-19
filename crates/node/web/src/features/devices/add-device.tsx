import { useEffect, useRef, useState } from "react"
import { Link } from "@tanstack/react-router"
import { Copy, Download, Laptop, Plus, Smartphone, TriangleAlert } from "lucide-react"
import qrcode from "qrcode-generator"
import { cn } from "cn"
import { BusyIcon, Details, InlineError } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Textarea } from "@/components/ui/textarea"
import { useDevice } from "@/features/identity/context"
import { isLoopback } from "@/features/workspace/labels"
import { downloadConnectionFile } from "@/lib/connection-file"
import { invitationLink } from "@/lib/enrollment-reference"
import { messageOf, record, text } from "@/lib/node"

type Kind = "client" | "node"
type Share = { kind: Kind; content: string; link: string; qr: string | null; local: boolean }

function share(kind: Kind, content: string, fallback: string): Share {
  const info = record(JSON.parse(content) as unknown, "连接信息")
  const page = typeof info.url === "string" ? text(info.url, "连接地址") : fallback
  let link = ""
  try { link = invitationLink(content, page) } catch {}
  const local = isLoopback(page)
  let qr: string | null = null
  if (kind === "client" && link && !local) { try { const code = qrcode(0, "M"); code.addData(link); code.make(); qr = code.createDataURL(4, 16) } catch {} }
  return { kind, content, link, qr, local }
}

/** "添加设备": one link (and QR for phones) the new device opens, then both sides compare the check code. */
export function AddDevice() {
  const device = useDevice()
  const { ready } = device
  const generation = device.connectionGeneration()
  const [open, setOpen] = useState(false)
  const [kind, setKind] = useState<Kind>("client")
  const [result, setResult] = useState<Share | null>(null)
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState("")
  const [status, setStatus] = useState("")
  useEffect(() => { setResult(null); setError(""); setStatus("") }, [ready])
  // While a link is out, look for the new device's request more often than the shell does.
  useEffect(() => {
    if (!result) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 5000)
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
    // Computers get the roles a personal device needs; no role picker (docs/design/web-ux.md section 8).
    const content = await device.createInvitation(kind, kind === "node" ? ["relay", "replica", "scheduler"] : [])
    if (connection?.isCurrent()) setResult(share(kind, content, connection.entry.address))
  }, "") }
  const copy = (value: string) => void run(() => navigator.clipboard.writeText(value), "已复制")
  const choose = (next: Kind) => { setKind(next); setResult(null); setStatus("") }

  return <>
    <Button type="button" onClick={() => setOpen(true)}><Plus aria-hidden="true" />添加设备</Button>
    <Sheet open={open} onOpenChange={setOpen}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-md">
        <SheetHeader className="pr-14"><SheetTitle>添加设备</SheetTitle><SheetDescription>在新设备上打开链接，两边核对码一致后允许。</SheetDescription></SheetHeader>
        <div className="grid gap-5 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
          <div role="radiogroup" aria-label="设备类型" className="grid grid-cols-2 gap-2">
            {([["client", Smartphone, "手机或浏览器"], ["node", Laptop, "另一台电脑"]] as const).map(([value, Icon, label]) => <button key={value} type="button" role="radio" aria-checked={kind === value} disabled={busy}
              className={cn("grid min-h-20 place-items-center gap-1 rounded-xl border p-3 text-meta", kind === value ? "border-foreground bg-muted" : "hover:bg-muted/50")} onClick={() => choose(value)}>
              <Icon aria-hidden="true" className="size-6" />{label}
            </button>)}
          </div>
          {kind === "node" ? <p className="text-meta text-muted-foreground">在那台电脑的 vibeface 本机网页里粘贴</p> : null}
          {!result ? <Button type="button" className="w-fit" disabled={busy} onClick={generate}><BusyIcon busy={busy} />生成链接</Button> : null}
          {result ? <div className="grid gap-3">
            {result.qr ? <img className="size-56 justify-self-center rounded-lg bg-white p-2" src={result.qr} alt="用新设备扫码打开" /> : null}
            {result.kind === "client" && result.local ? <div className="grid gap-2 rounded-lg bg-muted p-3 text-meta">
              <p className="flex items-center gap-1.5 text-warning"><TriangleAlert aria-hidden="true" className="size-4" />手机连不上本机地址</p>
              <Button asChild variant="outline" className="w-fit"><Link to="/devices/$deviceId" params={{ deviceId: ready.entry.peer_id }} onClick={() => setOpen(false)}>设置访问地址</Link></Button>
            </div> : null}
            {result.link ? <Input readOnly aria-label="加入链接" className="font-mono text-meta" value={result.link} onFocus={(event) => event.target.select()} /> : null}
            <div className="flex flex-wrap gap-2">
              {result.link ? <Button type="button" disabled={busy} onClick={() => copy(result.link)}><Copy aria-hidden="true" />复制链接</Button> : null}
              <Button type="button" variant="outline" disabled={busy} onClick={() => void run(async () => downloadConnectionFile(result.content), "已下载")}><Download aria-hidden="true" />下载文件</Button>
            </div>
            <p className="text-meta text-muted-foreground">只发给你自己的设备，10 分钟内有效。</p>
            <Details label="原始内容">
              <Textarea readOnly className="min-h-24 font-mono text-meta" value={result.content} onFocus={(event) => event.target.select()} />
              <Button type="button" variant="ghost" className="w-fit" disabled={busy} onClick={() => copy(result.content)}>复制原始内容</Button>
            </Details>
          </div> : null}
          {status ? <p role="status" className="text-meta text-success">{status}</p> : null}
          <InlineError error={error || null} />
        </div>
      </SheetContent>
    </Sheet>
  </>
}
