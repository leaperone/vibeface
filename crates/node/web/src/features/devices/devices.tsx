import { useEffect, useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ArrowRightLeft, ChevronRight, Copy, Info, Laptop, Monitor, Server, ShieldAlert, Smartphone, Tablet, type LucideIcon } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, DetailRow, EmptyState, InlineError, SettingsDisclosure, SettingsGroup, SettingsRow, Skeleton, toast, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { deviceCopy } from "@vibeface/client/identity/copy.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { deviceForm, deviceOnline, isLoopback } from "@vibeface/client/workspace/labels.ts"
import { terms } from "@/lib/glossary"
import type { NodeInfo } from "@vibeface/client/workspace/types.ts"
import { botReadiness } from "@vibeface/client/workspace/model.ts"
import type { Member } from "@vibeface/client/lib/node.ts"
import { AiSettings, docs } from "./ai-settings"
import { JoinRequests } from "./join"

type DeviceRow = {
  id: string; name: string; kind: "node" | "client"; member?: Member; node?: NodeInfo
  /** The device this browser is connected through. */ entry: boolean
  browser: boolean; online?: boolean; bot: ReturnType<typeof botReadiness>; admin: boolean
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
      online: deviceOnline(kind, node, entry && connectionOnline), bot: botReadiness(node), admin: member?.manage ?? false,
    }
  }).sort((a, b) => rank(a) - rank(b))
}

export function deviceIcon(row: Pick<DeviceRow, "kind" | "name" | "entry">): LucideIcon {
  if (row.kind === "node") return row.entry ? Laptop : Server
  const form = deviceForm(row.kind, row.name)
  return form === "tablet" ? Tablet : form === "phone" ? Smartphone : Monitor
}

function Tag({ children, title }: { children: ReactNode; title?: string }) {
  return <span title={title} className="inline-flex shrink-0 items-center gap-1 rounded-md bg-muted px-1.5 text-meta text-muted-foreground">{children}</span>
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
  if (ready && isLoopback(ready.entry.address)) return terms.thisComputer.label
  return connectionOnline ? "当前连接" : "重连中"
}

