import { useEffect, useState, type ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { Bot, ChevronRight, Crown, Globe, Laptop, Monitor, MonitorSmartphone, Server, Smartphone, TriangleAlert, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { Details, DetailRow, EmptyState, InlineError, Skeleton, StatusPill, toast, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@/features/identity/context"
import { useWorkspace } from "@/features/workspace/context"
import { isLoopback, roleNames } from "@/features/workspace/labels"
import type { NodeInfo } from "@/features/workspace/types"
import type { Member } from "@/lib/node"
import { AssistantSettings } from "./assistant-settings"
import { JoinRequests } from "./join"

export type DeviceRow = {
  id: string; name: string; kind: "node" | "client"; member?: Member; node?: NodeInfo
  /** The device this browser is connected through. */ entry: boolean
  browser: boolean; online?: boolean; assistant: boolean; admin: boolean
}

export function useDeviceRows(): DeviceRow[] {
  const { directory, ready, identity } = useDevice()
  const { nodes } = useWorkspace()
  const members = directory?.members ?? []
  const ids = [...new Set([...members.map((member) => member.subject_id), ...nodes.filter((node) => !node.revoked).map((node) => node.peer_id)])]
  const rank = (row: DeviceRow) => row.browser ? 0 : row.entry ? 1 : row.kind === "node" ? 2 : 3
  return ids.map((id): DeviceRow => {
    const member = members.find((item) => item.subject_id === id), node = nodes.find((item) => item.peer_id === id)
    const kind = member?.kind ?? "node"
    return {
      id, member, node, kind, name: member?.name || node?.name || (kind === "node" ? "电脑" : "浏览器"),
      entry: id === ready?.entry.peer_id, browser: id === identity?.peerId,
      online: kind === "node" ? node?.online ?? false : undefined, assistant: Boolean(node?.capabilities?.manager), admin: member?.manage ?? false,
    }
  }).sort((a, b) => rank(a) - rank(b))
}

export function deviceIcon(row: Pick<DeviceRow, "kind" | "name" | "entry">): LucideIcon {
  if (row.kind === "node") return row.entry ? Laptop : Server
  return /iphone|android|ipad|phone|手机/i.test(row.name) ? Smartphone : Monitor
}

function Tag({ children }: { children: ReactNode }) {
  return <span className="shrink-0 rounded-md bg-muted px-1.5 text-meta text-muted-foreground">{children}</span>
}

function Online({ online }: { online: boolean }) {
  return <span className={cn("flex shrink-0 items-center gap-1.5 text-meta", online ? "text-success" : "text-muted-foreground")}>
    <span aria-hidden="true" className={cn("size-2 rounded-full", online ? "bg-success" : "border border-muted-foreground")} />{online ? "在线" : "离线"}
  </span>
}

function useEntryLabel() {
  const { ready } = useDevice()
  return ready && isLoopback(ready.entry.address) ? "这台电脑" : "正在连接"
}

export function DevicesPage({ actions }: { actions?: ReactNode }) {
  const device = useDevice()
  const rows = useDeviceRows()
  const entryLabel = useEntryLabel()
  const [error, setError] = useState<unknown>(null)
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(setError) }, [device.ready])
  return <div className="mx-auto grid w-full max-w-3xl gap-6">
    {actions ? <div className="flex justify-end">{actions}</div> : null}
    <JoinRequests />
    <InlineError error={error} action={{ label: "重试", onClick: () => { setError(null); device.refreshDirectory().catch(setError) } }} />
    {!rows.length ? <div className="grid gap-2" aria-busy="true"><Skeleton className="h-14" /><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
      : <ul className="divide-y overflow-hidden rounded-xl border bg-card">{rows.map((row) => {
        const Icon = deviceIcon(row)
        return <li key={row.id}><Link to="/devices/$deviceId" params={{ deviceId: row.id }} className="flex min-h-14 items-center gap-3 px-4 py-2 hover:bg-muted/50">
          <Icon aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
          <span className="flex min-w-0 flex-1 items-center gap-2"><span className="truncate font-medium">{row.name}</span>{row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}</span>
          {row.assistant ? <Bot role="img" aria-label="能运行助手" className="size-4 shrink-0 text-muted-foreground"><title>能运行助手</title></Bot> : null}
          {row.admin ? <Crown role="img" aria-label="管理员" className="size-4 shrink-0 text-muted-foreground"><title>管理员</title></Crown> : null}
          {row.online !== undefined ? <Online online={row.online} /> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        </Link></li>
      })}</ul>}
    {device.directory?.revoked.length ? <Details label={`已移除的设备（${device.directory.revoked.length}）`}>{device.directory.revoked.map((id) => <span key={id} className="font-mono">{id}</span>)}</Details> : null}
  </div>
}

function Section({ icon: Icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <section className="grid gap-4 rounded-xl border bg-card p-4">
    <h3 className="flex items-center gap-2 text-heading font-semibold"><Icon aria-hidden="true" className="size-5 text-muted-foreground" />{title}</h3>
    {children}
  </section>
}

export function DeviceDetail({ deviceId }: { deviceId: string }) {
  const device = useDevice()
  const { ready } = device
  const rows = useDeviceRows()
  const entryLabel = useEntryLabel()
  const action = useAction()
  const [removing, setRemoving] = useState(false)
  const row = rows.find((item) => item.id === deviceId)
  const canManage = ready?.principal.can_manage === true
  const localSession = ready?.principal.source === "local_session"
  if (!row) return device.directory ? <EmptyState icon={MonitorSmartphone} title="找不到这台设备" action={<Button asChild variant="outline"><Link to="/devices">回到设备列表</Link></Button>} /> : <div className="mx-auto grid w-full max-w-3xl gap-4"><Skeleton className="h-16" /><Skeleton className="h-40" /></div>
  const Icon = deviceIcon(row)
  const address = row.entry ? ready?.entry.address ?? "" : ""

  return <div className="mx-auto grid w-full max-w-3xl gap-6">
    <header className="flex items-center gap-4">
      <span className="grid size-12 shrink-0 place-items-center rounded-xl bg-muted"><Icon aria-hidden="true" className="size-6" /></span>
      <div className="grid min-w-0 gap-1">
        <h2 className="truncate text-title font-semibold">{row.name}</h2>
        <div className="flex flex-wrap items-center gap-2">
          {row.online !== undefined ? <Online online={row.online} /> : null}
          {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
          {row.admin ? <StatusPill icon={Crown}>管理员</StatusPill> : null}
        </div>
      </div>
    </header>

    {row.kind === "node" ? <Section icon={Bot} title="助手">
      {row.entry && (canManage || localSession) ? <AssistantSettings /> : <div className="grid gap-1">
        <p>{row.assistant ? "能运行助手" : "还没配置助手"}</p>
        <p className="text-meta text-muted-foreground">连到这台设备后才能修改</p>
      </div>}
    </Section> : null}

    {row.entry ? <Section icon={Globe} title="访问地址">
      <p className="break-all font-mono text-meta">{address}</p>
      {isLoopback(address) ? <div className="grid gap-1">
        <p className="flex items-center gap-1.5 text-meta text-warning"><TriangleAlert aria-hidden="true" className="size-4" />手机和其他电脑连不上</p>
        <Details label="怎么设置">
          <span>把数据目录里 config.json 的 public_url 改成其他设备能打开的地址，比如局域网 IP 或 HTTPS 域名，然后重启 vibeface。</span>
        </Details>
      </div> : null}
      {canManage || localSession ? <label className="flex min-h-11 items-center justify-between gap-3 border-t pt-3">
        <span className="grid"><span>本机免确认打开</span><span className="text-meta text-muted-foreground">在这台电脑上打开时直接进入</span></span>
        <input type="checkbox" role="switch" className="switch" checked={device.localAutoAccess === true}
          disabled={action.busy !== null || device.localAutoAccess === null || localSession && !device.localAutoAccess}
          onChange={(event) => { const enabled = event.target.checked; void action.run("local", () => device.setLocalAutoAccess(enabled), "已保存") }} />
      </label> : null}
      {action.errorKey === "local" ? <InlineError error={action.error} /> : null}
    </Section> : null}

    <Details>
      <DetailRow label="设备编号" value={row.id} mono />
      {row.member ? <DetailRow label="公钥" value={row.member.public_key} mono /> : null}
      {row.member?.node_roles.length ? <DetailRow label="设备能力" value={row.member.node_roles.map((role) => roleNames[role] ?? role).join("、")} /> : null}
    </Details>

    {canManage && row.member && !row.admin && !row.entry && !row.browser ? <section className="grid gap-3 rounded-xl border border-danger-border p-4">
      {removing ? <>
        <p>移除后这台设备无法再连接，已有的对话记录会保留。</p>
        <div className="flex flex-wrap justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => setRemoving(false)}>取消</Button>
          <Button type="button" variant="destructive" disabled={action.busy !== null} onClick={() => void action.run("remove", async () => { await device.revoke(row.id); setRemoving(false); toast("已移除", { tone: "success" }) })}>
            {action.busy === "remove" ? "移除中" : "移除设备"}
          </Button>
        </div>
        {action.errorKey === "remove" ? <InlineError error={action.error} /> : null}
      </> : <Button type="button" variant="ghost" className="w-fit text-danger" onClick={() => setRemoving(true)}>移除设备</Button>}
    </section> : null}
  </div>
}
