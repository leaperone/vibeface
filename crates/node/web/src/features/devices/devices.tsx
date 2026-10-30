import { useEffect, useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ArrowRightLeft, Check, ChevronRight, Copy, Info, Monitor, Pencil, ShieldAlert, Smartphone, Tablet, X, type LucideIcon } from "lucide-react"
import { cn } from "@/lib/utils"
import { BusyIcon, DetailRow, EmptyState, InlineError, SettingsDisclosure, SettingsGroup, SettingsRow, Skeleton, toast, useAction } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { addOnManager, anotherManager, deviceCopy, soleBrowserManagerText } from "@vibeface/client/identity/copy.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { deviceForm, deviceOnline, isLoopback } from "@vibeface/client/workspace/labels.ts"
import { terms } from "@/lib/glossary"
import type { NodeInfo } from "@vibeface/client/workspace/types.ts"
import { botReadiness } from "@vibeface/client/workspace/model.ts"
import { entryAddress, type Member } from "@vibeface/client/lib/node.ts"
import { AiSettings } from "./ai-settings"
import { JoinRequests } from "./join"
import { localCopy } from "@/features/workspace/local-session"

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
  // Every device lists the same way: the one in hand first, then computers, phones and tablets, browsers.
  const rank = (row: DeviceRow) => row.browser ? 0 : row.entry ? 1 : row.kind === "node" ? 2 : deviceForm(row.kind, row.name) === "browser" ? 4 : 3
  return ids.map((id): DeviceRow => {
    const member = members.find((item) => item.device_id === id), node = nodes.find((item) => item.device_id === id)
    const kind = member?.kind ?? "node", entry = id === ready?.entry.device_id
    return {
      id, member, node, kind, name: member?.name || node?.name || (kind === "node" ? "电脑" : "浏览器"),
      entry, browser: id === identity?.deviceId,
      online: deviceOnline(kind, node, entry, connectionOnline), bot: botReadiness(node), admin: member?.manage ?? false,
    }
  }).sort((a, b) => rank(a) - rank(b))
}

/** One icon per kind of device; which computer this browser goes through is a tag, not another icon. */
export function deviceIcon(row: Pick<DeviceRow, "kind" | "name">): LucideIcon {
  if (row.kind === "node") return terms.thisComputer.icon
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
  if (ready && isLoopback(entryAddress(ready.entry))) return terms.thisComputer.label
  return connectionOnline ? "当前连接" : "重连中"
}

export function DevicesPage({ joins = true }: { joins?: boolean }) {
  const device = useDevice()
  const rows = useDeviceRows()
  const entryLabel = useEntryLabel()
  const [error, setError] = useState<unknown>(null)
  useEffect(() => { if (device.ready) device.refreshDirectory().catch(setError) }, [device.ready])
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    {joins ? <JoinRequests /> : null}
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
              {row.bot ? <Tag><terms.canRunBot.icon aria-hidden="true" className={cn("size-3.5", row.bot === "needs_setup" && "text-warning")} />{row.bot === "ready" ? "Bot" : row.bot === "starting" ? "Bot 启动中" : "Bot 需设置"}</Tag> : null}
              {row.admin ? <Tag title={terms.admin.label}><terms.admin.icon aria-hidden="true" className="size-3.5" />{terms.admin.tag}</Tag> : null}
            </span>
          </span>
          {row.online !== undefined ? <Online online={row.online} compact /> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        </Link></li>
      })}</ul>}
    <SoleBrowserManager rows={rows} />
    {device.ready && !device.ready.principal.can_manage && rows.length ? <p className="flex min-w-0 items-center gap-1.5 px-1 text-meta text-muted-foreground">
      <terms.admin.icon aria-hidden="true" className="size-4 shrink-0" /><span className="min-w-0 truncate">{addOnManager(rows.filter((row) => row.admin).map((row) => row.name))}</span>
    </p> : null}
  </div>
}

/**
 * One line while the only management device is a browser: clearing its site data takes management with it. A manager
 * is led to the device to promote, or to 添加设备 when there is none yet.
 */
