"use client"

import { useEffect, useRef, useState } from "react"
import { PlusIcon, RefreshCwIcon, Settings2Icon } from "lucide-react"
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
import { Spinner } from "@/components/ui/spinner"
import { useWorkspace } from "./context"
import { DevicesScreen } from "./devices-screen"
import { BotForm } from "./bot-form"
import { useDevice } from "../identity/context"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"

export function WorkspaceShell() {
  return <SidebarProvider className="workspace-shell"><WorkspaceLayout /></SidebarProvider>
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
                    const unread = workspace.notifications.some((item) => item.conversation_id === bot.conversation_id)
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
                          {unread ? <><span className="size-1.5 shrink-0 rounded-full bg-primary" aria-hidden="true" /><span className="sr-only">有未读进展</span></> : null}
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    )
                  }) : <SidebarMenuItem className="px-2 py-3 text-xs text-muted-foreground">创建一个机器人，开始聊天。</SidebarMenuItem>}
                </SidebarMenu>
              </nav>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="shrink-0 gap-3 px-4 py-3">
          {device.spaces.length > 1 && <label className="grid gap-1 text-xs text-muted-foreground">当前空间<NativeSelect aria-label="切换空间" value={device.ready?.scope.space_id ?? ''} onChange={event => void workspace.run(() => device.selectSpace(event.target.value))}>{device.spaces.map(space => <NativeSelectOption key={space.spaceId} value={space.spaceId}>{space.spaceId.slice(0, 17)}</NativeSelectOption>)}</NativeSelect></label>}
          {device.ready && <label className="grid min-w-0 gap-1 text-xs text-muted-foreground">当前项目<NativeSelect aria-label="切换项目" value={device.ready.project.id} onChange={event => void workspace.run(() => device.selectProject(event.target.value))}>{device.ready.projects.map(project => <NativeSelectOption key={project.id} value={project.id}>{project.id.slice(0, 17)}</NativeSelectOption>)}</NativeSelect></label>}
          {device.recoveryReadProjects.length > 0 && <label className="grid min-w-0 gap-1 text-xs text-muted-foreground">恢复后保留只读的原项目<NativeSelect aria-label="读取原 Node 的项目" value="" onChange={event => {
            const link = device.recoveryReadProjects.find(item => `${item.spaceId}/${item.projectId}` === event.target.value)
            if (link) void workspace.run(() => device.connect(link.entry, link.spaceId, link.projectId))
          }}><NativeSelectOption value="">选择原项目，需原 Node 在线</NativeSelectOption>{device.recoveryReadProjects.map(link => <NativeSelectOption key={`${link.spaceId}/${link.projectId}`} value={`${link.spaceId}/${link.projectId}`} disabled={link.expiresAt <= Date.now()}>{link.projectId.slice(0, 17)} · {link.expiresAt <= Date.now() ? '已过期' : '只读'}</NativeSelectOption>)}</NativeSelect></label>}
          {device.ready?.gateway && <p role="status" className="break-all text-xs text-muted-foreground" title={`${device.ready.gateway.entry.node_id} → ${device.ready.entry.node_id}`}>
            {device.ready.gateway.status === 'ready' ? '经网关' : device.ready.gateway.status === 'connecting' ? '连接网关中' : '网关不可用'} {device.ready.gateway.entry.address.replace(/^https?:\/\//, '')} → 项目 Node
          </p>}
          <Button type="button" size="sm" variant="ghost" className="h-8 justify-start px-0 text-muted-foreground" onClick={device.lockVault}>锁定设备仓库</Button>
          <div className="flex min-w-0 items-center gap-2 text-xs text-muted-foreground" data-connection={workspace.connectionOnline ? "online" : "offline"} title={workspace.connectionText}>
            <span className={`size-1.5 shrink-0 rounded-full ${workspace.connectionOnline ? "bg-emerald-500" : "bg-destructive"}`} aria-hidden="true" />
            <span>{workspace.connectionOnline ? "已连接" : "正在恢复连接"}</span>
          </div>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton type="button" className="min-h-11" isActive={workspace.view === "settings"} aria-current={workspace.view === "settings" ? "page" : undefined} onClick={() => { workspace.setView("settings"); setOpenMobile(false) }}>
                <Settings2Icon aria-hidden="true" /><span>设置</span>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset id="main" tabIndex={-1} className="min-h-0 min-w-0 overflow-hidden">
        <header className="flex h-14 shrink-0 items-center gap-3 border-b px-4 sm:px-6">
          <SidebarTrigger aria-label="切换侧栏" title="切换侧栏" />
          <h1 className="min-w-0 flex-1 truncate text-sm font-semibold" title={title}>{title}</h1>
          {workspace.view === "chat" && !workspace.selectedBot ? <Button type="button" variant="secondary" disabled={!workspace.writable && !workspace.pendingBot} onClick={() => openCreator()}>{createLabel}</Button> : null}
          <Button type="button" size="icon" variant="ghost" disabled={!workspace.connectionOnline} aria-label="刷新状态" title="刷新状态" aria-busy={workspace.refreshing} onClick={() => { void workspace.run(async () => { await workspace.refresh(); workspace.announce("已刷新。") }) }}>
            {workspace.refreshing ? <Spinner /> : <RefreshCwIcon aria-hidden="true" />}
          </Button>
        </header>
        {!workspace.connectionOnline || workspace.writeNotice || workspace.noticeText || workspace.error || workspace.data && workspace.incomplete ? (
          <div className="workspace-notices space-y-2">
            {!workspace.connectionOnline ? <Alert role="status"><AlertDescription>{workspace.connectionText}后台任务不会因网页断线而自动停止，当前显示最后核对的进展。</AlertDescription></Alert> : null}
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
