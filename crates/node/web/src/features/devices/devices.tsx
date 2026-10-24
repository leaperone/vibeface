import { useEffect, useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ArrowRightLeft, Bot, ChevronRight, Copy, Crown, Globe, Laptop, Monitor, MonitorSmartphone, Server, Smartphone, TriangleAlert, type LucideIcon } from "lucide-react"
import { cn } from "cn"
import { Details, DetailRow, EmptyState, InlineError, Skeleton, StatusPill, toast, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { isLoopback } from "@vibeface/client/workspace/labels.ts"
import { terms } from "@/lib/glossary"
import type { NodeInfo } from "@vibeface/client/workspace/types.ts"
import type { Member } from "@vibeface/client/lib/node.ts"
import { AiSettings, docs } from "./ai-settings"
import { JoinRequests } from "./join"

export type DeviceRow = {
  id: string; name: string; kind: "node" | "client"; member?: Member; node?: NodeInfo
  /** The device this browser is connected through. */ entry: boolean
  browser: boolean; online?: boolean; bot: boolean; admin: boolean
}

export function useDeviceRows(): DeviceRow[] {
  const { directory, ready, identity } = useDevice()
  const { nodes, connectionOnline } = useWorkspace()
  const members = directory?.members ?? []
  const removed = new Set(directory?.revoked.map((item) => item.device_id))
  const ids = [...new Set([...members.map((member) => member.device_id), ...nodes.filter((node) => !node.revoked).map((node) => node.device_id)])].filter((id) => !removed.has(id))
  const rank = (row: DeviceRow) => row.browser ? 0 : row.entry ? 1 : row.kind === "node" ? 2 : 3
  return ids.map((id): DeviceRow => {
    const member = members.find((item) => item.device_id === id), node = nodes.find((item) => item.device_id === id)
    const kind = member?.kind ?? "node", entry = id === ready?.entry.device_id
    return {
      id, member, node, kind, name: member?.name || node?.name || (kind === "node" ? "电脑" : "浏览器"),
      entry, browser: id === identity?.deviceId,
      // Before the device list arrives, the computer this page talks through is as online as the connection.
      online: kind === "node" ? node?.online ?? (entry && connectionOnline) : undefined, bot: Boolean(node?.capabilities?.manager), admin: member?.manage ?? false,
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

/** `compact`: the list names only the exception, so an online row shows the dot alone. */
function Online({ online, compact = false }: { online: boolean; compact?: boolean }) {
  return <span title={compact && online ? "在线" : undefined} className={cn("flex shrink-0 items-center gap-1.5 text-meta", online ? "text-success" : "text-muted-foreground")}>
    <span aria-hidden="true" className={cn("size-2 rounded-full", online ? "bg-success" : "border border-muted-foreground")} /><span className={cn(compact && online && "sr-only")}>{online ? "在线" : "离线"}</span>
  </span>
}

function useEntryLabel() {
  const { ready } = useDevice()
  const { connectionOnline } = useWorkspace()
  if (ready && isLoopback(ready.entry.address)) return "这台电脑"
  return connectionOnline ? "当前连接" : "重连中"
}

function Hint({ icon: Icon, label }: { icon: LucideIcon; label: string }) {
  return <span title={label} className="flex shrink-0"><Icon aria-hidden="true" className="size-4 text-muted-foreground" /><span className="sr-only">{label}</span></span>
}

export function DevicesPage({ actions }: { actions?: ReactNode }) {
  const device = useDevice()
  const rows = useDeviceRows()
  const { nodes } = useWorkspace()
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
          {row.bot ? <Hint icon={Bot} label="能运行 Bot" /> : null}
          {row.admin ? <Hint icon={Crown} label="管理员" /> : null}
          {row.online !== undefined ? <Online online={row.online} compact /> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        </Link></li>
      })}</ul>}
    {device.directory?.revoked.length ? <Details label={`已移除的设备（${device.directory.revoked.length}）`}>
      <ul className="grid gap-1">{device.directory.revoked.map((item) => <li key={item.device_id} title={item.device_id} className="flex min-h-8 items-center gap-2">
        <terms.remove.icon aria-hidden="true" className="size-4 shrink-0" />
        <span className="min-w-0 truncate">{nodes.find((node) => node.device_id === item.device_id)?.name || `设备 ${item.device_id.slice(0, 4).toUpperCase()}`}</span>
      </li>)}</ul>
    </Details> : null}
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
  const { writeState } = useWorkspace()
  const navigate = useNavigate()
  const action = useAction()
  const [removing, setRemoving] = useState(false)
  const row = rows.find((item) => item.id === deviceId)
  const copied = writeState.conflicts.some((item) => item.author === deviceId) || Boolean(row?.entry && writeState.state === "forked")
  const RemoveIcon = terms.remove.icon
  const canManage = ready?.principal.can_manage === true
  const localSession = ready?.principal.source === "local_session"
  if (!row) return device.directory ? <EmptyState icon={MonitorSmartphone} title="找不到这台设备" action={<Button asChild variant="outline"><Link to="/devices">回到设备列表</Link></Button>} /> : <div className="mx-auto grid w-full max-w-3xl gap-4"><Skeleton className="h-16" /><Skeleton className="h-40" /></div>
  const Icon = deviceIcon(row)
  const address = row.entry ? ready?.entry.address ?? "" : ""
  const saved = device.entries.find((item) => item.device_id === row.id)

  return <div className="mx-auto grid w-full max-w-3xl gap-6">
    <header className="flex items-center gap-4">
      <span className="grid size-12 shrink-0 place-items-center rounded-xl bg-muted"><Icon aria-hidden="true" className="size-6" /></span>
      <div className="grid min-w-0 gap-1">
        <h2 className="truncate text-title font-semibold" title={row.name}>{row.name}</h2>
        <div className="flex flex-wrap items-center gap-2">
          {row.online !== undefined ? <Online online={row.online} /> : null}
          {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
          {row.admin ? <StatusPill icon={Crown}>管理员</StatusPill> : null}
        </div>
        {row.online === false ? <p className="text-meta text-muted-foreground">确认它开着，并且和这台设备网络互通</p> : null}
      </div>
    </header>

    {copied ? <p className="flex items-start gap-2 rounded-xl border border-warning-border bg-warning-soft p-3 text-warning">
      <Copy aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      <span>{row.entry ? "它的数据被复制过，已暂停。在另一台设备上移除它，再重新加入。" : canManage ? "它的数据被复制过。移除它，再在那台电脑上重新加入。" : "它的数据被复制过。请用管理设备移除它，再重新加入。"}</span>
    </p> : null}

    {row.kind === "node" && !copied ? <Section icon={terms.ai.icon} title={terms.ai.label}>
      {row.entry && (canManage || localSession) ? <AiSettings /> : <div className="grid gap-1">
        <p>{row.bot ? "能运行 Bot" : "还没配置 Bot"}</p>
        <p className="text-meta text-muted-foreground">{canManage ? "连到这台设备后才能修改" : "只有管理员能修改"}</p>
        {canManage && saved ? <Button type="button" variant="outline" className="mt-2 w-fit" disabled={action.busy !== null} onClick={() => void action.run("switch", () => device.connect(saved))}><ArrowRightLeft aria-hidden="true" />切换到它</Button> : null}
      </div>}
    </Section> : null}

    {row.entry ? <Section icon={Globe} title="访问地址">
      <p className="break-all font-mono text-meta">{address}</p>
      {isLoopback(address) || !address.startsWith("https:") ? <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-meta">
        <span className="flex items-center gap-1.5 text-warning"><TriangleAlert aria-hidden="true" className="size-4" />只能本机打开，手机和其他电脑连不上</span>
        <a className="font-medium underline underline-offset-4" href={`${docs}node.md#%E9%85%8D%E7%BD%AE%E5%85%AC%E5%85%B1%E5%85%A5%E5%8F%A3`} target="_blank" rel="noopener noreferrer">怎么设置</a>
      </p> : null}
      {canManage || localSession ? <label className="flex min-h-11 items-center justify-between gap-3 border-t pt-3">
        <span>本机免确认打开</span>
        <input type="checkbox" role="switch" className="switch" checked={device.localAutoAccess === true}
          disabled={action.busy !== null || device.localAutoAccess === null || localSession && !device.localAutoAccess}
          onChange={(event) => { const enabled = event.target.checked; void action.run("local", () => device.setLocalAutoAccess(enabled), "已保存") }} />
      </label> : null}
      {action.errorKey === "local" ? <InlineError error={action.error} /> : null}
    </Section> : null}

    <Details>
      <DetailRow label="设备编号" value={row.id} mono />
      {row.node?.capabilities && (row.node.capabilities.relay || row.node.capabilities.manager) ? <DetailRow label="设备能力" value={[row.node.capabilities.manager ? "运行 Bot" : null, row.node.capabilities.relay ? "转发" : null].filter(Boolean).join("、")} /> : null}
    </Details>

    {canManage && row.member && !row.entry && !row.browser ? removing ? <section className="grid gap-3 rounded-xl border border-danger-border p-4">
      <p>移除后这台设备无法再连接，已有的对话记录会保留。</p>
      <div className="flex flex-wrap justify-end gap-2">
        <Button type="button" variant="ghost" onClick={() => setRemoving(false)}>取消</Button>
        <Button type="button" variant="destructive" disabled={action.busy !== null} onClick={() => void action.run("remove", async () => { await device.revoke(row.id); toast("已移除", { tone: "success" }); void navigate({ to: "/devices" }) })}>
          <RemoveIcon aria-hidden="true" />{action.busy === "remove" ? "移除中" : "移除设备"}
        </Button>
      </div>
      {action.errorKey === "remove" ? <InlineError error={action.error} /> : null}
    </section> : <Button type="button" variant="ghost" className="-ms-3 w-fit text-danger" onClick={() => setRemoving(true)}><RemoveIcon aria-hidden="true" />移除设备</Button> : null}
  </div>
}
