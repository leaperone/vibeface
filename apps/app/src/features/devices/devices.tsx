import { BusyIcon,DetailRow,EmptyState,InlineError,SettingsDisclosure,SettingsGroup,SettingsRow,Skeleton,toast,useAction } from "@/components/arc/product-kit"
import { RoutedAction } from "@/components/arc/routed-action"
import deviceStyles from "@/components/arc/task-compositions.module.css"
import { localCopy } from "@/features/workspace/local-session"
import { terms } from "@/lib/glossary"
import { cn } from "@/lib/utils"
import { Link,useNavigate } from "@tanstack/react-router"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { addOnManager,anotherManager,deviceCopy,editOnManager,soleBrowserManagerText } from "@vibeface/client/identity/copy.ts"
import { entryAddress,throughEntry,type Member } from "@vibeface/client/lib/node.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { deviceForm,deviceOnline,isLoopback } from "@vibeface/client/workspace/labels.ts"
import { botReadiness } from "@vibeface/client/workspace/model.ts"
import { describeNode } from "@vibeface/client/workspace/node-diagnostics.ts"
import type { NodeInfo } from "@vibeface/client/workspace/types.ts"
import { useNodeChecks } from "@vibeface/client/workspace/use-node-checks.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Input } from "@vibeface/uiarc/input/input.tsx"
import { Switch } from "@vibeface/uiarc/switch/switch.tsx"
import { ArrowRightLeft,Check,ChevronRight,Copy,Info,Pencil,RadioTower,ShieldAlert,Smartphone,Tablet,X,type LucideIcon } from "lucide-react"
import { useEffect,useState,type ReactNode } from "react"
import { AiSettings } from "./ai-settings"
import { JoinRequests } from "./join"
import { NodeConnection,NodeStatus } from "./node-connection"
import { RelaySettings,ShareLink,relayLabel } from "./relay-settings"

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
    const kind = member?.kind ?? "node", entry = id === ready?.entry.device_id, browser = id === identity?.deviceId
    return {
      id, member, node, kind, name: member?.name || node?.name || (kind === "node" ? "电脑" : "浏览器"),
      entry, browser,
      online: deviceOnline(kind, node, entry, connectionOnline, browser), bot: botReadiness(node), admin: member?.manage ?? false,
    }
  }).sort((a, b) => rank(a) - rank(b))
}

/** One icon per kind of device; which computer this browser goes through is a tag, not another icon. */
export function deviceIcon(row: Pick<DeviceRow, "kind" | "name">): LucideIcon {
  if (row.kind === "node") return terms.thisComputer.icon
  const form = deviceForm(row.kind, row.name)
  return form === "tablet" ? Tablet : form === "phone" ? Smartphone : terms.browser.icon
}

function Tag({ children, title }: { children: ReactNode; title?: string }) {
  return <span title={title} className="inline-flex shrink-0 items-center gap-1 rounded-control bg-surface-muted px-1.5 text-sm text-secondary">{children}</span>
}

/** `compact`: the list names only the exception, so an online row shows the dot alone. */
function Online({ online, compact = false }: { online: boolean; compact?: boolean }) {
  return <span title={compact && online ? "在线" : undefined} className={cn("flex shrink-0 items-center gap-1.5 text-sm", online ? "text-success" : "text-secondary")}>
    <span aria-hidden="true" className={cn("size-2 rounded-pill", online ? "bg-success" : "border border-strong")} /><span className={cn(compact && online && "sr-only")}>{online ? "在线" : "离线"}</span>
  </span>
}

function useEntryLabel() {
  const { ready } = useDevice()
  const { connectionOnline } = useWorkspace()
  if (ready && isLoopback(entryAddress(ready.entry))) return terms.thisComputer.label
  return connectionOnline ? "当前连接" : "重连中"
}

const inForeground = () => typeof document !== "undefined" && document.visibilityState === "visible"

