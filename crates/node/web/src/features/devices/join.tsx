import { useEffect, useLayoutEffect, useRef, useState } from "react"
import { ActionCard, claimFocus, Details, DetailRow, InlineError, SafetyCode, Skeleton, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import type { JoinRequest } from "@vibeface/client/lib/node.ts"
import { deviceIcon } from "./devices"
import { terms } from "@/lib/glossary"

/** Requests an admin can still act on. */
export function useOpenJoins(): JoinRequest[] {
  const device = useDevice()
  return device.ready?.principal.can_manage ? device.joins.filter((request) => request.status === "pending" || request.status === "confirming") : []
}

/** Join requests waiting on an admin, compared by the safety code shown on both screens. */
/**
 * `issuing`: this page just handed out a link, so it starts the comparison without a click.
 * `onAllowed`: the page shows the added device itself instead of a toast.
 */
export function JoinRequests({ issuing = false, onAllowed }: { issuing?: boolean; onAllowed?(name: string): void }) {
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
    {joins.map((request) => <div key={request.join_id} tabIndex={-1} className="min-w-0 outline-none"><JoinCard request={request} auto={issuing} onAllowed={onAllowed} /></div>)}
  </section>
}

// The same request can show in the sheet, the page and the inbox at once; start it only once per page.
const started = new Set<string>()

/**
 * Starting the comparison claims the request for this admin, so only the page that issued the link does it
 * on its own; every other admin page asks first.
 */
export function JoinCard({ request, auto = false, onAllowed }: { request: JoinRequest; auto?: boolean; onAllowed?(name: string): void }) {
  const device = useDevice()
  const action = useAction()
  const [manage, setManage] = useState(false)
  // Opening the card is the admin's commitment; the new device then reveals and both sides show the code.
  useEffect(() => {
    if (!auto || request.status !== "pending" || started.has(request.join_id)) return
    started.add(request.join_id)
    void action.run("start", () => device.startApproval(request.join_id))
  }, [auto, request.status, request.join_id])
  const busy = action.busy !== null
  // 显示安全码 is disabled while it works, which drops focus to <body>; once the code is up, 允许 is the next step.
  const allow = useRef<HTMLButtonElement>(null)
  useEffect(() => { if (request.code && !busy && allow.current && document.activeElement === document.body) allow.current.focus() }, [request.code, busy])
  // Only the admin that started the comparison can allow it (the Node enforces this too).
  const elsewhere = Boolean(request.admin_id && request.admin_id !== device.identity?.deviceId)
  return <ActionCard
    icon={deviceIcon({ kind: request.kind, name: request.name, entry: false })}
    tone="info"
    title={<span className="flex min-w-0"><span className="truncate" title={request.name || undefined}>{request.name || "新设备"}</span><span className="shrink-0">&nbsp;想加入</span></span>}
    meta={request.kind === "node" ? "电脑" : "浏览器"}
    actions={<>
      <Button type="button" variant="ghost" disabled={busy} onClick={() => void action.run("reject", () => device.rejectJoin(request.join_id), "已拒绝")}>拒绝</Button>
      {request.status === "pending" && !auto ? <Button type="button" disabled={busy} onClick={() => void action.run("start", () => device.startApproval(request.join_id))}>{action.busy === "start" ? "处理中" : "显示安全码"}</Button>
      : <Button ref={allow} type="button" disabled={!request.code || busy || elsewhere} onClick={() => void action.run("allow", async () => { await device.approveJoin(request.join_id, request.kind === "client" && manage); onAllowed?.(request.name || "新设备") }, onAllowed ? undefined : "已允许加入")}>{action.busy === "allow" ? "处理中" : "一致，允许"}</Button>}
    </>}
  >
    <p className="text-meta text-muted-foreground">{elsewhere ? "正在另一台管理设备上处理" : request.status === "pending" && !auto ? "点“显示安全码”，新设备上会出现同样的码" : request.code ? "和新设备上的比对，不一致就拒绝" : "等新设备显示安全码"}</p>
    {request.code ? <SafetyCode code={request.code} /> : request.status === "pending" && !auto ? null : <Skeleton className="h-8 w-40" />}
    {request.kind === "client" ? <label className="flex min-h-11 w-fit items-center gap-2 text-meta md:min-h-9"><input type="checkbox" className="size-4" checked={manage} disabled={busy} onChange={(event) => setManage(event.target.checked)} />设为{terms.admin.label}</label> : null}
    <InlineError error={action.error} />
    <Details>
      <DetailRow label="设备编号" value={request.device_id} mono />
    </Details>
  </ActionCard>
}
