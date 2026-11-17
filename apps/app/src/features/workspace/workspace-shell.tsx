"use client"

import { BotFace,botMood } from "@/components/app/bot-face"
import { claimFocus,CountBadge,Dot,EmptyState,PageActionsSlot,PrimaryMark,Skeleton,toast,Wordmark } from "@/components/arc/product-kit"
import layout from "@/components/arc/workspace-layout.module.css"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { AiOffHint } from "@/features/devices/ai-off-hint"
import { useOpenJoins } from "@/features/devices/join"
import { SecondDeviceHint } from "@/features/devices/second-device"
import { RecoveryAlert } from "@/features/identity/recovery-controls"
import { terms } from "@/lib/glossary"
import { cn } from "@/lib/utils"
import { Link,useNavigate,useRouterState } from "@tanstack/react-router"
import type { Mood } from "@vibeface/client/avatar/face.ts"
import { moodLabel } from "@vibeface/client/avatar/mood.ts"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { entryAddress,entryKey } from "@vibeface/client/lib/node.ts"
import { unreachableText } from "@vibeface/client/lib/transport.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { backTo,tabs } from "@vibeface/client/workspace/home.ts"
import { waitingItems } from "@vibeface/client/workspace/inbox.ts"
import { addressName,entryLabel,hostOf,isLoopback } from "@vibeface/client/workspace/labels.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Popover,PopoverContent,PopoverTrigger } from "@vibeface/uiarc/popover/popover.tsx"
import { Progress } from "@vibeface/uiarc/progress/progress.tsx"
import { ArrowRightLeft,ChevronLeft,CircleAlert,CircleHelp,Hand,LoaderCircle,MessageSquare,MessagesSquare,PanelLeft,Plus,RefreshCw,RotateCw,ShieldAlert,Wifi,WifiOff,type LucideIcon } from "lucide-react"
import { createContext,useContext,useEffect,useRef,useState,useSyncExternalStore,type ReactNode } from "react"
import { BotForm } from "./bot-form"
import { BotPanelButton } from "./bot-panel"
import { useInbox } from "./inbox"
import { LocalSession } from "./local-session"

const ShellNavigation = createContext({ open: true, setOpen: (_open: boolean) => {} })
const subscribeWidth = (listener: () => void) => {
  const media = window.matchMedia("(max-width: 48rem)")
  media.addEventListener("change", listener)
  return () => media.removeEventListener("change", listener)
}
export function WorkspaceShell({ children, landing = false }: { children: ReactNode; landing?: boolean }) {
  const [open,setOpen] = useState(true)
  return <ShellNavigation.Provider value={{ open,setOpen }}><div className={layout.shell}><WorkspaceLayout landing={landing}>{children}</WorkspaceLayout></div></ShellNavigation.Provider>
}

const CreateBot = createContext<() => void>(() => {})
/** Opens the new-bot form from anywhere inside the workspace. */
export const useCreateBot = () => useContext(CreateBot)

const moodIcons: Partial<Record<Mood, LucideIcon>> = { thinking: LoaderCircle, replying: LoaderCircle, waiting: Hand }

/** What a bot is doing beside its name, in words with an icon so the face's color is never the only sign. Idle and offline say nothing here; the face shows offline. */
export function MoodTag({ mood }: { mood: Mood }) {
  const Icon = moodIcons[mood]
  if (!Icon) return moodLabel[mood] ? <span className="sr-only">，{moodLabel[mood]}</span> : null
  return <span className={cn("flex shrink-0 items-center gap-1 text-sm", mood === "waiting" ? "text-warning" : "text-secondary")}>
    <span className="sr-only">，</span><Icon aria-hidden="true" className={cn("size-3.5", Icon === LoaderCircle && "animate-spin motion-reduce:animate-none")} />{moodLabel[mood]}
  </span>
}

const JOINS_SEEN = "vibeface.joins-seen"
const titles: Record<string, string> = { "/": "消息", "/tasks": terms.task.label, "/channels": "频道", "/devices": terms.myDevices.label, "/settings": terms.settings.label, "/settings/logs": "日志", "/settings/lab": "实验室", "/settings/lab/transfers": "文件传输实验" }

