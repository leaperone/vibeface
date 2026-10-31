"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import { Link, useNavigate, useRouterState } from "@tanstack/react-router"
import { ArrowRightLeft, ChevronLeft, CircleAlert, Hand, LoaderCircle, Plus, RefreshCw, RotateCw, ShieldAlert, Wifi, WifiOff, type LucideIcon } from "lucide-react"
import { terms } from "@/lib/glossary"
import { Popover } from "radix-ui"
import { cn } from "@/lib/utils"
import { claimFocus, CountBadge, Dot, EmptyState, PageActionsSlot, PrimaryMark, Skeleton, toast, Wordmark } from "@/components/app/kit"
import { BotFace, botMood } from "@/components/app/bot-face"
import { moodLabel } from "@vibeface/client/avatar/mood.ts"
import type { Mood } from "@vibeface/client/avatar/face.ts"
import { useOpenJoins } from "@/features/devices/join"
import { AiOffHint } from "@/features/devices/ai-off-hint"
import { SecondDeviceHint } from "@/features/devices/second-device"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { RecoveryAlert } from "@/features/identity/recovery-controls"
import { Button } from "@/components/ui/button"
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupAction, SidebarGroupContent, SidebarGroupLabel, SidebarHeader,
  SidebarInset, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarTrigger, useSidebar,
} from "@/components/ui/sidebar"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { BotPanelButton } from "./bot-panel"
import { BotForm } from "./bot-form"
import { useInbox } from "./inbox"
import { waitingItems } from "@vibeface/client/workspace/inbox.ts"
import { backTo } from "@vibeface/client/workspace/home.ts"
import { addressName, hostOf, isLoopback } from "@vibeface/client/workspace/labels.ts"
import { entryAddress, entryKey } from "@vibeface/client/lib/node.ts"
import { unreachableText } from "@vibeface/client/lib/transport.ts"
import { LocalSession } from "./local-session"

/** `landing`: the title takes the focus when the shell replaces the page the person was on. */
export function WorkspaceShell({ children, landing = false }: { children: ReactNode; landing?: boolean }) {
  const [sidebarOpen, setSidebarOpen] = useState(() => typeof window === "undefined" || window.matchMedia("(min-width: 1024px)").matches)
  useEffect(() => {
    const wide = window.matchMedia("(min-width: 1024px)")
    const follow = () => { if (wide.matches) setSidebarOpen(true) }
    wide.addEventListener("change", follow)
    return () => wide.removeEventListener("change", follow)
  }, [])
  return <SidebarProvider className="workspace-shell" open={sidebarOpen} onOpenChange={setSidebarOpen}><WorkspaceLayout landing={landing}>{children}</WorkspaceLayout></SidebarProvider>
}

const CreateBot = createContext<() => void>(() => {})
/** Opens the new-bot form from anywhere inside the workspace. */
export const useCreateBot = () => useContext(CreateBot)

const moodIcons: Partial<Record<Mood, LucideIcon>> = { thinking: LoaderCircle, replying: LoaderCircle, waiting: Hand }

/** What a bot is doing beside its name, in words with an icon so the face's color is never the only sign. Idle and offline say nothing here; the face shows offline. */
export function MoodTag({ mood }: { mood: Mood }) {
  const Icon = moodIcons[mood]
  if (!Icon) return moodLabel[mood] ? <span className="sr-only">，{moodLabel[mood]}</span> : null
  return <span className={cn("flex shrink-0 items-center gap-1 text-meta", mood === "waiting" ? "text-warning" : "text-muted-foreground")}>
    <span className="sr-only">，</span><Icon aria-hidden="true" className={cn("size-3.5", Icon === LoaderCircle && "animate-spin motion-reduce:animate-none")} />{moodLabel[mood]}
  </span>
}

const JOINS_SEEN = "vibeface.joins-seen"
const titles: Record<string, string> = { "/": "消息", "/inbox": terms.inbox.label, "/tasks": terms.task.label, "/devices": terms.myDevices.label, "/settings": terms.settings.label, "/settings/logs": "日志", "/settings/lab": "实验室", "/settings/lab/transfers": "文件传输实验" }

