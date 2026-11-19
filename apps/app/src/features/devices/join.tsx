import { ActionCard,claimFocus,DetailRow,Details,InlineError,SafetyCode,Skeleton,toast,useAction } from "@/components/arc/product-kit"
import { terms } from "@/lib/glossary"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { deviceCopy } from "@vibeface/client/identity/copy.ts"
import type { JoinRequest } from "@vibeface/client/lib/node.ts"
import { openJoins } from "@vibeface/client/workspace/inbox.ts"
import { deviceForm,formName } from "@vibeface/client/workspace/labels.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Checkbox } from "@vibeface/uiarc/checkbox/checkbox.tsx"
import { CircleX } from "lucide-react"
import { useEffect,useLayoutEffect,useRef,useState } from "react"
import { deviceIcon } from "./devices"

/** Requests an admin can still act on. */
export const useOpenJoins = (): JoinRequest[] => openJoins(useDevice())

/** A device this page just allowed in. */
export type Added = { id: string; kind: "node" | "client"; name: string; manage: boolean }
type OnAllowed = (added: Added, inviteId: string) => void

/** Only the device using this sheet's invitation can start automatically or inherit its management preset. */
export function JoinRequests({ inviteId, manage = false, onAllowed }: { inviteId?: string; manage?: boolean; onAllowed?: OnAllowed }) {
  const device = useDevice()
  const joins = useOpenJoins()
  // The code appears once the new device reveals its nonce; poll until every open request shows one.
  const waiting = joins.some((request) => !request.code)
  useEffect(() => {
    if (!waiting) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 2000)
    return () => clearInterval(timer)
  }, [waiting, device.ready])
  // A handled card leaves with the button just pressed; the card now in its place, or the sheet or page title, takes the focus.
  const list = useRef<HTMLElement>(null)
  const held = useRef<{ id: string; index: number; title: HTMLElement | null } | null>(null)
  const ids = joins.map((request) => request.join_id).join("\n")
  useLayoutEffect(() => {
    const last = held.current
    if (!last || joins.some((request) => request.join_id === last.id)) return
    held.current = null
    const cards = list.current?.children
    const next = cards?.length ? cards[Math.min(last.index, cards.length - 1)] : last.title
    if (next instanceof HTMLElement && next.isConnected) claimFocus(next)
  }, [ids])
  if (!joins.length) return null
  return <section ref={list} className="grid gap-3" aria-label="等待加入" onFocus={(event) => {
    const index = [...event.currentTarget.children].findIndex((card) => card.contains(event.target))
    const scope = event.currentTarget.closest<HTMLElement>("[role=dialog]") ?? document.getElementById("main")
    if (joins[index]) held.current = { id: joins[index].join_id, index, title: scope?.querySelector<HTMLElement>("h1, h2") ?? null }
  }} onBlur={(event) => {
    // Focus moved on to something else (say the sheet over this page), so a card leaving later is not this list's to answer.
    if (event.relatedTarget && !event.currentTarget.contains(event.relatedTarget as Node)) held.current = null
  }}>
    {joins.map((request) => {
      const invitedHere = request.invite_id === inviteId
      return <div key={request.join_id} tabIndex={-1} className="min-w-0 outline-none"><JoinCard request={request} auto={invitedHere} manage={invitedHere && manage} onAllowed={onAllowed} /></div>
    })}
  </section>
}

// The same request can show in the sheet, the page and the inbox at once; start it only once per page.
const started = new Set<string>()

/**
 * Starting the comparison claims the request for this admin, so only the page that issued the link does it
 * on its own; every other admin page asks first.
 */
export function JoinCard({ request, auto = false, manage: asManager = false, onAllowed }: { request: JoinRequest; auto?: boolean; manage?: boolean; onAllowed?: OnAllowed }) {
  const device = useDevice()
  const action = useAction()
  const [manageChoice, setManageChoice] = useState<boolean | null>(null)
  const manage = manageChoice ?? asManager
  const asking = request.status === "pending"
  const start = () => {
    started.add(request.join_id)
    void action.run("start", () => device.startApproval(request.join_id))
  }
  // Opening the card is the admin's commitment; the new device then reveals and both sides show the code.
  useEffect(() => {
    if (auto && asking && !started.has(request.join_id)) start()
  }, [auto, asking, request.join_id])
  const busy = action.busy !== null
  // 显示安全码 is disabled while it works, which drops focus to <body>; once the code is up, 允许 is the next step.
  const allow = useRef<HTMLButtonElement>(null)
  useEffect(() => { if (request.code && !busy && allow.current && document.activeElement === document.body) allow.current.focus() }, [request.code, busy])
  // Only the admin that started the comparison can allow it (the Node enforces this too).
  const elsewhere = Boolean(request.admin_id && request.admin_id !== device.identity?.deviceId)
  return <ActionCard
    icon={deviceIcon(request)}
    tone="info"
    title={<span className="flex min-w-0"><span className="truncate" title={request.name || undefined}>{request.name || "新设备"}</span><span className="shrink-0">&nbsp;想加入</span></span>}
    meta={formName[deviceForm(request.kind, request.name)]}
    actions={<>
      <Button type="button" variant="ghost" disabled={busy} onClick={() => void action.run("reject", async () => { await device.rejectJoin(request.join_id); toast(deviceCopy.rejected(request.name || "新设备"), { icon: CircleX }) })}>拒绝</Button>
      {asking ? <Button type="button" disabled={busy || elsewhere} onClick={start}>{action.busy === "start" ? "处理中" : "显示安全码"}</Button>
      : <Button ref={allow} type="button" disabled={!request.code || busy || elsewhere} onClick={() => void action.run("allow", async () => { await device.approveJoin(request.join_id, request.kind === "client" && manage); onAllowed?.({ id: request.device_id, kind: request.kind, name: request.name || "新设备", manage: request.kind === "client" && manage }, request.invite_id) }, onAllowed ? undefined : manage && request.kind === "client" ? deviceCopy.addedManager(request.name || "新设备") : "已允许加入")}>{action.busy === "allow" ? "处理中" : "一致，允许"}</Button>}
    </>}
  >
    <p role="status" className="text-sm text-secondary">{elsewhere ? "正在另一台管理设备上处理" : asking && action.busy !== "start" ? deviceCopy.startCode : request.code ? deviceCopy.compareCode : deviceCopy.waitForCode}</p>
    {elsewhere ? null : request.code ? <SafetyCode code={request.code} /> : !asking || action.busy === "start" ? <Skeleton className="h-8 w-40" /> : null}
    {request.kind === "client" && !elsewhere ? <div><label className="flex min-h-11 w-fit items-center gap-2 text-sm md:min-h-9"><Checkbox aria-label={`设为${terms.admin.label}`} checked={manage} disabled={busy} onCheckedChange={next => setManageChoice(next === true)} />设为{terms.admin.label}</label>
      <p className="ps-6 text-sm text-secondary">{deviceCopy.manageHint}</p></div> : null}
    <InlineError error={action.error} />
    <Details>
      <DetailRow label="设备编号" value={request.device_id} mono />
    </Details>
  </ActionCard>
}