function WorkspaceLayout({ children, landing }: { children: ReactNode; landing: boolean }) {
  const workspace = useWorkspace()
  // A question answered here stays on screen, collapsed, until the next read; it no longer counts.
  const waiting = waitingItems(useInbox()).length + (workspace.workspaceState?.tasks ?? []).filter((task) => ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'].includes(task.state)).length
  // A recovery about to take over outranks every other waiting item, so 任务 shows a red shield instead of a count.
  const device = useDevice()
  const recovering = Boolean(device.pendingRecovery)
  // A browser joins on this computer's lasting page, never the temporary origin this session opened on, or it would hold a second identity.
  const lasting = device.ready?.info.connection_entry
  const home = lasting && isLoopback(entryAddress(lasting)) ? `http://localhost:${new URL(entryAddress(lasting)).port}` : undefined
  const { open: sidebarOpen, setOpen: setSidebarOpen } = useContext(ShellNavigation)
  const isMobile = useSyncExternalStore(subscribeWidth, () => typeof window !== "undefined" && window.matchMedia("(max-width: 48rem)").matches, () => false)
  const [openMobile,setOpenMobile] = useState(false)
  const menuTrigger = useRef<HTMLButtonElement>(null)
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const chatRoute = pathname.startsWith("/b/")
  const scopedChat = pathname.startsWith('/channels/') || /^\/tasks\/[^/]+/.test(pathname)
  const back = backTo(pathname, useRouterState({ select: (state) => (state.location.search as { bot?: string }).bot }))
  const [creatingBot, setCreatingBot] = useState(false)
  const creatorTrigger = useRef<HTMLElement | null>(null)
  const routeBot = /^\/b\/([^/]+)/.exec(pathname)?.[1]
  // A chat link to a bot this network does not have; the conversation open before must not stand in for it.
  const missingBot = routeBot !== undefined && workspace.bots.length > 0 && !workspace.bots.some((bot) => bot.bot_id === decodeURIComponent(routeBot))
  const botTitle = chatRoute && !missingBot && workspace.selectedBot
  // On a phone the back button stands in for the sidebar, so a device's page is titled by the device.
  const deviceId = /^\/devices\/([^/]+)$/.exec(pathname)?.[1]
  const deviceName = isMobile && deviceId ? device.directory?.members.find((member) => member.device_id === decodeURIComponent(deviceId))?.name : undefined
  const scopedTitle = pathname.startsWith('/channels/') ? '频道' : pathname.startsWith('/tasks/') ? '任务' : ''
  const title = chatRoute ? "对话" : scopedTitle || deviceName || (titles[pathname] ?? (pathname.startsWith("/devices") ? terms.myDevices.label : ""))
  const navigate = useNavigate()
  const joins = useOpenJoins()
  const [actionsSlot, setActionsSlot] = useState<HTMLElement | null>(null)
  // On a phone the tabs carry 任务 and 设置, so a tab's header keeps only its own actions; a chat's header opens the bot's settings.
  const phoneTab = isMobile && tabs.includes(pathname)
  const tasksInHeader = !phoneTab && !botTitle && pathname !== "/tasks"
  const devicesAttention = joins.length > 0 || workspace.nodes.some((node) => !node.revoked && !node.online)
  const latest = useRef(workspace)
  latest.current = workspace
  useEffect(() => { void latest.current.readWorkspaceState().catch(() => {}) }, [workspace.generation])

  useEffect(() => { setOpenMobile(false) }, [pathname, setOpenMobile])
  // Each request toasts once, also across reloads of this tab; on a page that lists it, seeing it there counts.
  const joinIds = joins.map((join) => join.join_id).join()
  useEffect(() => {
    if (!joinIds) return
    let seen: string[] = []
    try { seen = JSON.parse(sessionStorage.getItem(JOINS_SEEN) ?? "[]") } catch {}
    const fresh = joins.filter((join) => !seen.includes(join.join_id))
    if (!fresh.length) return
    try { sessionStorage.setItem(JOINS_SEEN, JSON.stringify([...seen, ...fresh.map((join) => join.join_id)].slice(-50))) } catch {}
    if (pathname === "/devices" || pathname === "/tasks") return
    for (const join of fresh) toast(`${join.name || "新设备"} 想加入`, { icon: terms.myDevices.icon, action: { label: "查看", onClick: () => { void navigate({ to: "/devices" }) } } })
  }, [joinIds, pathname])
  useEffect(() => { if (workspace.noticeText) toast(workspace.noticeText) }, [workspace.noticeText])
  useEffect(() => { if (workspace.error) toast(workspace.error.text, { tone: "danger", action: workspace.error.retry ? { label: "重试", onClick: workspace.error.retry } : undefined }) }, [workspace.error])

  function openCreator() {
    creatorTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setCreatingBot(true); setOpenMobile(false)
  }

  const navigation = <nav aria-label="导航" className={layout.navigation}>
    <div className={layout.brand}><Link to="/"><Wordmark /></Link></div>
    <div className={layout.navBody}>
      <ul className={layout.list}><NavItem to="/" icon={MessageSquare} label="消息" active={pathname === "/"} /><NavItem to="/tasks" icon={terms.task.icon} label={terms.task.label} active={pathname === "/tasks"} badge={waiting || undefined} alert={recovering} /></ul>
      <section className={layout.section}><div className={layout.sectionHeader}><h2>频道</h2><Button type="button" variant="ghost" aria-label="管理频道" title="管理频道" onClick={() => void navigate({ to: "/channels" })}><Plus aria-hidden="true" /></Button></div>
        <ul className={layout.list}>{(workspace.workspaceState?.channels ?? []).map(channel => <li key={channel.channel_id}><Link to="/channels/$channelId" params={{ channelId: channel.channel_id }} className={layout.item} aria-current={pathname === `/channels/${channel.channel_id}` ? "page" : undefined}><MessagesSquare aria-hidden="true" /><span title={channel.title}>{channel.title}</span></Link></li>)}{!workspace.workspaceState?.channels.length ? <NavItem to="/channels" icon={MessagesSquare} label="查看频道" active={pathname === "/channels"} /> : null}</ul>
      </section>
      <section className={layout.section}><div className={layout.sectionHeader}><h2>Bot</h2><Button type="button" variant="ghost" aria-label="新 Bot" title="新 Bot" disabled={!workspace.writable && !workspace.pendingBot} onClick={openCreator}><Plus aria-hidden="true" /></Button></div>
        <ul className={layout.list}>
          {!workspace.bots.length && !workspace.synced ? [0,1,2].map(item => <li key={item}><Skeleton className="h-11" /></li>) : null}
          {workspace.bots.map(bot => {
            const active = chatRoute && !missingBot && bot.bot_id === workspace.selectedBot?.bot_id
            const marked = !active && workspace.notifications.some(item => item.conversation_id === bot.conversation_id && item.target?.kind === 'bot_main' && item.target.bot_id === bot.bot_id && !workspace.ignoredNotifications.has(item.notification_id))
            const mood = botMood(bot,workspace)
            return <li key={bot.bot_id}><Link to="/b/$botId" params={{ botId: bot.bot_id }} aria-current={active ? "page" : undefined} title={bot.name} className={layout.item}><BotFace bot={bot} mood={mood} size={28} /><span>{bot.name}</span>{workspace.primary === bot.conversation_id ? <PrimaryMark /> : null}{mood !== "waiting" && marked ? <Dot label="有新进展" /> : <MoodTag mood={mood} />}</Link></li>
          })}
        </ul>
      </section>
    </div>
    <div className={layout.footer}><AiOffHint /><SecondDeviceHint /><ConnectionCapsule /><ul className={layout.list}><NavItem to="/settings" icon={terms.settings.icon} label={terms.settings.label} active={pathname.startsWith("/settings") || pathname.startsWith("/devices")} dot={devicesAttention} /><li><a href="https://www.vibeface.dev/docs/user-guide" target="_blank" rel="noopener noreferrer" className={layout.item}><CircleHelp aria-hidden="true" /><span>使用指南</span><span className="sr-only">，在新标签页打开</span></a></li></ul></div>
  </nav>

  return <CreateBot.Provider value={openCreator}><PageActionsSlot.Provider value={actionsSlot}>
    <a className="skip-link" href="#main">跳到主要内容</a>
    {isMobile ? <Drawer open={openMobile} onOpenChange={setOpenMobile}><DrawerContent side="left" title="导航和会话" description="切换工作区视图或选择会话。" className={layout.mobileDrawer} onCloseAutoFocus={(event) => { event.preventDefault(); menuTrigger.current?.focus({ preventScroll: true }) }}>{navigation}</DrawerContent></Drawer> : sidebarOpen ? navigation : null}
    <main id="main" tabIndex={-1} className={layout.main}>
      <header className={layout.header}>
        <Button ref={menuTrigger} type="button" variant="ghost" aria-label="菜单" title="菜单" onClick={() => isMobile ? setOpenMobile(true) : setSidebarOpen(!sidebarOpen)}><PanelLeft aria-hidden="true" /></Button>
        {back ? <Button type="button" variant="ghost" aria-label="返回" title="返回" onClick={() => void navigate({ to: back })}><ChevronLeft aria-hidden="true" /></Button> : null}
        <h1 ref={landing ? claimFocus : undefined} tabIndex={-1} title={botTitle ? undefined : title}>{botTitle ? <BotPanelButton /> : title}</h1>
        <div ref={setActionsSlot} className={layout.headerActions} />
        {isMobile && pathname === "/" ? <Button type="button" variant="ghost" aria-label="新 Bot" title="新 Bot" disabled={!workspace.writable && !workspace.pendingBot} onClick={openCreator}><Plus aria-hidden="true" /></Button> : tasksInHeader ? <Button type="button" variant="ghost" aria-label={recovering ? `${terms.task.label}，有人在找回` : waiting ? `${terms.task.label} ${waiting}` : terms.task.label} title={terms.task.label} onClick={() => void navigate({ to: "/tasks" })}><terms.task.icon aria-hidden="true" />{recovering ? <ShieldAlert aria-hidden="true" className="size-4 text-danger" /> : waiting ? <CountBadge count={waiting} /> : null}</Button> : null}
      </header>
      {workspace.joined ? null : <LocalSession onJoin={() => { if (home && home !== window.location.origin) window.location.assign(`${home}/join`); else void navigate({ to: "/join" }) }} />}
      {pathname === "/tasks" ? null : <RecoveryAlert />}
      {workspace.data && workspace.incomplete ? <Progress value={30} label="同步中" /> : null}
      <div className={cn(layout.chat,!chatRoute && layout.chatHidden)} inert={!chatRoute} aria-hidden={!chatRoute || undefined}><div className="conversation-view"><ConversationScreen missing={missingBot} active={/^\/b\/[^/]+\/?$/.test(pathname) && !missingBot} /></div>{chatRoute ? children : null}</div>
      {chatRoute ? null : <div className={scopedChat ? 'flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden' : layout.page}>{children}</div>}
      {phoneTab ? <TabBar pathname={pathname} waiting={waiting} recovering={recovering} attention={devicesAttention} /> : null}
    </main>
    <BotForm open={creatingBot} onOpenChange={setCreatingBot} restoreFocus={() => { const target = creatorTrigger.current; if (target?.isConnected && target.getClientRects().length) target.focus(); else document.getElementById("main")?.focus() }} />
  </PageActionsSlot.Provider></CreateBot.Provider>
}

const tabItems = [
  { to: "/", icon: MessageSquare, label: "消息" },
  { to: "/tasks", icon: terms.task.icon, label: terms.task.label },
  { to: "/settings", icon: terms.settings.icon, label: terms.settings.label },
] as const

/** The phone's bottom tabs; the count on 任务 and the mark on 设置 stand in for what a wide screen shows in the sidebar. */
function TabBar({ pathname,waiting,recovering,attention }: { pathname: string; waiting: number; recovering: boolean; attention: boolean }) {
  return <nav aria-label="标签栏" data-toast-anchor="" className={layout.tabs}>{tabItems.map(({ to,icon: Icon,label }) => <Link key={to} to={to} replace aria-current={pathname === to ? "page" : undefined} className={layout.tab}><Icon aria-hidden="true" className="size-5 shrink-0" /><span>{label}</span>{to === "/tasks" ? recovering ? <ShieldAlert aria-label="有人在找回" className="size-4 text-danger" /> : waiting ? <CountBadge count={waiting} /> : null : to === "/settings" && attention ? <CircleAlert aria-label="需要留意" className="size-4 text-warning" /> : null}</Link>)}</nav>
}
function NavItem({ to,icon: Icon,label,active,badge,dot,alert }: { to: string; icon: LucideIcon; label: string; active: boolean; badge?: number; dot?: boolean; alert?: boolean }) {
  return <li><Link to={to} aria-current={active ? "page" : undefined} className={layout.item}><Icon aria-hidden="true" /><span>{label}</span>{alert ? <span className="inline-flex items-center gap-1 text-danger"><ShieldAlert aria-hidden="true" className="size-4" />找回</span> : badge ? <CountBadge count={badge} /> : null}{dot ? <CircleAlert role="img" aria-label="需要留意" className="size-4 text-warning" /> : null}</Link></li>
}

/** `side`: where the details open; the phone home list puts it below, so the Bot list above stays in view. */
export function ConnectionCapsule({ side = "top" }: { side?: "top" | "bottom" }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const { link, entrances } = device
  const address = device.ready ? entryAddress(device.ready.entry) : ""
  const name = entryLabel({ joined: workspace.joined, directory: device.directory, entryId: device.ready?.entry.device_id, ownName: workspace.info?.name, address })
  // Devices of another network this browser opened before; the current network fails over by itself.
  const others = device.entries.filter((item) => item.kind !== "local" && !device.directory?.members.some((member) => member.device_id === item.device_id))
  const state: { label: string; icon: LucideIcon; tone: string; encrypted?: boolean } = link.kind === "connecting" || name === null
    ? { label: entrances.length > 1 && name !== null ? "切换中" : "连接中", icon: ArrowRightLeft, tone: "text-info" }
    : !workspace.connectionOnline || link.kind === "offline"
      ? { label: "离线", icon: WifiOff, tone: "text-secondary" }
      : workspace.writeState.state === "forked"
      ? { label: terms.paused.label, icon: terms.paused.icon, tone: "text-warning" }
      : workspace.readOnlyNotice && workspace.writeState.state !== "aligning"
      ? { label: terms.readOnly.label, icon: terms.readOnly.icon, tone: "text-warning" }
      : link.kind === "online" && link.catchingUp || workspace.writeNotice || workspace.writeState.state === "aligning" || workspace.data && workspace.incomplete
        ? { label: terms.syncing.label, icon: terms.syncing.icon, tone: "text-info" }
        : { label: "在线", icon: Wifi, tone: "text-success", encrypted: true }
  return <Popover>
    <PopoverTrigger asChild>
      <Button variant="ghost" type="button" className="flex w-full items-center gap-2 text-left" aria-label={name === null ? state.label : `${name} · ${state.encrypted ? `${terms.encrypted.label} · ` : ""}${state.label}`}>
        <state.icon aria-hidden="true" className={cn("size-4 shrink-0", state.tone)} />
        <span className={cn("min-w-0 flex-1 truncate", name === null && state.tone)}>{name ?? state.label}</span>
        {name === null ? null : <span className={cn("shrink-0", state.tone)}>{state.label}</span>}
      </Button>
    </PopoverTrigger>
      <PopoverContent side={side} align="start" sideOffset={6} className="z-50 grid w-72 gap-2 rounded-panel border bg-surface-raised p-3 text-sm text-foreground shadow-lg">
        <p className="font-medium">{terms.address.label}</p>
        <p className="break-all text-secondary" title={address}>{hostOf(address) || address}</p>
        {link.kind === "offline" ? <div className="grid gap-2"><p className="text-secondary">{link.reason === unreachableText ? "连不上这台电脑" : "所有访问地址都连不上"}，{Math.ceil(link.retryInMs / 1000)} 秒后再试。</p>
          <Button type="button" size="sm" className="w-fit" onClick={device.retryNow}><RotateCw aria-hidden="true" />立即重连</Button></div>
          : workspace.connectionOnline ? <p className="flex items-center gap-1.5 text-success"><terms.encrypted.icon aria-hidden="true" className="size-4" />{terms.encrypted.label}，设备已验证</p> : null}
        {workspace.readOnlyNotice ? <p className="text-warning">{workspace.readOnlyNotice}</p> : null}
        {others.length ? <>
          <p className="mt-1 font-medium">换一台设备</p>
          <ul className="grid gap-1" aria-label="换一台设备">{others.map((item) => <li key={entryKey(item)}>
            <Button variant="ghost" type="button" className="flex w-full min-w-0 items-center gap-2 text-left" onClick={() => { void device.connect(item).catch(() => {}) }}>
              <span className="min-w-0 flex-1 truncate">{device.entryName(item) || addressName(entryAddress(item))}</span>
              <ArrowRightLeft aria-hidden="true" className="size-4 shrink-0 text-secondary" />
            </Button>
          </li>)}</ul>
        </> : null}
        {entrances.length > 1 && device.ready?.principal.can_manage ? <>
          <p className="mt-1 font-medium">可切换的访问地址</p>
          <ul className="grid gap-1" aria-label="可切换的访问地址">{entrances.map((item) => <li key={item.device_id} className="flex min-w-0 items-center gap-2">
            <span className="min-w-0 flex-1 truncate" title={item.gateway_url}>{device.directory?.members.find((member) => member.device_id === item.device_id)?.name || addressName(item.gateway_url)}</span>
            <span className="shrink-0 text-secondary">{item.device_id === device.ready?.entry.device_id ? "当前" : item.public ? "公网" : "仅本机或局域网"}</span>
          </li>)}</ul>
        </> : null}
      </PopoverContent>
  </Popover>
}

/** Same layout as the workspace, shown while the browser loads its keys and connects. */
/** `home`: the conversation list loads on a phone, the last conversation on a wide screen. */
export function ShellSkeleton({ retry = null, onRetry, onLeave, home = false, queued = null }: { retry?: number | null; onRetry?: () => void; onLeave?: () => void; home?: boolean; queued?: number | null } = {}) {
  // A tap on reconnect holds the offline view with a turning icon until the attempt ends: connected, or a new countdown.
  const [retrying, setRetrying] = useState(false)
  useEffect(() => { if (retry !== null) setRetrying(false) }, [retry])
  const shown = useRef(retry)
  if (retry !== null) shown.current = retry
  const offline = retry !== null || retrying
  // Offline, the page says it once in the middle; the corner status is only for a connection still in progress.
  const status = offline ? null : <><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{queued ? `排队中，第 ${queued} 位` : '连接中'}</>
  return <div className="flex h-dvh" aria-busy={!offline}>
    <div className="hidden w-64 shrink-0 flex-col gap-3 border-r bg-surface-muted p-4 md:flex">
      <Wordmark />
      <Skeleton className="mt-2 h-9" /><Skeleton className="h-4 w-12" /><Skeleton className="h-9" /><Skeleton className="h-9" />
      {status ? <div className="mt-auto flex items-center gap-2 px-2 text-sm text-info" role="status">{status}</div> : null}
    </div>
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="flex h-14 items-center gap-3 border-b px-4">{home ? <span className="text-xl font-medium md:hidden">消息</span> : null}<Skeleton className={cn("h-5 w-32", home && "max-md:hidden")} />{status ? <span className="ml-auto flex items-center gap-2 text-sm text-info md:hidden">{status}</span> : null}</div>
      {offline ? <div className="my-auto"><EmptyState icon={WifiOff} title="连不上设备" action={<div className="grid justify-items-center gap-3">
        <p className="flex items-center gap-1 text-sm text-secondary"><span aria-hidden={retrying || undefined}>{retrying ? "正在重连" : `${shown.current} 秒后自动重试`}</span>
          {/* The countdown ticks every second, so only the state change is announced. */}
          <span role="status" className="sr-only">{retrying ? "正在重连" : "连不上设备"}</span>
          {onRetry ? <Button type="button" variant="ghost" size="md" className="hit-area relative" aria-label="立即重连" title="立即重连" disabled={retrying} aria-busy={retrying || undefined}
            onClick={() => { setRetrying(true); onRetry() }}><RefreshCw aria-hidden="true" className={cn(retrying && "animate-spin")} /></Button> : null}
        </p>
        {onLeave ? <Button type="button" variant="ghost" onClick={onLeave}>换个方式连接</Button> : null}
      </div>} /></div> : <>
        {home ? <div className="grid gap-2 p-4 md:hidden" aria-hidden="true">{[0, 1, 2, 3].map((item) => <Skeleton key={item} className="h-16 rounded-panel" />)}</div> : null}
        <div className={cn("contents", home && "max-md:hidden")}><ChatSkeleton /></div>
        {onLeave ? <div className="mt-auto flex justify-center px-4 py-6"><Button type="button" variant="ghost" onClick={onLeave}>换个方式连接</Button></div> : null}
      </>}
    </div>
  </div>
}

export function ChatSkeleton() {
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6 px-4 py-6 sm:px-6" aria-hidden="true">
    <Skeleton className="ml-auto h-10 w-2/5 rounded-panel" />
    <div className="grid gap-2"><Skeleton className="h-4 w-4/5" /><Skeleton className="h-4 w-3/5" /><Skeleton className="h-4 w-2/3" /></div>
    <Skeleton className="ml-auto h-10 w-1/3 rounded-panel" />
    <div className="grid gap-2"><Skeleton className="h-4 w-3/4" /><Skeleton className="h-4 w-1/2" /></div>
  </div>
}