function useNodeDiagnostics(rows: DeviceRow[], visible: boolean) {
  const { ready } = useDevice()
  const { connectionOnline, nodeReport, refreshNodes } = useWorkspace()
  const active = visible && connectionOnline && ready?.isCurrent() === true
  const checks = useNodeChecks({ report: nodeReport, refreshNodes, active, foreground: inForeground })
  useEffect(() => {
    if (!active || !refreshNodes || typeof document === "undefined") return
    document.addEventListener("visibilitychange", checks.refresh)
    return () => document.removeEventListener("visibilitychange", checks.refresh)
  }, [active, refreshNodes, checks.refresh])
  const describe = (row: DeviceRow) => describeNode({
    target: { deviceId: row.id, name: row.name, entry: row.entry },
    ready,
    connected: connectionOnline,
    report: nodeReport,
    fresh: checks.fresh,
    checking: checks.checking,
    clientLabel: "这个浏览器",
    nameFor: (id) => rows.find((item) => item.id === id)?.name,
  })
  return { describe, checks, canRefresh: active && Boolean(refreshNodes) }
}

export function DevicesPage({ joins = true }: { joins?: boolean }) {
  const device = useDevice()
  const workspace = useWorkspace()
  const rows = useDeviceRows()
  const entryLabel = useEntryLabel()
  const connected = rows.find((row) => row.entry && row.kind === "node")
  const local = device.ready && isLoopback(entryAddress(device.ready.entry))
  const canEdit = device.ready?.principal.can_manage || device.ready?.principal.source === "local_session"
  const [error, setError] = useState<unknown>(null)
  const [directoryRead, setDirectoryRead] = useState<typeof device.ready>(null)
  const [settingsRead, setSettingsRead] = useState<{ connection: typeof device.ready; deviceId: string } | null>(null)
  const [shownConnection, setShownConnection] = useState<typeof device.ready>(null)
  useEffect(() => {
    if (!device.ready) return
    let active = true
    setError(null)
    void device.refreshDirectory().catch((cause) => { if (active) setError(cause) }).finally(() => { if (active) setDirectoryRead(device.ready) })
    return () => { active = false }
  }, [device.ready])
  // The signed directory arrives during authentication; an empty `nodes` list before the
  // workspace's first read does not yet describe the computers available to this browser.
  const readUnavailable = Boolean(workspace.error) || !workspace.connectionOnline
  const nodesRead = workspace.nodesRead || readUnavailable
  const initialRead = directoryRead === device.ready && nodesRead && (readUnavailable || !connected || !canEdit || settingsRead?.connection === device.ready && settingsRead.deviceId === connected.id)
  const contentReady = Boolean(device.ready && (shownConnection === device.ready || initialRead))
  useEffect(() => { if (initialRead) setShownConnection(device.ready) }, [device.ready, initialRead])
  const { describe } = useNodeDiagnostics(rows, contentReady && rows.some((row) => row.kind === "node"))
  const entryDiagnostics = connected ? describe(connected) : null
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6" aria-busy={!contentReady}>
    {joins ? <JoinRequests /> : null}
    <InlineError error={error} action={{ label: "重试", onClick: () => { setError(null); device.refreshDirectory().catch(setError) } }} />
    {!contentReady ? <p role="status" className="flex items-center gap-2 px-1 text-sm text-secondary"><BusyIcon busy />正在读取设备与 AI 服务…</p> : null}
    <div hidden={!contentReady} className={contentReady ? "grid gap-6" : undefined}>
    {connected ? <section className="grid min-w-0 gap-6" aria-label="当前连接的电脑">
      <header className={deviceStyles.deviceTitle}>
        <terms.thisComputer.icon aria-hidden="true" className="size-6 text-secondary" />
        <div className="grid min-w-0 flex-1 basis-32 gap-1">
          <p className="text-sm text-secondary">当前连接的电脑</p>
          <h2 className="text-lg font-medium [overflow-wrap:anywhere]">{connected.name}</h2>
          <div className="flex flex-wrap items-center gap-2"><Tag>{entryLabel}</Tag>{entryDiagnostics ? <NodeStatus status={entryDiagnostics.status} /> : null}</div>
        </div>
        <RoutedAction variant="ghost" to="/devices/$deviceId" params={{ deviceId: connected.id }} title="详细设置"><span className="max-sm:sr-only">详细设置</span><ChevronRight aria-hidden="true" /></RoutedAction>
      </header>
      {canEdit ? <AiSettings key={connected.id} compact targetName={local ? undefined : connected.name} onInitialRead={() => setSettingsRead({ connection: device.ready, deviceId: connected.id })} /> : <p className="px-1 text-sm text-secondary">{editOnManager(rows.filter((row) => row.admin).map((row) => row.name))}</p>}
    </section> : null}
    {!rows.length ? device.directory ? <EmptyState icon={terms.myDevices.icon} title="还没有设备" /> : <div className="grid gap-2" aria-busy="true"><Skeleton className="h-14" /><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
      : [{ title: "电脑", items: rows.filter((row) => row.kind === "node") }, { title: "手机与浏览器", items: rows.filter((row) => row.kind === "client") }].filter((group) => group.items.length).map((group) => <section key={group.title} className="grid gap-2">
        <h2 className="px-1 text-sm font-medium text-secondary">{group.title}</h2>
        <ul className={deviceStyles.deviceGroup}>{group.items.map((row) => {
        const Icon = deviceIcon(row)
        const diagnostics = row.kind === "node" ? describe(row) : null
        const path = diagnostics?.pathLabel ?? (diagnostics?.status === "online" ? "路径未知" : null)
        return <li key={row.id}><Link to="/devices/$deviceId" params={{ deviceId: row.id }} className={deviceStyles.deviceRow}>
          <Icon aria-hidden="true" className="size-5 shrink-0 text-secondary" />
          <span className="grid min-w-0 flex-1 gap-1">
            <span className="flex min-w-0 flex-col items-start gap-x-2 gap-y-0.5 sm:flex-row sm:flex-wrap sm:items-center">
              <span className="line-clamp-2 min-w-0 font-medium [overflow-wrap:anywhere]">{row.name}</span>
              <span className="flex flex-wrap gap-1 empty:hidden">
                {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
                {row.bot ? <Tag><terms.canRunBot.icon aria-hidden="true" className={cn("size-3.5", row.bot === "needs_setup" && "text-warning")} />{row.bot === "ready" ? "Bot" : row.bot === "unverified" ? "Bot 未验证，可尝试" : row.bot === "starting" ? "Bot 启动中" : "Bot 需设置"}</Tag> : null}
                {row.admin ? <Tag title={terms.admin.label}><terms.admin.icon aria-hidden="true" className="size-3.5" />{terms.admin.tag}</Tag> : null}
              </span>
            </span>
            {diagnostics ? <span className="flex min-w-0 flex-wrap items-baseline gap-x-3 gap-y-0.5">
              <NodeStatus status={diagnostics.status} />
              {path ? <span className="min-w-0 text-sm text-secondary [overflow-wrap:anywhere]">{path}</span> : null}
            </span> : null}
          </span>
          {!diagnostics && row.online !== undefined ? <Online online={row.online} compact /> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-secondary" />
        </Link></li>
      })}</ul></section>)}
    <SoleBrowserManager rows={rows} />
    {device.ready && !device.ready.principal.can_manage && rows.length ? <p className="flex min-w-0 items-center gap-1.5 px-1 text-sm text-secondary">
      <terms.admin.icon aria-hidden="true" className="size-4 shrink-0" /><span className="min-w-0 truncate">{addOnManager(rows.filter((row) => row.admin).map((row) => row.name))}</span>
    </p> : null}
    </div>
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
  // Only a manager can act on it; anywhere else it is a risk to a device this one cannot help.
  if (!ready?.principal.can_manage || !sole || sole.kind !== "client" || deviceForm(sole.kind, sole.name) !== "browser") return null
  const next = rows.find((row) => row.kind === "client" && !row.admin)
  const link = "hit-area relative shrink-0 font-medium text-foreground underline underline-offset-4"
  return <p className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 px-1 text-sm text-warning">
    <ShieldAlert aria-hidden="true" className="size-4 shrink-0" />
    <span className="min-w-0">{soleBrowserManagerText(sole.browser ? "这个浏览器" : `「${sole.name}」`)}</span>
    {next
      ? <Link to="/devices/$deviceId" params={{ deviceId: next.id }} className={link}>{anotherManager}</Link>
      : <Link to="/devices" search={{ add: true, manage: true }} className={link}>{anotherManager}</Link>}
  </p>
}

/** Captioned like a settings group, so a device reads the same as 设置. `bare`: the content brings its own cards. */
function Section({ title, bare = false, children }: { title: string; bare?: boolean; children: ReactNode }) {
  return <section className={deviceStyles.section}>
    <h3 className="text-lg font-medium">{title}</h3>
    {bare ? children : <div className={deviceStyles.sectionBody}>{children}</div>}
  </section>
}

/** The name heads the device page; a management device taps it to rename the device in place. */
function DeviceTitle({ row, editable }: { row: DeviceRow; editable: boolean }) {
  const device = useDevice()
  const action = useAction()
  const [draft, setDraft] = useState<string | null>(null)
  const close = () => { setDraft(null); action.clear() }
  if (draft === null) return <h2 className="min-w-0 text-xl font-medium">
    {editable ? <Button variant="ghost" type="button" className="group flex max-w-full min-w-0 items-center gap-2 text-start" title="改名" onClick={() => setDraft(row.name)}>
      <span className="truncate">{row.name}</span>
      <Pencil aria-hidden="true" className="size-4 shrink-0 text-secondary opacity-60 transition-opacity group-hover:opacity-100" />
      <span className="sr-only">改名</span>
    </Button> : <span className="block truncate" title={row.name}>{row.name}</span>}
  </h2>
  const save = () => {
    const name = draft.trim()
    if (name === row.name) return close()
    void action.run("rename", async () => { await device.renameDevice(row.id, name); setDraft(null) }, "已改名")
  }
  return <form className="grid gap-1" onSubmit={(event) => { event.preventDefault(); save() }}>
    <div className="flex min-w-0 items-center gap-1">
      <Input label="设备名称" aria-label="设备名称" autoFocus required maxLength={128} className="min-w-0" value={draft} disabled={action.busy !== null}
        onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Escape") close() }} />
      <Button type="submit" variant="ghost" size="md" aria-label="保存" title="保存" disabled={action.busy !== null || !draft.trim()}><BusyIcon busy={action.busy === "rename"} icon={Check} /></Button>
      <Button type="button" variant="ghost" size="md" aria-label="取消" title="取消" disabled={action.busy !== null} onClick={close}><X aria-hidden="true" /></Button>
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
  const { describe, checks, canRefresh } = useNodeDiagnostics(rows, row?.kind === "node")
  const entryName = rows.find((item) => item.entry)?.name
  const copied = writeState.conflicts.some((item) => item.author === deviceId) || Boolean(row?.entry && writeState.state === "forked")
  const RemoveIcon = terms.remove.icon
  const canManage = ready?.principal.can_manage === true
  const localSession = ready?.principal.source === "local_session"
  if (!row) return device.directory ? <EmptyState icon={terms.myDevices.icon} title="找不到这个设备" action={<RoutedAction variant="secondary" to="/devices">回到设备列表</RoutedAction>} /> : <div className="mx-auto grid w-full max-w-(--page-width) gap-4"><Skeleton className="h-16" /><Skeleton className="h-40" /></div>
  const Icon = deviceIcon(row)
  const diagnostics = row.kind === "node" ? describe(row) : null
  const address = row.entry && ready ? entryAddress(ready.entry) : ""
  const target = device.entries.find((item) => item.device_id === row.id) ?? (ready && row.member ? throughEntry(ready.entry, row.id) : undefined)

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <header className="flex items-center gap-4">
      <span className="grid size-12 shrink-0 place-items-center rounded-panel bg-surface-muted  "><Icon aria-hidden="true" className="size-6" /></span>
      <div className="grid min-w-0 gap-1">
        <DeviceTitle row={row} editable={canManage && Boolean(row.member)} />
        <div className="flex flex-wrap items-center gap-2">
          {!diagnostics && row.online !== undefined ? <Online online={row.online} /> : null}
          {row.browser ? <Tag>这个浏览器</Tag> : row.entry ? <Tag>{entryLabel}</Tag> : null}
          {row.browser && entryName ? <Tag title={`连在「${entryName}」上`}><terms.thisComputer.icon aria-hidden="true" className="size-3.5" />{entryName}</Tag> : null}
          {row.admin ? <Tag title={terms.admin.label}><terms.admin.icon aria-hidden="true" className="size-3.5" />{terms.admin.tag}</Tag> : null}
        </div>
        {!diagnostics && row.online === false ? <p className="text-sm text-secondary">{entryName && !row.entry ? `确认它开着，并且能连到「${entryName}」` : deviceCopy.offline}</p> : null}
      </div>
    </header>

    {copied ? <p className="flex items-start gap-2 rounded-panel border border-warning-border bg-warning-subtle p-3 text-warning">
      <Copy aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      <span>{row.entry ? "它的数据被复制过，已暂停。在另一台设备上移除它，再重新加入。" : canManage ? "它的数据被复制过。移除它，再在那台电脑上重新加入。" : "它的数据被复制过。用管理设备移除它，再重新加入。"}</span>
    </p> : null}

    {diagnostics ?
      <NodeConnection key={`${ready?.generation ?? "offline"}:${row.id}`} name={row.name} diagnostics={diagnostics} checks={checks} canRefresh={canRefresh} />
      : null}

    {row.kind === "node" && !copied ? row.entry && (canManage || localSession) ? <Section title={terms.ai.label} bare><AiSettings /></Section> : <Section title={terms.ai.label}>
      <div className="grid gap-1">
        <p>{deviceCopy.botStatus[deviceCopy.botState(row.bot, row.online)]}</p>
        {canManage && target ? <Button type="button" variant="secondary" className="mt-2 w-fit" disabled={action.busy !== null} onClick={() => void action.run("switch", () => device.connect(target))}><ArrowRightLeft aria-hidden="true" />切换到它</Button>
          : <p className="text-sm text-secondary">{canManage ? "连到它后才能修改" : editOnManager(rows.filter((item) => item.admin).map((item) => item.name))}</p>}
        {action.errorKey === "switch" ? <InlineError error={action.error} /> : null}
      </div>
    </Section> : null}

    {row.entry ? <Section title="在哪能打开">
      {isLoopback(address) || !address.startsWith("https:")
        ? <p className="flex items-center gap-2"><terms.thisComputer.icon aria-hidden="true" className="size-4 shrink-0 text-secondary" />只能在{isLoopback(address) ? "这" : "那"}台电脑上打开</p>
        : <p className="flex items-center gap-2"><terms.address.icon aria-hidden="true" className="size-4 shrink-0 text-secondary" />在哪都能打开</p>}
      {canManage || localSession ? <div className="grid gap-2 border-t pt-3">
        <label className="flex min-h-11 items-center justify-between gap-3">
          <span className="grid gap-0.5"><span>{isLoopback(address) ? "在这台电脑上免加入打开" : "在那台电脑上免加入打开"}</span><span className="text-sm text-secondary">{localCopy.note}</span></span>
          <Switch checked={device.localAutoAccess === true} aria-describedby="local-access-risk" disabled={action.busy !== null || device.localAutoAccess === null || localSession && !device.localAutoAccess} onCheckedChange={next => { const checked = next === true;  const enabled = checked; void action.run("local", () => device.setLocalAutoAccess(enabled), "已保存")  }} />
        </label>
        <p id="local-access-risk" className={cn("flex items-center gap-1.5 text-sm", device.localAutoAccess ? "text-warning" : "text-secondary")}>
          <ShieldAlert aria-hidden="true" className="size-4 shrink-0" />{isLoopback(address) ? "这" : "那"}台电脑上的人和程序不用批准就能看对话、改设置、替你回 Bot
        </p>
      </div> : null}
      {action.errorKey === "local" ? <InlineError error={action.error} /> : null}
    </Section> : null}

    {row.entry && row.kind === "node" && (canManage || localSession) ? <ShareLink /> : null}
    {row.entry && row.kind === "node" ? canManage || localSession ? <RelaySettings /> : <SettingsGroup>
      <SettingsRow icon={RadioTower} label={relayLabel}>{editOnManager(rows.filter((item) => item.admin).map((item) => item.name))}</SettingsRow>
    </SettingsGroup> : null}

    {canManage && row.member?.kind === "client" && !row.browser ? <SettingsGroup>
      <SettingsRow icon={terms.admin.icon} label={unmanaging ? deviceCopy.unmanageAsk(row.name) : terms.admin.label} tone={unmanaging ? "danger" : undefined} stacked={unmanaging}
        description={unmanaging ? deviceCopy.unmanageNote(rows.filter((item) => item.admin && item.id !== row.id).length) : deviceCopy.manageHint}>
        {unmanaging ? <>
          <Button type="button" size="sm" variant="ghost" autoFocus onClick={() => setUnmanaging(false)}>取消</Button>
          <Button type="button" size="sm" variant="danger" disabled={action.busy !== null} aria-busy={action.busy === "manage" || undefined} onClick={() => void action.run("manage", async () => { await device.setManage(row.id, false); setUnmanaging(false) }, "已取消管理")}>
            <BusyIcon busy={action.busy === "manage"} />{deviceCopy.unmanage}
          </Button>
        </> : <Switch aria-label={terms.admin.label} checked={row.admin} disabled={action.busy !== null || row.admin && !rows.some((item) => item.admin && item.id !== row.id)} onCheckedChange={next => { const checked = next === true;  if (!checked) setUnmanaging(true); else void action.run("manage", () => device.setManage(row.id, true), "已设为管理设备")  }} />}
      </SettingsRow>
      {action.errorKey === "manage" ? <div className="px-4 py-3"><InlineError error={action.error} /></div> : null}
    </SettingsGroup> : null}

    <SettingsGroup>
      <SettingsDisclosure icon={Info} label="详细信息">
        <div className="grid gap-3 text-sm text-secondary">
          {address ? <DetailRow label={terms.address.label} value={address} mono /> : null}
          <DetailRow label="设备编号" value={row.id} mono />
        </div>
      </SettingsDisclosure>
      {canManage && row.member && !row.entry && !row.browser ? <SettingsRow icon={RemoveIcon} tone="danger" label={terms.remove.label} description={removing ? deviceCopy.removeNotice(Boolean(row.bot)) : undefined} stacked onClick={removing ? undefined : () => setRemoving(true)}>
        {removing ? <>
          <Button type="button" size="sm" variant="ghost" onClick={() => setRemoving(false)}>取消</Button>
          <Button type="button" size="sm" variant="danger" disabled={action.busy !== null} aria-busy={action.busy === "remove" || undefined} onClick={() => void action.run("remove", async () => { await device.revoke(row.id); toast(`已移除「${row.name}」`, { tone: "success" }); void navigate({ to: "/devices" }) })}>
            <BusyIcon busy={action.busy === "remove"} />移除
          </Button>
        </> : null}
      </SettingsRow> : null}
      {action.errorKey === "remove" ? <div className="px-4 py-3"><InlineError error={action.error} /></div> : null}
    </SettingsGroup>
  </div>
}
