"use client"

import { useEffect, useRef, useState, type ReactNode } from "react"
import { Link, useNavigate, useRouterState } from "@tanstack/react-router"
import { Inbox, LoaderCircle, MonitorSmartphone, Plus, RefreshCw, Settings, ShieldCheck, Wifi, WifiOff, type LucideIcon } from "lucide-react"
import { Popover } from "radix-ui"
import { cn } from "cn"
import { Skeleton, toast } from "@/components/app/kit"
import { useIgnoredPairs } from "@/features/devices/ignored"
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
  return <SidebarProvider className="workspace-shell" open={sidebarOpen} onOpenChange={setSidebarOpen}><WorkspaceLayout>{children}</WorkspaceLayout></SidebarProvider>
}

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
  const ignored = useIgnoredPairs()
  const navigate = useNavigate()
  const joins = device.ready?.principal.can_manage === true ? device.pairs.filter((pair) => !ignored.includes(pair.request_id)) : []
  const announced = useRef(new Set<string>())
  const devicesAttention = joins.length > 0 || workspace.nodes.some((node) => !node.revoked && !node.online)

  useEffect(() => { setOpenMobile(false) }, [pathname, setOpenMobile])
  useEffect(() => {
    for (const pair of joins) {
      if (announced.current.has(pair.request_id)) continue
      announced.current.add(pair.request_id)
      toast(`${pair.review.new_member.name || "新设备"} 想加入`, { action: { label: "查看", onClick: () => { void navigate({ to: "/inbox" }) } } })
    }
  }, [joins.map((pair) => pair.request_id).join()])
  useEffect(() => { if (workspace.noticeText) toast(workspace.noticeText) }, [workspace.noticeText])
  useEffect(() => { if (workspace.error) toast(workspace.error, { tone: "danger" }) }, [workspace.error])

  function openCreator() {
    creatorTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setCreatingBot(true); setOpenMobile(false)
  }

  return (
    <>
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
                  const marked = workspace.notifications.some((item) => item.conversation_id === bot.conversation_id)
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
        <div className="flex min-h-0 min-w-0 flex-1" hidden={!chatRoute}>
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
    </>
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
  const address = device.ready?.entry.address ?? ""
  const name = addressName(address)
  const state: { label: string; icon: LucideIcon; tone: string } = !workspace.connectionOnline
    ? { label: "离线", icon: WifiOff, tone: "text-muted-foreground" }
    : workspace.writeNotice || workspace.data && workspace.incomplete
      ? { label: "同步中", icon: RefreshCw, tone: "text-info" }
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
      <Popover.Content side="top" align="start" sideOffset={6} className="z-50 grid w-64 gap-2 rounded-xl border bg-popover p-3 text-meta text-popover-foreground shadow-lg">
        <p className="font-medium">访问地址</p>
        <p className="break-all text-muted-foreground">{address}</p>
        <p className="flex items-center gap-1.5 text-success"><ShieldCheck aria-hidden="true" className="size-4" />端到端加密，已验证设备</p>
      </Popover.Content>
    </Popover.Portal>
  </Popover.Root>
}

/** Same layout as the workspace, shown while the browser loads its keys and connects. */
export function ShellSkeleton() {
  return <div className="flex h-dvh" aria-busy="true">
    <div className="hidden w-64 shrink-0 flex-col gap-3 border-r bg-sidebar p-4 md:flex">
      <span className="text-heading font-semibold">vibeface</span>
      <Skeleton className="mt-2 h-9" /><Skeleton className="h-4 w-12" /><Skeleton className="h-9" /><Skeleton className="h-9" />
      <div className="mt-auto flex items-center gap-2 px-2 text-meta text-info" role="status"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />连接中</div>
    </div>
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="flex h-14 items-center gap-3 border-b px-4"><Skeleton className="h-5 w-32" /><span className="ml-auto flex items-center gap-2 text-meta text-info md:hidden"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />连接中</span></div>
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