export function DevicesPage() {
  const device = useDevice()
  const rows = useDeviceRows()
  const entryLabel = useEntryLabel()
  const [error, setError] = useState<unknown>(null)
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(setError) }, [device.ready])
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <JoinRequests />
    <InlineError error={error} action={{ label: "重试", onClick: () => { setError(null); device.refreshDirectory().catch(setError) } }} />
    {!rows.length ? <div className="grid gap-2" aria-busy="true"><Skeleton className="h-14" /><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
      : <ul className="divide-y overflow-hidden rounded-xl border bg-card shadow-xs">{rows.map((row) => {
        const Icon = deviceIcon(row)
        return <li key={row.id}><Link to="/devices/$deviceId" params={{ deviceId: row.id }} className="interactive-row flex min-h-14 items-center gap-3 px-4 py-2">
          <Icon aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" />
          {/* On a phone the tag drops under the name, so the name keeps the width. */}
          <span className="flex min-w-0 flex-1 flex-col items-start gap-x-2 gap-y-0.5 sm:flex-row sm:items-center"><span className="line-clamp-2 min-w-0 font-medium [overflow-wrap:anywhere]">{row.name}</span>
            <span className="flex flex-wrap gap-1 empty:hidden">
              {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
              {row.bot ? <Tag><terms.canRunBot.icon aria-hidden="true" className={cn("size-3.5", row.bot === "needs_setup" && "text-warning")} />{row.bot === "ready" ? "Bot" : "Bot 需设置"}</Tag> : null}
              {row.admin ? <Tag title={terms.admin.label}><terms.admin.icon aria-hidden="true" className="size-3.5" />{terms.admin.tag}</Tag> : null}
            </span>
          </span>
          {row.online !== undefined ? <Online online={row.online} compact /> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        </Link></li>
      })}</ul>}
  </div>
}

/** Captioned like a settings group, so a device reads the same as 设置. `bare`: the content brings its own cards. */
function Section({ title, bare = false, children }: { title: string; bare?: boolean; children: ReactNode }) {
  return <section className="grid gap-2">
    <h3 className="px-1 text-meta font-medium text-muted-foreground">{title}</h3>
    {bare ? children : <div className="grid gap-4 rounded-xl border bg-card p-4 shadow-xs">{children}</div>}
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
  const entryName = rows.find((item) => item.entry)?.name
  const copied = writeState.conflicts.some((item) => item.author === deviceId) || Boolean(row?.entry && writeState.state === "forked")
  const RemoveIcon = terms.remove.icon
  const canManage = ready?.principal.can_manage === true
  const localSession = ready?.principal.source === "local_session"
  if (!row) return device.directory ? <EmptyState icon={terms.myDevices.icon} title="找不到这个设备" action={<Button asChild variant="outline"><Link to="/devices">回到设备列表</Link></Button>} /> : <div className="mx-auto grid w-full max-w-(--page-width) gap-4"><Skeleton className="h-16" /><Skeleton className="h-40" /></div>
  const Icon = deviceIcon(row)
  const address = row.entry ? ready?.entry.address ?? "" : ""
  const saved = device.entries.find((item) => item.device_id === row.id)

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <header className="flex items-center gap-4">
      <span className="grid size-12 shrink-0 place-items-center rounded-2xl bg-muted ring-1 ring-border"><Icon aria-hidden="true" className="size-6" /></span>
      <div className="grid min-w-0 gap-1">
        <h2 className="truncate text-title font-semibold" title={row.name}>{row.name}</h2>
        <div className="flex flex-wrap items-center gap-2">
          {row.online !== undefined ? <Online online={row.online} /> : null}
          {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
          {row.admin ? <Tag title={terms.admin.label}><terms.admin.icon aria-hidden="true" className="size-3.5" />{terms.admin.tag}</Tag> : null}
        </div>
        {row.online === false ? <p className="text-meta text-muted-foreground">{entryName && !row.entry ? `确认它开着，并且能连到「${entryName}」` : deviceCopy.offline}</p> : null}
      </div>
    </header>

    {copied ? <p className="flex items-start gap-2 rounded-xl border border-warning-border bg-warning-soft p-3 text-warning">
      <Copy aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      <span>{row.entry ? "它的数据被复制过，已暂停。在另一台设备上移除它，再重新加入。" : canManage ? "它的数据被复制过。移除它，再在那台电脑上重新加入。" : "它的数据被复制过。用管理设备移除它，再重新加入。"}</span>
    </p> : null}

    {row.kind === "node" && !copied ? row.entry && (canManage || localSession) ? <Section title={terms.ai.label} bare><AiSettings /></Section> : <Section title={terms.ai.label}>
      <div className="grid gap-1">
        <p>{row.bot === "ready" ? terms.canRunBot.label : row.bot ? "Bot 需设置" : "还没配置 Bot"}</p>
        <p className="text-meta text-muted-foreground">{canManage ? "连到它后才能修改" : `只有${terms.admin.label}能修改`}</p>
        {canManage && saved ? <Button type="button" variant="outline" className="mt-2 w-fit" disabled={action.busy !== null} onClick={() => void action.run("switch", () => device.connect(saved))}><ArrowRightLeft aria-hidden="true" />切换到它</Button> : null}
      </div>
    </Section> : null}

    {row.entry ? <Section title={terms.address.label}>
      <p className="break-all font-mono text-meta">{address}</p>
      {isLoopback(address) || !address.startsWith("https:") ? <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-meta">
        <span className="flex items-center gap-1.5 text-muted-foreground"><Monitor aria-hidden="true" className="size-4 shrink-0" />只能在本机打开</span>
        <a className="hit-area relative font-medium underline underline-offset-4" href={`${docs}node.md#%E9%85%8D%E7%BD%AE%E5%85%AC%E5%85%B1%E5%85%A5%E5%8F%A3`} target="_blank" rel="noopener noreferrer">怎么设置</a>
      </p> : null}
      {canManage || localSession ? <div className="grid gap-2 border-t pt-3">
        <label className="flex min-h-11 items-center justify-between gap-3">
          <span className="grid gap-0.5"><span>{isLoopback(address) ? "在这台电脑上直接打开" : "在那台电脑上直接打开"}</span><span className="text-meta text-muted-foreground">不用加入链接</span></span>
          <input type="checkbox" role="switch" className="switch" checked={device.localAutoAccess === true} aria-describedby={device.localAutoAccess ? "local-access-risk" : undefined}
            disabled={action.busy !== null || device.localAutoAccess === null || localSession && !device.localAutoAccess}
            onChange={(event) => { const enabled = event.target.checked; void action.run("local", () => device.setLocalAutoAccess(enabled), "已保存") }} />
        </label>
        {/* Once on, nobody on that computer is asked before opening the network. */}
        {device.localAutoAccess ? <p id="local-access-risk" className="flex items-center gap-1.5 text-meta text-warning">
          <ShieldAlert aria-hidden="true" className="size-4 shrink-0" />{isLoopback(address) ? "这台电脑上的人都能直接打开" : "那台电脑上的人都能直接打开"}
        </p> : null}
      </div> : null}
      {action.errorKey === "local" ? <InlineError error={action.error} /> : null}
    </Section> : null}

    <SettingsGroup>
      <SettingsDisclosure icon={Info} label="详细信息">
        <div className="grid gap-3 text-meta text-muted-foreground">
          <DetailRow label="设备编号" value={row.id} mono />
          {row.node?.capabilities && (row.node.capabilities.relay || row.node.capabilities.manager) ? <DetailRow label="设备能力" value={[row.node.capabilities.manager ? "运行 Bot" : null, row.node.capabilities.relay ? "转发" : null].filter(Boolean).join("、")} /> : null}
        </div>
      </SettingsDisclosure>
      {canManage && row.member?.kind === "client" && !row.browser ? <SettingsRow icon={terms.admin.icon} label={terms.admin.label}>
        <input type="checkbox" role="switch" className="switch" aria-label={terms.admin.label} checked={row.admin}
          disabled={action.busy !== null || row.admin && !rows.some((item) => item.admin && item.id !== row.id)}
          onChange={(event) => { const manage = event.target.checked; void action.run("manage", () => device.setManage(row.id, manage), "已保存") }} />
      </SettingsRow> : null}
      {action.errorKey === "manage" ? <div className="px-4 py-3"><InlineError error={action.error} /></div> : null}
      {canManage && row.member && !row.entry && !row.browser ? <SettingsRow icon={RemoveIcon} tone="danger" label={terms.remove.label} description={removing ? deviceCopy.removeNotice(Boolean(row.bot)) : undefined} stacked onClick={removing ? undefined : () => setRemoving(true)}>
        {removing ? <>
          <Button type="button" size="sm" variant="ghost" onClick={() => setRemoving(false)}>取消</Button>
          <Button type="button" size="sm" variant="destructive" disabled={action.busy !== null} aria-busy={action.busy === "remove" || undefined} onClick={() => void action.run("remove", async () => { await device.revoke(row.id); toast(`已移除「${row.name}」`, { tone: "success" }); void navigate({ to: "/devices" }) })}>
            <BusyIcon busy={action.busy === "remove"} />移除
          </Button>
        </> : null}
      </SettingsRow> : null}
      {action.errorKey === "remove" ? <div className="px-4 py-3"><InlineError error={action.error} /></div> : null}
    </SettingsGroup>
  </div>
}
