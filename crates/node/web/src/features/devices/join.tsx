import { useEffect, useRef, useState } from "react"
import { ActionCard, Details, DetailRow, InlineError, SafetyCode, Skeleton, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import type { JoinRequest } from "@/lib/node"
import { deviceIcon } from "./devices"

/** Requests an admin can still act on. */
export function useOpenJoins(): JoinRequest[] {
  const device = useDevice()
  return device.ready?.principal.can_manage ? device.joins.filter((request) => request.status === "pending" || request.status === "confirming") : []
}

/** Join requests waiting on an admin, compared by the safety code shown on both screens. */
export function JoinRequests() {
  const device = useDevice()
  const joins = useOpenJoins()
  // The code appears once the new device reveals its nonce; poll until every open request shows one.
  const waiting = joins.some((request) => !request.code)
  useEffect(() => {
    if (!waiting) return
    const timer = setInterval(() => { device.refreshDirectory().catch(() => {}) }, 2000)
    return () => clearInterval(timer)
  }, [waiting, device.ready])
  if (!joins.length) return null
  return <section className="grid gap-3" aria-label="等待加入">
    <h3 className="text-meta font-medium text-muted-foreground">等待加入（{joins.length}）</h3>
    {joins.map((request) => <JoinCard key={request.join_id} request={request} />)}
  </section>
}

export function JoinCard({ request }: { request: JoinRequest }) {
  const device = useDevice()
  const action = useAction()
  const [manage, setManage] = useState(false)
  const started = useRef(false)
  // Opening the card is the admin's commitment; the new device then reveals and both sides show the code.
  useEffect(() => {
    if (request.status !== "pending" || started.current) return
    started.current = true
    void action.run("start", () => device.startApproval(request.join_id))
  }, [request.status, request.join_id])
  const busy = action.busy !== null
  // Only the admin that started the comparison can allow it (the Node enforces this too).
  const elsewhere = Boolean(request.admin_id && request.admin_id !== device.identity?.deviceId)
  return <ActionCard
    icon={deviceIcon({ kind: request.kind, name: request.name, entry: false })}
    tone="info"
    title={`${request.name || "新设备"} 想加入`}
    meta={request.kind === "node" ? "电脑" : "浏览器"}
    actions={<>
      <Button type="button" variant="ghost" disabled={busy} onClick={() => void action.run("reject", () => device.rejectJoin(request.join_id), "已拒绝")}>拒绝</Button>
      <Button type="button" disabled={!request.code || busy || elsewhere} onClick={() => void action.run("allow", () => device.approveJoin(request.join_id, request.kind === "client" && manage), "已允许加入")}>{action.busy === "allow" ? "处理中" : "一致，允许"}</Button>
    </>}
  >
    <p className="text-meta text-muted-foreground">{elsewhere ? "正在另一台管理设备上处理" : request.code ? "和新设备上显示的一致再允许" : "等新设备显示安全码"}</p>
    {request.code ? <SafetyCode code={request.code} /> : <Skeleton className="h-8 w-40" />}
    {request.kind === "client" ? <label className="flex min-h-11 items-center gap-2 text-meta"><input type="checkbox" checked={manage} disabled={busy} onChange={(event) => setManage(event.target.checked)} />设为管理员</label> : null}
    <InlineError error={action.error} />
    <Details>
      <DetailRow label="设备编号" value={request.device_id} mono />
      <DetailRow label="邀请编号" value={request.invite_id} mono />
      <DetailRow label="个人网络" value={request.network_id} mono />
    </Details>
  </ActionCard>
}