function SoleBrowserManager({ rows }: { rows: DeviceRow[] }) {
  const { ready } = useDevice()
  const admins = rows.filter((row) => row.admin)
  const sole = admins.length === 1 ? admins[0] : null
  if (!sole || sole.kind !== "client" || deviceForm(sole.kind, sole.name) !== "browser") return null
  const next = rows.find((row) => row.kind === "client" && !row.admin)
  const link = "hit-area relative shrink-0 font-medium text-foreground underline underline-offset-4"
  return <p className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 px-1 text-meta text-warning">
    <ShieldAlert aria-hidden="true" className="size-4 shrink-0" />
    <span className="min-w-0">{soleBrowserManagerText(sole.browser ? "这个浏览器" : `「${sole.name}」`)}</span>
    {ready?.principal.can_manage ? next
      ? <Link to="/devices/$deviceId" params={{ deviceId: next.id }} className={link}>{anotherManager}</Link>
      : <Link to="/devices" search={{ add: true }} className={link}>{anotherManager}</Link> : null}
  </p>
}

/** Captioned like a settings group, so a device reads the same as 设置. `bare`: the content brings its own cards. */
function Section({ title, bare = false, children }: { title: string; bare?: boolean; children: ReactNode }) {
  return <section className="grid gap-2">
    <h3 className="px-1 text-meta font-medium text-muted-foreground">{title}</h3>
    {bare ? children : <div className="grid gap-4 rounded-xl border bg-card p-4 shadow-xs">{children}</div>}
  </section>
}

