import { useEffect, useState } from "react"
import { ActionCard, Details, DetailRow, InlineError, SafetyCode, Skeleton, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { checkCode } from "@/lib/check-code"
import type { PairReceipt } from "@/lib/node"
import { deviceIcon } from "./devices"
import { ignorePair, useIgnoredPairs } from "./ignored"

/** Join requests waiting on an admin, compared by the check code shown on both screens. */
export function JoinRequests() {
  const device = useDevice()
  const ignored = useIgnoredPairs()
  const pairs = device.pairs.filter((pair) => !ignored.includes(pair.request_id))
  if (!device.ready?.principal.can_manage || !pairs.length) return null
  return <section className="grid gap-3" aria-label="等待加入">
    <h3 className="text-meta font-medium text-muted-foreground">等待加入（{pairs.length}）</h3>
    {pairs.map((pair) => <JoinCard key={pair.request_id} pair={pair} />)}
  </section>
}

export function JoinCard({ pair }: { pair: PairReceipt }) {
  const device = useDevice()
  const action = useAction()
  const [code, setCode] = useState("")
  const member = pair.review.new_member
  const signed = Boolean(pair.new_subject_signature)
  useEffect(() => {
    let current = true
    void checkCode(pair.review.network_id, pair.review.inviter_subject_id, member.subject_id).then((value) => { if (current) setCode(value) })
    return () => { current = false }
  }, [pair.review.network_id, pair.review.inviter_subject_id, member.subject_id])
  return <ActionCard
    icon={deviceIcon({ kind: member.kind, name: member.name, entry: false })}
    tone="info"
    title={`${member.name || "新设备"} 想加入`}
    meta={member.kind === "node" ? "电脑" : "浏览器"}
    actions={<>
      <Button type="button" variant="ghost" disabled={action.busy !== null} onClick={() => ignorePair(pair.request_id)}>忽略</Button>
      <Button type="button" disabled={!signed || action.busy !== null} onClick={() => void action.run("allow", () => device.approvePair(pair.request_id), "已允许加入")}>{action.busy ? "处理中" : "允许"}</Button>
    </>}
  >
    <p className="text-meta text-muted-foreground">{signed ? "两边的核对码一致再允许" : "等对方确认核对码"}</p>
    {code ? <SafetyCode code={code} /> : <Skeleton className="h-8 w-56" />}
    <InlineError error={action.error} />
    <Details>
      <DetailRow label="设备编号" value={member.subject_id} mono />
      <DetailRow label="公钥" value={member.public_key} mono />
      <DetailRow label="邀请方" value={pair.review.inviter_subject_id} mono />
      <DetailRow label="个人网络" value={pair.review.network_id} mono />
    </Details>
  </ActionCard>
}
