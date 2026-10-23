import { useState } from "react"
import { Link } from "@tanstack/react-router"
import { Copy, Lock } from "lucide-react"
import { ActionCard, DetailRow, Details } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { terms } from "@/lib/glossary"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { ActionButton } from "./present"
import type { ConflictVersion } from "./types"

export type WriteCardKind = "read_only" | "forked" | "conflicts"

const summary = (version: ConflictVersion | null) => {
  if (!version) return "这台设备上没有"
  const text = version.body.text
  return `${new Date(version.created_at_ms).toLocaleString()} · ${typeof text === "string" && text ? text.slice(0, 120) : String(version.body.type ?? "")}`
}

function Steps({ items }: { items: string[] }) {
  return <ol className="grid list-decimal gap-1 ps-5 text-muted-foreground">{items.map((item) => <li key={item}>{item}</li>)}</ol>
}

/** 只读确认 and the two copies of a device's records, from docs/design/unified-network.md 5.7 and 5.8. */
export function WriteCard({ kind }: { kind: WriteCardKind }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const [waiting, setWaiting] = useState(false)
  const lookup = { nodes: workspace.nodes, directory: device.directory }
  const entry = device.ready?.entry.device_id
  const here = entry ? nodeName(entry, lookup) : "这台设备"
  const canManage = device.ready?.principal.can_manage === true
  const away = workspace.nodes.filter((node) => !node.revoked && !node.online && node.device_id !== entry).map((node) => nodeName(node.device_id, lookup)).join("、")
  const unreached = `${away ? `${away} ` : "其他设备都"}连不上，没法确认数据是最新的。`
  if (kind === "read_only") return <ActionCard icon={Lock} title={`${here} 暂时只读`} actions={!canManage ? undefined : waiting
    ? <Button type="button" variant="ghost" onClick={() => setWaiting(false)}>返回</Button>
    : <>
      <Button type="button" variant="ghost" onClick={() => setWaiting(true)}>是，等其他设备</Button>
      <ActionButton variant="default" action={() => workspace.confirmWritable()}>不是，继续使用</ActionButton>
    </>}>
    <p className="text-muted-foreground">{!canManage ? `${unreached}打开其他设备，或在管理设备上确认。` : waiting ? `打开${away ? ` ${away}` : "你的其他设备"}，连上后会自动同步，然后就能用。` : `${unreached}它是从备份恢复的吗？`}</p>
  </ActionCard>
  const conflicts = workspace.writeState.conflicts
  const authors = [...new Set(conflicts.map((item) => item.author))]
  if (kind === "forked") return <ActionCard icon={Copy} title={`${here} 的数据被复制过`}>
    <p className="text-muted-foreground">两份记了不同的内容，它已暂停，其他设备照常用。</p>
    <Steps items={["在另一台设备上移除它", "在这台电脑上重新加入"]} />
  </ActionCard>
  const RemoveIcon = terms.remove.icon
  return <ActionCard icon={Copy} title={`${authors.map((id) => nodeName(id, lookup)).join("、")} 的数据被复制过`}
    actions={authors[0] && canManage ? <Button asChild variant="default"><Link to="/devices/$deviceId" params={{ deviceId: authors[0] }}><RemoveIcon aria-hidden="true" />去移除</Link></Button> : undefined}>
    <p className="text-muted-foreground">两份记了不同的内容，这部分先不算，其他内容照常。</p>
    <Steps items={[canManage ? "移除它，保留这里先收到的那份" : "用管理设备移除它，保留先收到的那份", "在那台电脑上重新加入"]} />
    {conflicts.length ? <Details label="两份内容">
      {conflicts.map((item) => <div key={item.other.hash} className="grid gap-1 border-t pt-2 first:border-t-0 first:pt-0">
        <DetailRow label="这里先收到的" value={summary(item.kept)} />
        <DetailRow label="另一份" value={summary(item.other)} />
      </div>)}
    </Details> : null}
  </ActionCard>
}
