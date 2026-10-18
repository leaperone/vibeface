"use client"

import { useEffect, useRef, useState } from "react"
import { PlusIcon, RefreshCwIcon, Settings2Icon, UnplugIcon } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { StatusOrb } from "@/components/status-orb"
import { useDevice } from "@/features/identity/context"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
  useSidebar,
} from "@/components/ui/sidebar"
import { useWorkspace } from "./context"
import { DevicesScreen } from "./devices-screen"
import { BotForm } from "./bot-form"

export function WorkspaceShell() {
  const [sidebarOpen, setSidebarOpen] = useState(() => typeof window === "undefined" || window.matchMedia("(min-width: 1024px)").matches)
  return <SidebarProvider className="workspace-shell" open={sidebarOpen} onOpenChange={setSidebarOpen}><WorkspaceLayout /></SidebarProvider>
}

function WorkspaceLayout() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { setOpenMobile } = useSidebar()
  const [creatingBot, setCreatingBot] = useState(false)
  const creatorTrigger = useRef<HTMLElement | null>(null)
  const title = workspace.view === "chat" ? workspace.selectedBot?.name || "机器人" : "设置"
  const createLabel = workspace.pendingBot ? "核对机器人创建" : "创建机器人"

  function openCreator() {
    creatorTrigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setCreatingBot(true); setOpenMobile(false)
  }

  function restoreCreatorFocus() {
    const target = creatorTrigger.current
    if (target?.isConnected && target.getClientRects().length) target.focus()
    else document.getElementById("main")?.focus()
  }

  useEffect(() => {
    setOpenMobile(false)
  }, [workspace.view, workspace.conversationId, setOpenMobile])

  return (
    <>
      <a className="skip-link" href="#main">跳到主要内容</a>
      <Sidebar aria-label="机器人和设置">
        <SidebarHeader className="shrink-0 gap-3 px-4 pt-4 pb-2">
          <div className="flex h-8 items-center gap-2">
            <a href="/" aria-label="Vibeface 首页" className="text-lg font-semibold tracking-tight">vibeface</a>
            <SidebarTrigger className="ml-auto md:hidden" aria-label="关闭侧栏" title="关闭侧栏" />
          </div>
          <Button type="button" disabled={!workspace.writable && !workspace.pendingBot} className="min-h-11 w-full justify-start" onClick={() => openCreator()}>
            <PlusIcon aria-hidden="true" />{createLabel}
          </Button>
        </SidebarHeader>
        <SidebarContent className="gap-3 px-2 pb-3">
          <SidebarGroup>
            <SidebarGroupLabel>机器人</SidebarGroupLabel>
            <SidebarGroupContent>
              <nav aria-label="机器人列表">
                <SidebarMenu>
                  {workspace.bots.length ? workspace.bots.map((bot) => {
                    const marks = workspace.notifications.filter((item) => item.conversation_id === bot.conversation_id)
                    const mark = marks.some((item) => item.kind === "approval" || item.kind === "attention") ? "待决定" : marks.length ? "有进展" : ""
                    const active = workspace.view === "chat" && bot.bot_id === workspace.selectedBot?.bot_id
                    return (
                      <SidebarMenuItem key={bot.bot_id}>
                        <SidebarMenuButton
                          type="button"
                          className="min-h-11"
                          isActive={active}
                          aria-current={active ? "page" : undefined}
                          title={bot.name}
                          disabled={!workspace.connectionOnline}
                          onClick={() => {
                            setOpenMobile(false)
                            void workspace.run(() => workspace.selectBot(bot))
                          }}
                        >
                          <span className="min-w-0 flex-1 truncate">{bot.name}</span>
                          {mark ? <Badge variant="outline" className="shrink-0">{mark}</Badge> : null}
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    )
                  }) : <SidebarMenuItem className="px-2 py-3 text-xs text-muted-foreground">还没有机器人。</SidebarMenuItem>}
                </SidebarMenu>
              </nav>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="shrink-0 gap-3 px-4 py-3">
          <div className="flex min-w-0 items-center gap-2 text-xs text-muted-foreground" data-connection={workspace.connectionOnline ? "online" : "offline"} title={workspace.connectionText}>
            <StatusOrb place={workspace.connectionOnline ? "idle-link" : "connection"} size={20} />
            <span>{workspace.connectionOnline ? "已连接" : "正在恢复连接"}</span>
          </div>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton type="button" className="min-h-11" isActive={workspace.view === "settings"} aria-current={workspace.view === "settings" ? "page" : undefined} onClick={() => { workspace.setView("settings"); setOpenMobile(false) }}>
                <Settings2Icon aria-hidden="true" /><span>设置</span>
              </SidebarMenuButton>
            </SidebarMenuItem>
            <SidebarMenuItem>
              <SidebarMenuButton type="button" className="min-h-11" title="断开当前连接，回到连接页。已保存的入口还在。" onClick={() => device.disconnect()}>
                <UnplugIcon aria-hidden="true" /><span>断开</span>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset id="main" tabIndex={-1} className="min-h-0 min-w-0 overflow-hidden">
        <header className="flex h-14 shrink-0 items-center gap-2 border-b ps-[max(0.75rem,env(safe-area-inset-left))] pe-[max(0.75rem,env(safe-area-inset-right))] sm:gap-3 sm:ps-[max(1.5rem,env(safe-area-inset-left))] sm:pe-[max(1.5rem,env(safe-area-inset-right))]">
          <SidebarTrigger aria-label="切换侧栏" title="切换侧栏" />
          <h1 className="min-w-0 flex-1 truncate text-sm font-semibold" title={title}>{title}</h1>
          {workspace.view === "chat" && !workspace.selectedBot ? <Button type="button" variant="secondary" className="min-h-11 shrink-0" disabled={!workspace.writable && !workspace.pendingBot} onClick={() => openCreator()}><PlusIcon aria-hidden="true" /><span className="sr-only sm:not-sr-only">{createLabel}</span></Button> : null}
          <Button type="button" size="icon" variant="ghost" className="size-11 shrink-0" disabled={!workspace.connectionOnline} aria-label="刷新状态" title="刷新状态" aria-busy={workspace.refreshing} onClick={() => { void workspace.run(async () => { await workspace.refresh(); workspace.announce("已刷新。") }) }}>
            {workspace.refreshing ? <StatusOrb place="refresh" size={20} /> : <RefreshCwIcon aria-hidden="true" />}
          </Button>
        </header>
        {!workspace.connectionOnline || workspace.writeNotice || workspace.noticeText || workspace.error || workspace.data && workspace.incomplete ? (
          <div className="workspace-notices space-y-2">
            {!workspace.connectionOnline ? <Alert role="status"><AlertDescription>连接中断，正在重连。</AlertDescription></Alert> : null}
            {workspace.writeNotice ? <Alert role="status"><AlertDescription>{workspace.writeNotice}</AlertDescription></Alert> : null}
            {workspace.noticeText ? <Alert role="status"><AlertDescription>{workspace.noticeText}</AlertDescription></Alert> : null}
            {workspace.error ? <Alert variant="destructive" role="alert"><AlertDescription>{workspace.error}</AlertDescription></Alert> : null}
            {workspace.data && workspace.incomplete ? <Alert role="status" data-incomplete="true"><AlertDescription>{workspace.syncText}</AlertDescription></Alert> : null}
          </div>
        ) : null}
        <div className="conversation-view" hidden={workspace.view !== "chat"}><ConversationScreen /></div>
        <DevicesScreen />
      </SidebarInset>
      <BotForm open={creatingBot} onOpenChange={setCreatingBot} restoreFocus={restoreCreatorFocus} />
    </>
  )
}