/** The name heads the device page; a management device taps it to rename the device in place. */
function DeviceTitle({ row, editable }: { row: DeviceRow; editable: boolean }) {
  const device = useDevice()
  const action = useAction()
  const [draft, setDraft] = useState<string | null>(null)
  const close = () => { setDraft(null); action.clear() }
  if (draft === null) return <h2 className="min-w-0 text-title font-semibold">
    {editable ? <button type="button" className="group flex max-w-full min-w-0 items-center gap-2 rounded-md text-start" title="改名" onClick={() => setDraft(row.name)}>
      <span className="truncate">{row.name}</span>
      <Pencil aria-hidden="true" className="size-4 shrink-0 text-muted-foreground opacity-60 transition-opacity group-hover:opacity-100" />
      <span className="sr-only">改名</span>
    </button> : <span className="block truncate" title={row.name}>{row.name}</span>}
  </h2>
  const save = () => {
    const name = draft.trim()
    if (name === row.name) return close()
    void action.run("rename", async () => { await device.renameDevice(row.id, name); setDraft(null) }, "已改名")
  }
  return <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); save() }}>
    <div className="flex min-w-0 items-center gap-1">
      <Input aria-label="设备名称" autoFocus required maxLength={128} className="h-9 min-w-0 text-title font-semibold" value={draft} disabled={action.busy !== null}
        onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Escape") close() }} />
      <Button type="submit" variant="ghost" size="icon" aria-label="保存" title="保存" disabled={action.busy !== null || !draft.trim()}><BusyIcon busy={action.busy === "rename"} icon={Check} /></Button>
      <Button type="button" variant="ghost" size="icon" aria-label="取消" title="取消" disabled={action.busy !== null} onClick={close}><X aria-hidden="true" /></Button>
    </div>
    {action.errorKey === "rename" ? <InlineError error={action.error} /> : null}
  </form>
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
  // Taking management away asks first; giving it does not.
  const [unmanaging, setUnmanaging] = useState(false)
  const row = rows.find((item) => item.id === deviceId)
  const entryName = rows.find((item) => item.entry)?.name
  const copied = writeState.conflicts.some((item) => item.author === deviceId) || Boolean(row?.entry && writeState.state === "forked")
  const RemoveIcon = terms.remove.icon
  const canManage = ready?.principal.can_manage === true
  const localSession = ready?.principal.source === "local_session"
  if (!row) return device.directory ? <EmptyState icon={terms.myDevices.icon} title="找不到这个设备" action={<Button asChild variant="outline"><Link to="/devices">回到设备列表</Link></Button>} /> : <div className="mx-auto grid w-full max-w-(--page-width) gap-4"><Skeleton className="h-16" /><Skeleton className="h-40" /></div>
  const Icon = deviceIcon(row)
  const address = row.entry && ready ? entryAddress(ready.entry) : ""
  const saved = device.entries.find((item) => item.device_id === row.id)

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <header className="flex items-center gap-4">
      <span className="grid size-12 shrink-0 place-items-center rounded-2xl bg-muted ring-1 ring-border"><Icon aria-hidden="true" className="size-6" /></span>
      <div className="grid min-w-0 gap-1">
        <DeviceTitle row={row} editable={canManage && Boolean(row.member)} />
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
        <p>{deviceCopy.botStatus[deviceCopy.botState(row.bot, row.online)]}</p>
        <p className="text-meta text-muted-foreground">{canManage ? "连到它后才能修改" : `只有${terms.admin.label}能修改`}</p>
        {canManage && saved ? <Button type="button" variant="outline" className="mt-2 w-fit" disabled={action.busy !== null} onClick={() => void action.run("switch", () => device.connect(saved))}><ArrowRightLeft aria-hidden="true" />切换到它</Button> : null}
      </div>
    </Section> : null}

    {row.entry ? <Section title="在哪能打开">
      {isLoopback(address) || !address.startsWith("https:")
        ? <p className="flex items-center gap-2"><terms.thisComputer.icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />只能在{isLoopback(address) ? "这" : "那"}台电脑上打开</p>
        : <p className="flex items-center gap-2"><terms.address.icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />在哪都能打开</p>}
      {canManage || localSession ? <div className="grid gap-2 border-t pt-3">
        <label className="flex min-h-11 items-center justify-between gap-3">
          <span className="grid gap-0.5"><span>{isLoopback(address) ? "在这台电脑上免加入打开" : "在那台电脑上免加入打开"}</span><span className="text-meta text-muted-foreground">{localCopy.note}</span></span>
          <input type="checkbox" role="switch" className="switch" checked={device.localAutoAccess === true} aria-describedby="local-access-risk"
            disabled={action.busy !== null || device.localAutoAccess === null || localSession && !device.localAutoAccess}
            onChange={(event) => { const enabled = event.target.checked; void action.run("local", () => device.setLocalAutoAccess(enabled), "已保存") }} />
        </label>
        <p id="local-access-risk" className={cn("flex items-center gap-1.5 text-meta", device.localAutoAccess ? "text-warning" : "text-muted-foreground")}>
          <ShieldAlert aria-hidden="true" className="size-4 shrink-0" />{isLoopback(address) ? "这" : "那"}台电脑上的人和程序不用批准就能看对话、改设置、替你回 Bot
        </p>
      </div> : null}
      {action.errorKey === "local" ? <InlineError error={action.error} /> : null}
    </Section> : null}

    <SettingsGroup>
      <SettingsDisclosure icon={Info} label="详细信息">
        <div className="grid gap-3 text-meta text-muted-foreground">
          {address ? <DetailRow label={terms.address.label} value={address} mono /> : null}
          <DetailRow label="设备编号" value={row.id} mono />
        </div>
      </SettingsDisclosure>
      {canManage && row.member?.kind === "client" && !row.browser ? <SettingsRow icon={terms.admin.icon} label={unmanaging ? deviceCopy.unmanageAsk(row.name) : terms.admin.label} tone={unmanaging ? "danger" : undefined} stacked={unmanaging}
        description={unmanaging ? deviceCopy.unmanageNote(rows.filter((item) => item.admin && item.id !== row.id).length) : deviceCopy.manageHint}>
        {unmanaging ? <>
          <Button type="button" size="sm" variant="ghost" autoFocus onClick={() => setUnmanaging(false)}>取消</Button>
          <Button type="button" size="sm" variant="destructive" disabled={action.busy !== null} aria-busy={action.busy === "manage" || undefined} onClick={() => void action.run("manage", async () => { await device.setManage(row.id, false); setUnmanaging(false) }, "已取消管理")}>
            <BusyIcon busy={action.busy === "manage"} />{deviceCopy.unmanage}
          </Button>
        </> : <input type="checkbox" role="switch" className="switch" aria-label={terms.admin.label} checked={row.admin}
          disabled={action.busy !== null || row.admin && !rows.some((item) => item.admin && item.id !== row.id)}
          onChange={(event) => { if (!event.target.checked) setUnmanaging(true); else void action.run("manage", () => device.setManage(row.id, true), "已设为管理设备") }} />}
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
