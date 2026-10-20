"use client"

import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import { Link, useNavigate, useRouterState } from "@tanstack/react-router"
import { ArrowRightLeft, Inbox, LoaderCircle, MonitorSmartphone, Plus, Settings, ShieldCheck, Wifi, WifiOff, type LucideIcon } from "lucide-react"
import { terms } from "@/lib/glossary"
import { Popover } from "radix-ui"
import { cn } from "cn"
import { Skeleton, toast } from "@/components/app/kit"
import { joinsOnScreen, useOpenJoins } from "@/features/devices/join"
import { useDevice } from "@/features/identity/context"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { Button } from "@/components/ui/button"
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupAction, SidebarGroupContent, SidebarGroupLabel, SidebarHeader,
  SidebarInset, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarTrigger, useSidebar,
} from "@/components/ui/sidebar"
import { useWorkspace } from "./context"
import { BotForm } from "./bot-form"
import { useInbox } from "./inbox"
import { addressName } from "./labels"

export function WorkspaceShell({ children }: { children: ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(() => typeof window === "undefined" || window.matchMedia("(min-width: 1024px)").matches)
  useEffect(() => {
    const wide = window.matchMedia("(min-width: 1024px)")
    const follow = () => { if (wide.matches) setSidebarOpen(true) }
    wide.addEventListener("change", follow)
    return () => wide.removeEventListener("change", follow)
  }, [])
  return <SidebarProvider className="workspace-shell" open={sidebarOpen} onOpenChange={setSidebarOpen}><WorkspaceLayout>{children}</WorkspaceLayout></SidebarProvider>
}

const CreateBot = createContext<() => void>(() => {})
/** Opens the new-assistant form from anywhere inside the workspace. */
export const useCreateBot = () => useContext(CreateBot)

const titles: Record<string, string> = { "/inbox": "待处理", "/devices": "我的设备", "/settings": "设置" }

function WorkspaceLayout({ children }: { children: ReactNode }) {
  const workspace = useWorkspace()
  const device = useDevice()
  const inbox = useInbox()
  const { setOpenMobile } = useSidebar()
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const chatRoute = pathname.startsWith("/b/")
  const [creatingBot, setCreatingBot] = useState(false)
  const creatorTrigger = useRef<HTMLElement | null>(null)
  const title = chatRoute ? workspace.selectedBot?.name ?? "" : titles[pathname] ?? (pathname.startsWith("/devices") ? "我的设备" : "")
  const navigate = useNavigate()
  const joins = useOpenJoins()
  const announced = useRef(new Set<string>())
  const devicesAttention = joins.length > 0 || workspace.nodes.some((node) => !node.revoked && !node.online)

  useEffect(() => { setOpenMobile(false) }, [pathname, setOpenMobile])
  // ponytail: join requests aren't pushed; admins poll every 30s. Push them with the directory if this gets busy.
  useEffect(() => {
    if (!device.ready?.principal.can_manage) return
    const refresh = () => { device.refreshDirectory().catch(() => {}) }
    refresh()
    const timer = setInterval(refresh, 30_000)
    return () => clearInterval(timer)
  }, [device.ready])
  useEffect(() => {
    for (const join of joins) {
      if (announced.current.has(join.join_id)) continue
      announced.current.add(join.join_id)
      if (joinsOnScreen()) continue
      toast(`${join.name || "新设备"} 想加入`, { action: { label: "查看", onClick: () => { void navigate({ to: "/inbox" }) } } })
    }
  }, [joins.map((join) => join.join_id).join()])
  useEffect(() => { if (workspace.noticeText) toast(workspace.noticeText) }, [workspace.noticeText])
  useEffect(() => { if (workspace.error) toast(workspace.error, { tone: "danger" }) }, [workspace.error])

  function openCreator() {
    creatorTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setCreatingBot(true); setOpenMobile(false)
  }

  const waitingHere = Boolean(workspace.data?.works.some((work) => work.open_approvals.length))

  return (
    <CreateBot.Provider value={openCreator}>
      <a className="skip-link" href="#main">跳到主要内容</a>
      <Sidebar aria-label="导航">
        <SidebarHeader className="shrink-0 px-4 pt-4 pb-2">
          <div className="flex h-8 items-center">
            <Link to="/" className="text-heading font-semibold tracking-tight">vibeface</Link>
          </div>
        </SidebarHeader>
        <SidebarContent className="gap-1 px-2 pb-3">
          <SidebarGroup className="pb-0">
            <SidebarMenu>
              <NavItem to="/inbox" icon={Inbox} label="待处理" active={pathname === "/inbox"} badge={inbox.length || undefined} />
            </SidebarMenu>
          </SidebarGroup>
          <SidebarGroup>
            <SidebarGroupLabel>助手</SidebarGroupLabel>
            <SidebarGroupAction title="新助手" aria-label="新助手" disabled={!workspace.writable && !workspace.pendingBot} onClick={openCreator}><Plus /></SidebarGroupAction>
            <SidebarGroupContent>
              <SidebarMenu>
                {workspace.bots.map((bot) => {
                  const active = chatRoute && bot.bot_id === workspace.selectedBot?.bot_id
                  const marked = !active && (workspace.notifications.some((item) => item.conversation_id === bot.conversation_id) || bot.bot_id === workspace.selectedBot?.bot_id && waitingHere)
                  return <SidebarMenuItem key={bot.bot_id}>
                    <SidebarMenuButton asChild isActive={active} className="h-10 max-md:h-11">
                      <Link to="/b/$botId" params={{ botId: bot.bot_id }} aria-current={active ? "page" : undefined} title={bot.name}>
                        <span className="min-w-0 flex-1 truncate">{bot.name}</span>
                        {marked ? <span aria-label="有新进展" className="size-2 shrink-0 rounded-full bg-warning" /> : null}
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
          <SidebarGroup className="pt-0">
            <SidebarMenu>
              <NavItem to="/devices" icon={MonitorSmartphone} label="设备" active={pathname.startsWith("/devices")} dot={devicesAttention} />
            </SidebarMenu>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="shrink-0 gap-1 px-2 py-3">
          <ConnectionCapsule />
          <SidebarMenu>
            <NavItem to="/settings" icon={Settings} label="设置" active={pathname === "/settings"} />
          </SidebarMenu>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset id="main" tabIndex={-1} className="min-h-0 min-w-0 overflow-hidden">
        <header className="flex h-14 shrink-0 items-center gap-2 border-b ps-[max(0.75rem,env(safe-area-inset-left))] pe-[max(0.75rem,env(safe-area-inset-right))] sm:ps-[max(1.5rem,env(safe-area-inset-left))] sm:pe-[max(1.5rem,env(safe-area-inset-right))]">
          <SidebarTrigger aria-label="菜单" title="菜单" />
          <h1 className="min-w-0 flex-1 truncate text-heading font-semibold" title={title}>{title}</h1>
          <Button asChild variant="ghost" size="icon" className="relative md:hidden">
            <Link to="/inbox" aria-label={inbox.length ? `待处理 ${inbox.length}` : "待处理"}>
              <Inbox aria-hidden="true" />
              {inbox.length ? <span className="absolute top-1 right-1 grid min-w-4 place-items-center rounded-full bg-warning px-1 text-[11px] leading-4 font-semibold text-background">{inbox.length}</span> : null}
            </Link>
          </Button>
        </header>
        {workspace.data && workspace.incomplete ? <div role="progressbar" aria-label="同步中" className="h-0.5 shrink-0 overflow-hidden bg-info-soft"><div className="h-full w-1/3 animate-pulse bg-info" /></div> : null}
        {/* Kept laid out while hidden so the chat keeps its scroll position across pages. */}
        <div className={cn("flex min-h-0 min-w-0 flex-1", !chatRoute && "invisible absolute inset-x-0 top-14 bottom-0")} inert={!chatRoute} aria-hidden={!chatRoute || undefined}>
          <div className="conversation-view"><ConversationScreen /></div>
          {chatRoute ? children : null}
        </div>
        {chatRoute ? null : <div className="workspace-screen">{children}</div>}
      </SidebarInset>
      <BotForm open={creatingBot} onOpenChange={setCreatingBot} restoreFocus={() => {
        const target = creatorTrigger.current
        if (target?.isConnected && target.getClientRects().length) target.focus()
        else document.getElementById("main")?.focus()
      }} />
    </CreateBot.Provider>
  )
}

function NavItem({ to, icon: Icon, label, active, badge, dot }: { to: string; icon: LucideIcon; label: string; active: boolean; badge?: number; dot?: boolean }) {
  return <SidebarMenuItem>
    <SidebarMenuButton asChild isActive={active} className="h-10 max-md:h-11">
      <Link to={to} aria-current={active ? "page" : undefined}>
        <Icon aria-hidden="true" />
        <span className="flex-1">{label}</span>
        {badge ? <span className="grid min-w-5 place-items-center rounded-full bg-warning px-1.5 text-meta font-semibold text-background tabular-nums">{badge}</span> : null}
        {dot ? <span aria-label="需要留意" className="size-2 rounded-full bg-warning" /> : null}
      </Link>
    </SidebarMenuButton>
  </SidebarMenuItem>
}

function ConnectionCapsule() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { link, entrances } = device
  const address = device.ready?.entry.address ?? ""
  const entryId = device.ready?.entry.device_id
  const entryName = device.directory?.members.find((member) => member.device_id === entryId)?.name || workspace.info?.name
  const name = entryName ? `经 ${entryName}` : addressName(address)
  // Devices of another network this browser opened before; the current network fails over by itself.
  const others = device.entries.filter((item) => !item.local && !device.directory?.members.some((member) => member.device_id === item.device_id))
  const state: { label: string; icon: LucideIcon; tone: string } = link.kind === "connecting"
    ? { label: entrances.length > 1 ? "切换中" : "连接中", icon: ArrowRightLeft, tone: "text-info" }
    : !workspace.connectionOnline || link.kind === "offline"
      ? { label: "离线", icon: WifiOff, tone: "text-muted-foreground" }
      : workspace.readOnlyNotice
      ? { label: terms.readOnly.label, icon: terms.readOnly.icon, tone: "text-warning" }
      : link.kind === "online" && link.catchingUp || workspace.writeNotice || workspace.data && workspace.incomplete
        ? { label: terms.syncing.label, icon: terms.syncing.icon, tone: "text-info" }
        : { label: "在线", icon: Wifi, tone: "text-success" }
  return <Popover.Root>
    <Popover.Trigger asChild>
      <button type="button" className="flex h-10 w-full items-center gap-2 rounded-md px-2 text-left text-meta hover:bg-sidebar-accent max-md:h-11" aria-label={`${name} · ${state.label}`}>
        <state.icon aria-hidden="true" className={cn("size-4 shrink-0", state.tone)} />
        <span className="min-w-0 flex-1 truncate">{name}</span>
        <span className={cn("shrink-0", state.tone)}>{state.label}</span>
      </button>
    </Popover.Trigger>
    <Popover.Portal>
      <Popover.Content side="top" align="start" sideOffset={6} className="z-50 grid w-72 gap-2 rounded-xl border bg-popover p-3 text-meta text-popover-foreground shadow-lg">
        <p className="font-medium">访问地址</p>
        <p className="break-all text-muted-foreground">{address}</p>
        {link.kind === "offline" ? <p className="text-muted-foreground">所有访问地址都连不上，{Math.ceil(link.retryInMs / 1000)} 秒后再试。</p>
          : workspace.connectionOnline ? <p className="flex items-center gap-1.5 text-success"><ShieldCheck aria-hidden="true" className="size-4" />端到端加密，已验证设备</p> : null}
        {workspace.readOnlyNotice ? <p className="text-warning">{workspace.readOnlyNotice}</p> : null}
        {others.length ? <>
          <p className="mt-1 font-medium">换一台设备</p>
          <ul className="grid gap-1" aria-label="换一台设备">{others.map((item) => <li key={`${item.address}:${item.device_id}`}>
            <button type="button" className="flex min-h-9 w-full min-w-0 items-center gap-2 rounded-md px-2 text-left hover:bg-muted" onClick={() => { void device.connect(item).catch(() => {}) }}>
              <span className="min-w-0 flex-1 truncate">{device.entryName(item) || addressName(item.address)}</span>
              <ArrowRightLeft aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            </button>
          </li>)}</ul>
        </> : null}
        {entrances.length > 1 ? <>
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
export function ShellSkeleton({ retry = null, onLeave }: { retry?: number | null; onLeave?: () => void } = {}) {
  const status = retry === null
    ? <><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />连接中</>
    : <><WifiOff aria-hidden="true" className="size-4" />连不上，{retry} 秒后重试</>
  return <div className="flex h-dvh" aria-busy="true">
    <div className="hidden w-64 shrink-0 flex-col gap-3 border-r bg-sidebar p-4 md:flex">
      <span className="text-heading font-semibold">vibeface</span>
      <Skeleton className="mt-2 h-9" /><Skeleton className="h-4 w-12" /><Skeleton className="h-9" /><Skeleton className="h-9" />
      <div className={cn("mt-auto flex items-center gap-2 px-2 text-meta", retry === null ? "text-info" : "text-muted-foreground")} role="status">{status}</div>
    </div>
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="flex h-14 items-center gap-3 border-b px-4"><Skeleton className="h-5 w-32" /><span className={cn("ml-auto flex items-center gap-2 text-meta md:hidden", retry === null ? "text-info" : "text-muted-foreground")}>{status}</span></div>
      {retry !== null && onLeave ? <div className="flex justify-center px-4 pt-6"><Button type="button" variant="ghost" onClick={onLeave}>换个方式连接</Button></div> : null}
      <ChatSkeleton />
    </div>
  </div>
}

export function ChatSkeleton() {
  return <div className="mx-auto grid w-full max-w-3xl gap-6 px-4 py-6 sm:px-6" aria-hidden="true">
    <Skeleton className="ml-auto h-10 w-2/5 rounded-2xl" />
    <div className="grid gap-2"><Skeleton className="h-4 w-4/5" /><Skeleton className="h-4 w-3/5" /><Skeleton className="h-4 w-2/3" /></div>
    <Skeleton className="ml-auto h-10 w-1/3 rounded-2xl" />
    <div className="grid gap-2"><Skeleton className="h-4 w-3/4" /><Skeleton className="h-4 w-1/2" /></div>
  </div>
}