function WorkspaceLayout({ children, landing }: { children: ReactNode; landing: boolean }) {
  const workspace = useWorkspace()
  // A question answered here stays on screen, collapsed, until the next read; it no longer counts.
  const waiting = waitingItems(useInbox()).length
  // A recovery about to take over outranks every other waiting item, so 待处理 shows a red shield instead of a count.
  const device = useDevice()
  const recovering = Boolean(device.pendingRecovery)
  // A browser joins on this computer's lasting page, never the temporary origin this session opened on, or it would hold a second identity.
  const lasting = device.ready?.info.connection_entry
  const home = lasting && isLoopback(entryAddress(lasting)) ? `http://localhost:${new URL(entryAddress(lasting)).port}` : undefined
  const { setOpenMobile, isMobile, open: sidebarOpen } = useSidebar()
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const chatRoute = pathname.startsWith("/b/")
  const taskOpen = /^\/b\/[^/]+\/work\//.test(pathname)
  const back = backTo(pathname, useRouterState({ select: (state) => (state.location.search as { bot?: string }).bot }))
  const nested = Boolean(back) && back !== "/" && !chatRoute
  const [creatingBot, setCreatingBot] = useState(false)
  const creatorTrigger = useRef<HTMLElement | null>(null)
  const routeBot = /^\/b\/([^/]+)/.exec(pathname)?.[1]
  // A chat link to a bot this network does not have; the conversation open before must not stand in for it.
  const missingBot = routeBot !== undefined && workspace.bots.length > 0 && !workspace.bots.some((bot) => bot.bot_id === decodeURIComponent(routeBot))
  const botTitle = chatRoute && !missingBot && workspace.selectedBot
  const title = chatRoute ? "对话" : titles[pathname] ?? (pathname.startsWith("/devices") ? terms.myDevices.label : "")
  const navigate = useNavigate()
  const joins = useOpenJoins()
  const [actionsSlot, setActionsSlot] = useState<HTMLElement | null>(null)
  // The phone's home lists 待处理 itself; elsewhere the header carries it whenever the sidebar is out of sight.
  const phoneHome = isMobile && pathname === "/"
  const inboxInHeader = (isMobile || !sidebarOpen) && !phoneHome && pathname !== "/inbox"
  const devicesAttention = joins.length > 0 || workspace.nodes.some((node) => !node.revoked && !node.online)

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
    if (pathname === "/devices" || pathname === "/inbox") return
    for (const join of fresh) toast(`${join.name || "新设备"} 想加入`, { icon: terms.myDevices.icon, action: { label: "查看", onClick: () => { void navigate({ to: "/devices" }) } } })
  }, [joinIds, pathname])
  useEffect(() => { if (workspace.noticeText) toast(workspace.noticeText) }, [workspace.noticeText])
  useEffect(() => { if (workspace.error) toast(workspace.error.text, { tone: "danger", action: workspace.error.retry ? { label: "重试", onClick: workspace.error.retry } : undefined }) }, [workspace.error])

  function openCreator() {
    creatorTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setCreatingBot(true); setOpenMobile(false)
  }

  return (
    <CreateBot.Provider value={openCreator}><PageActionsSlot.Provider value={actionsSlot}>
      <a className="skip-link" href="#main">跳到主要内容</a>
      <Sidebar aria-label="导航">
        <SidebarHeader className="shrink-0 px-4 pt-4 pb-2">
          <div className="flex h-8 items-center">
            <Link to="/" className="rounded-md outline-none focus-visible:ring-2 focus-visible:ring-ring"><Wordmark /></Link>
          </div>
        </SidebarHeader>
        <SidebarContent className="gap-1 pb-3">
          <SidebarGroup className="pb-0">
            <SidebarMenu>
              <NavItem to="/tasks" icon={terms.task.icon} label={terms.task.label} active={pathname === "/tasks"} />
              <NavItem to="/inbox" icon={terms.inbox.icon} label={terms.inbox.label} active={pathname === "/inbox"} badge={waiting || undefined} alert={recovering} />
            </SidebarMenu>
          </SidebarGroup>
          <SidebarGroup>
            <SidebarGroupLabel>Bot</SidebarGroupLabel>
            <SidebarGroupAction title="新 Bot" aria-label="新 Bot" disabled={!workspace.writable && !workspace.pendingBot} onClick={openCreator}><Plus /></SidebarGroupAction>
            <SidebarGroupContent>
              <SidebarMenu>
                {!workspace.bots.length && !workspace.synced ? [0, 1, 2].map((item) => <SidebarMenuItem key={item} aria-hidden="true"><Skeleton className="h-10 max-md:h-11" /></SidebarMenuItem>) : null}
                {workspace.bots.map((bot) => {
                  const active = chatRoute && !missingBot && bot.bot_id === workspace.selectedBot?.bot_id
                  const marked = !active && (workspace.notifications.some((item) => item.conversation_id === bot.conversation_id))
                  const mood = botMood(bot, workspace)
                  return <SidebarMenuItem key={bot.bot_id}>
                    <SidebarMenuButton asChild isActive={active} className="h-10 max-md:h-11">
                      <Link to="/b/$botId" params={{ botId: bot.bot_id }} aria-current={active ? "page" : undefined} title={bot.name}>
                        <BotFace bot={bot} mood={mood} size={28} className="size-7!" />
                        <span className="min-w-0 flex-1 truncate">{bot.name}</span>
                        <MoodTag mood={mood} />
                        {workspace.primary === bot.conversation_id ? <PrimaryMark /> : null}
                        {marked ? <Dot label="有新进展" /> : null}
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="shrink-0 gap-1 px-2 py-3">
          <AiOffHint />
          <SecondDeviceHint />
          <ConnectionCapsule />
          <SidebarMenu>
            <NavItem to="/devices" icon={terms.myDevices.icon} label={terms.myDevices.label} active={pathname.startsWith("/devices")} dot={devicesAttention} />
            <NavItem to="/settings" icon={terms.settings.icon} label={terms.settings.label} active={pathname.startsWith("/settings")} />
          </SidebarMenu>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset id="main" tabIndex={-1} className="min-h-0 min-w-0 overflow-hidden">
        {/* The middle column has the pages' width, so the title and the page's actions line up with the content below. */}
        {/* Beside an open task panel the chat column narrows, so the title keeps over it rather than over the gap. */}
        <header className={cn("grid h-14 shrink-0 grid-cols-[minmax(max-content,1fr)_minmax(0,var(--page-width))_minmax(max-content,1fr)] items-center border-b ps-[max(1rem,env(safe-area-inset-left))] pe-[max(1rem,env(safe-area-inset-right))] sm:ps-[max(1.5rem,env(safe-area-inset-left))] sm:pe-[max(1.5rem,env(safe-area-inset-right))]", taskOpen && "lg:pe-[420px]")}>
          <div className="flex items-center pe-2 empty:hidden">
            {/* A page inside another page goes back from its header at any width; the sidebar covers the rest on wide screens. */}
            {back ? <Button asChild variant="ghost" size="icon" className={cn("-ms-3", !nested && "md:hidden")}><Link to={back} aria-label="返回" title="返回"><ChevronLeft aria-hidden="true" /></Link></Button> : null}
            {isMobile ? null : <SidebarTrigger aria-label="菜单" title="菜单" className={cn("max-md:hidden", nested ? "max-lg:hidden" : "-ms-2.5")} />}
          </div>
          <div className={cn("col-start-2 flex min-w-0 items-center gap-2", chatRoute && "lg:px-6")}>
            <div className="flex min-w-0 flex-1 items-center gap-2">
              {/* On the chat route the bot's name is the button that opens its panel; the heading around it still takes the focus. */}
              <h1 ref={landing ? claimFocus : undefined} tabIndex={-1} className={cn("min-w-0 text-title font-semibold outline-none", botTitle ? "flex" : "truncate")} title={botTitle ? undefined : title}>{botTitle ? <BotPanelButton /> : title}</h1>
            </div>
            <div ref={setActionsSlot} className="flex shrink-0 items-center gap-1 empty:hidden" />
          </div>
          <div className="col-start-3 flex items-center justify-end ps-2 empty:hidden">
          {phoneHome ? <div className="-me-3 flex">
            <Button asChild variant="ghost" size="icon" className="relative">
              <Link to="/devices" aria-label={devicesAttention ? `${terms.myDevices.label}，需要留意` : terms.myDevices.label} title={terms.myDevices.label}>
                <terms.myDevices.icon aria-hidden="true" />
                {devicesAttention ? <CircleAlert aria-hidden="true" className="absolute top-1 right-1 size-3! rounded-full bg-background text-warning" /> : null}
              </Link>
            </Button>
            <Button asChild variant="ghost" size="icon"><Link to="/settings" aria-label={terms.settings.label} title={terms.settings.label}><terms.settings.icon aria-hidden="true" /></Link></Button>
          </div> : inboxInHeader ? <Button asChild variant="ghost" size="icon" className="relative -me-3">
            <Link to="/inbox" aria-label={recovering ? `${terms.inbox.label}，有人在找回` : waiting ? `${terms.inbox.label} ${waiting}` : terms.inbox.label} title={terms.inbox.label}>
              <terms.inbox.icon aria-hidden="true" />
              {recovering ? <ShieldAlert aria-hidden="true" className="absolute top-0.5 right-0.5 size-4! rounded-full bg-background text-danger md:-top-0.5 md:-right-0.5" /> : waiting ? <CountBadge key={waiting} count={waiting} className="absolute top-0.5 right-0.5 h-4 min-w-4 px-1 ring-2 ring-background animate-in zoom-in-50 duration-200 ease-out md:-top-0.5 md:-right-0.5" /> : null}
            </Link>
          </Button> : null}
          </div>
        </header>
        {workspace.joined ? null : <LocalSession onJoin={() => { if (home && home !== window.location.origin) window.location.assign(`${home}/join`); else void navigate({ to: "/join" }) }} />}
        {pathname === "/inbox" ? null : <RecoveryAlert />}
        {workspace.data && workspace.incomplete ? <div role="progressbar" aria-label="同步中" className="h-0.5 shrink-0 overflow-hidden bg-info-soft"><div className="h-full w-1/3 animate-pulse bg-info" /></div> : null}
        {/* Kept laid out while hidden so the chat keeps its scroll position across pages. */}
        <div className={cn("flex min-h-0 min-w-0 flex-1", !chatRoute && "invisible absolute inset-x-0 top-14 bottom-0")} inert={!chatRoute} aria-hidden={!chatRoute || undefined}>
          <div className="conversation-view"><ConversationScreen missing={missingBot} /></div>
          {chatRoute ? children : null}
        </div>
        {chatRoute ? null : <div className="workspace-screen">{children}</div>}
      </SidebarInset>
      <BotForm open={creatingBot} onOpenChange={setCreatingBot} restoreFocus={() => {
        const target = creatorTrigger.current
        if (target?.isConnected && target.getClientRects().length) target.focus()
        else document.getElementById("main")?.focus()
      }} />
    </PageActionsSlot.Provider></CreateBot.Provider>
  )
}

function NavItem({ to, icon: Icon, label, active, badge, dot, alert }: { to: string; icon: LucideIcon; label: string; active: boolean; badge?: number; dot?: boolean; alert?: boolean }) {
  return <SidebarMenuItem>
    <SidebarMenuButton asChild isActive={active} className="h-10 max-md:h-11">
      <Link to={to} aria-current={active ? "page" : undefined}>
        <Icon aria-hidden="true" />
        <span className="flex-1">{label}</span>
        {alert ? <span className="flex items-center gap-1 text-meta font-medium text-danger"><ShieldAlert aria-hidden="true" className="size-4!" />找回</span> : badge ? <CountBadge count={badge} /> : null}
        {/* A shape, not only a color, so it reads without telling colors apart. */}
        {dot ? <CircleAlert role="img" aria-label="需要留意" className="size-4! text-warning" /> : null}
      </Link>
    </SidebarMenuButton>
  </SidebarMenuItem>
}

/** `side`: where the details open; the phone home list puts it below, so the Bot list above stays in view. */
export function ConnectionCapsule({ side = "top" }: { side?: "top" | "bottom" }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const { link, entrances } = device
  const address = device.ready ? entryAddress(device.ready.entry) : ""
  const entryId = device.ready?.entry.device_id
  const entryName = device.directory?.members.find((member) => member.device_id === entryId)?.name || workspace.info?.name
  const name = entryName || addressName(address)
  // Devices of another network this browser opened before; the current network fails over by itself.
  const others = device.entries.filter((item) => item.kind !== "local" && !device.directory?.members.some((member) => member.device_id === item.device_id))
  const state: { label: string; icon: LucideIcon; tone: string; encrypted?: boolean } = link.kind === "connecting"
    ? { label: entrances.length > 1 ? "切换中" : "连接中", icon: ArrowRightLeft, tone: "text-info" }
    : !workspace.connectionOnline || link.kind === "offline"
      ? { label: "离线", icon: WifiOff, tone: "text-muted-foreground" }
      : workspace.writeState.state === "forked"
      ? { label: terms.paused.label, icon: terms.paused.icon, tone: "text-warning" }
      : workspace.readOnlyNotice && workspace.writeState.state !== "aligning"
      ? { label: terms.readOnly.label, icon: terms.readOnly.icon, tone: "text-warning" }
      : link.kind === "online" && link.catchingUp || workspace.writeNotice || workspace.writeState.state === "aligning" || workspace.data && workspace.incomplete
        ? { label: terms.syncing.label, icon: terms.syncing.icon, tone: "text-info" }
        : { label: "在线", icon: Wifi, tone: "text-success", encrypted: true }
  return <Popover.Root>
    <Popover.Trigger asChild>
      <button type="button" className="flex h-10 w-full items-center gap-2 rounded-md px-2 text-left text-meta hover:bg-sidebar-accent active:bg-sidebar-accent max-md:h-11" aria-label={`${name} · ${state.encrypted ? `${terms.encrypted.label} · ` : ""}${state.label}`}>
        <state.icon aria-hidden="true" className={cn("size-4 shrink-0", state.tone)} />
        <span className="min-w-0 flex-1 truncate">{name}</span>
        <span className={cn("shrink-0", state.tone)}>{state.label}</span>
      </button>
    </Popover.Trigger>
    <Popover.Portal>
      <Popover.Content side={side} align="start" sideOffset={6} className="z-50 grid w-72 gap-2 rounded-xl border bg-popover p-3 text-meta text-popover-foreground shadow-lg">
        <p className="font-medium">{terms.address.label}</p>
        <p className="break-all text-muted-foreground" title={address}>{hostOf(address) || address}</p>
        {link.kind === "offline" ? <div className="grid gap-2"><p className="text-muted-foreground">{link.reason === unreachableText ? "连不上这台电脑" : "所有访问地址都连不上"}，{Math.ceil(link.retryInMs / 1000)} 秒后再试。</p>
          <Button type="button" size="sm" className="w-fit" onClick={device.retryNow}><RotateCw aria-hidden="true" />立即重连</Button></div>
          : workspace.connectionOnline ? <p className="flex items-center gap-1.5 text-success"><terms.encrypted.icon aria-hidden="true" className="size-4" />{terms.encrypted.label}，设备已验证</p> : null}
        {workspace.readOnlyNotice ? <p className="text-warning">{workspace.readOnlyNotice}</p> : null}
        {others.length ? <>
          <p className="mt-1 font-medium">换一台设备</p>
          <ul className="grid gap-1" aria-label="换一台设备">{others.map((item) => <li key={entryKey(item)}>
            <button type="button" className="flex min-h-9 w-full min-w-0 items-center gap-2 rounded-md px-2 text-left hover:bg-muted active:bg-muted" onClick={() => { void device.connect(item).catch(() => {}) }}>
              <span className="min-w-0 flex-1 truncate">{device.entryName(item) || addressName(entryAddress(item))}</span>
              <ArrowRightLeft aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            </button>
          </li>)}</ul>
        </> : null}
        {entrances.length > 1 && device.ready?.principal.can_manage ? <>
          <p className="mt-1 font-medium">可切换的访问地址</p>
          <ul className="grid gap-1" aria-label="可切换的访问地址">{entrances.map((item) => <li key={item.device_id} className="flex min-w-0 items-center gap-2">
            <span className="min-w-0 flex-1 truncate" title={item.gateway_url}>{device.directory?.members.find((member) => member.device_id === item.device_id)?.name || addressName(item.gateway_url)}</span>
            <span className="shrink-0 text-muted-foreground">{item.device_id === device.ready?.entry.device_id ? "当前" : item.public ? "公网" : "仅本机或局域网"}</span>
          </li>)}</ul>
        </> : null}
      </Popover.Content>
    </Popover.Portal>
  </Popover.Root>
}

/** Same layout as the workspace, shown while the browser loads its keys and connects. */
/** `home`: the conversation list loads on a phone, the last conversation on a wide screen. */
export function ShellSkeleton({ retry = null, onRetry, onLeave, home = false }: { retry?: number | null; onRetry?: () => void; onLeave?: () => void; home?: boolean } = {}) {
  // A tap on reconnect holds the offline view with a turning icon until the attempt ends: connected, or a new countdown.
  const [retrying, setRetrying] = useState(false)
  useEffect(() => { if (retry !== null) setRetrying(false) }, [retry])
  const shown = useRef(retry)
  if (retry !== null) shown.current = retry
  const offline = retry !== null || retrying
  // Offline, the page says it once in the middle; the corner status is only for a connection still in progress.
  const status = offline ? null : <><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />连接中</>
  return <div className="flex h-dvh" aria-busy={!offline}>
    <div className="hidden w-64 shrink-0 flex-col gap-3 border-r bg-sidebar p-4 md:flex">
      <Wordmark />
      <Skeleton className="mt-2 h-9" /><Skeleton className="h-4 w-12" /><Skeleton className="h-9" /><Skeleton className="h-9" />
      {status ? <div className="mt-auto flex items-center gap-2 px-2 text-meta text-info" role="status">{status}</div> : null}
    </div>
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="flex h-14 items-center gap-3 border-b px-4">{home ? <span className="text-title font-semibold md:hidden">消息</span> : null}<Skeleton className={cn("h-5 w-32", home && "max-md:hidden")} />{status ? <span className="ml-auto flex items-center gap-2 text-meta text-info md:hidden">{status}</span> : null}</div>
      {offline ? <div className="my-auto"><EmptyState icon={WifiOff} title="连不上设备" action={<div className="grid justify-items-center gap-3">
        <p className="flex items-center gap-1 text-meta text-muted-foreground"><span aria-hidden={retrying || undefined}>{retrying ? "正在重连" : `${shown.current} 秒后自动重试`}</span>
          {/* The countdown ticks every second, so only the state change is announced. */}
          <span role="status" className="sr-only">{retrying ? "正在重连" : "连不上设备"}</span>
          {onRetry ? <Button type="button" variant="ghost" size="icon-sm" className="hit-area relative text-muted-foreground" aria-label="立即重连" title="立即重连" disabled={retrying} aria-busy={retrying || undefined}
            onClick={() => { setRetrying(true); onRetry() }}><RefreshCw aria-hidden="true" className={cn(retrying && "animate-spin")} /></Button> : null}
        </p>
        {onLeave ? <Button type="button" variant="ghost" onClick={onLeave}>换个方式连接</Button> : null}
      </div>} /></div> : <>
        {onLeave ? <div className="flex justify-center px-4 pt-6"><Button type="button" variant="ghost" onClick={onLeave}>换个方式连接</Button></div> : null}
        {home ? <div className="grid gap-2 p-4 md:hidden" aria-hidden="true">{[0, 1, 2, 3].map((item) => <Skeleton key={item} className="h-16 rounded-xl" />)}</div> : null}
        <div className={cn("contents", home && "max-md:hidden")}><ChatSkeleton /></div>
      </>}
    </div>
  </div>
}

export function ChatSkeleton() {
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6 px-4 py-6 sm:px-6" aria-hidden="true">
    <Skeleton className="ml-auto h-10 w-2/5 rounded-2xl" />
    <div className="grid gap-2"><Skeleton className="h-4 w-4/5" /><Skeleton className="h-4 w-3/5" /><Skeleton className="h-4 w-2/3" /></div>
    <Skeleton className="ml-auto h-10 w-1/3 rounded-2xl" />
    <div className="grid gap-2"><Skeleton className="h-4 w-3/4" /><Skeleton className="h-4 w-1/2" /></div>
  </div>
}
