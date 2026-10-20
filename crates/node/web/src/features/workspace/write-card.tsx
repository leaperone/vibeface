import { useState } from "react"
import { Link } from "@tanstack/react-router"
import { Copy, Lock } from "lucide-react"
import { ActionCard, DetailRow, Details } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
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

/** 只读确认 and the two copies of a device's records, from docs/design/unified-network.md 5.7 and 5.8. */
export function WriteCard({ kind }: { kind: WriteCardKind }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const [waiting, setWaiting] = useState(false)
  const lookup = { nodes: workspace.nodes, directory: device.directory }
  if (kind === "read_only") return <ActionCard icon={Lock} title="这个设备的数据是从备份恢复的吗？" actions={waiting ? undefined : <>
    <Button type="button" variant="ghost" onClick={() => setWaiting(true)}>是，先同步</Button>
    <ActionButton variant="default" action={() => workspace.confirmWritable()}>不是，继续使用</ActionButton>
  </>}>
    <p className="text-muted-foreground">{waiting ? "打开你的其他设备，连上后会自动对齐，然后就能写。" : "你的其他设备都连不上，这台设备先只读。"}</p>
  </ActionCard>
  const conflicts = workspace.writeState.conflicts
  const authors = [...new Set(conflicts.map((item) => item.author))]
  const entry = device.ready?.entry.device_id
  const target = kind === "forked" ? entry : authors[0]
  return <ActionCard icon={Copy} title={kind === "forked" ? "这台设备的数据有两份副本" : `${authors.map((id) => nodeName(id, lookup)).join("、")} 的数据有两份副本`}
    actions={target ? <Button asChild variant="outline"><Link to="/devices/$deviceId" params={{ deviceId: target }}>去设备页</Link></Button> : undefined}>
    <p className="text-muted-foreground">{kind === "forked" ? "它已停止写入，其他设备照常使用。移除这台设备，再用新的数据目录重新加入；需要保留的内容请重新发送。" : "一份复制出来的数据写了不同的内容，已放在一边，不影响对话。移除那台设备后重新加入即可。"}</p>
    {conflicts.length ? <Details label="两份内容">
      {conflicts.map((item) => <div key={item.other.hash} className="grid gap-1 border-t pt-2 first:border-t-0 first:pt-0">
        <DetailRow label={`${nodeName(item.author, lookup)} 第 ${item.seq} 条 · 保留的`} value={summary(item.kept)} />
        <DetailRow label="放在一边的" value={summary(item.other)} />
      </div>)}
    </Details> : null}
  </ActionCard>
}
