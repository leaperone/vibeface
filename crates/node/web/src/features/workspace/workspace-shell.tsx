"use client"

import { useEffect, useRef, useState } from "react"
import { BellIcon, ClipboardListIcon, MessageSquareIcon, MonitorIcon, PlusIcon, RefreshCwIcon, Settings2Icon, TerminalIcon, type LucideIcon } from "lucide-react"
import { useDevice } from "@/features/identity/context"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
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
import { ConversationView } from "./conversation-screen"
import { useWorkspace } from "./context"
import { DevicesScreen } from "./devices-screen"
import { nodeName } from "./labels"
import { NotificationsScreen } from "./notifications-screen"
import { SubmitButton } from "./present"
import { SessionsScreen } from "./sessions-screen"
import { newId } from "./storage"
import { TasksScreen } from "./tasks-screen"
import type { WorkspaceView } from "./types"

const views: Array<[WorkspaceView, string, LucideIcon]> = [
  ["chat", "会话", MessageSquareIcon],
  ["tasks", "工单", ClipboardListIcon],
  ["sessions", "执行会话", TerminalIcon],
  ["devices", "设备", MonitorIcon],
  ["notifications", "通知", BellIcon],
]

export function WorkspaceShell() {
  return <SidebarProvider className="workspace-shell"><WorkspaceLayout /></SidebarProvider>
}

function WorkspaceLayout() {
  const workspace = useWorkspace()
  const device = useDevice()
  const { setOpenMobile } = useSidebar()
  const titleRef = useRef<HTMLInputElement>(null)
  const newConversationRef = useRef<HTMLButtonElement>(null)
  const [creating, setCreating] = useState(false)
  const [creatingConversation, setCreatingConversation] = useState(false)
  const title = workspace.view === "chat"
    ? workspace.data?.conversation?.title || "新会话"
    : views.find(([view]) => view === workspace.view)?.[1]

  useEffect(() => {
    if (creating) titleRef.current?.focus()
  }, [creating])

  useEffect(() => {
    setOpenMobile(false)
  }, [workspace.view, workspace.conversationId, setOpenMobile])

  return (
    <>
      <a className="skip-link" href="#main">跳到主要内容</a>
      <Sidebar aria-label="导航和会话">
        <SidebarHeader className="shrink-0 gap-3 px-4 pt-4 pb-2">
          <div className="flex h-8 items-center gap-2">
            <a href="/" aria-label="Vibeface 首页" className="text-lg font-semibold tracking-tight">vibeface</a>
            <SidebarTrigger className="ml-auto md:hidden" aria-label="关闭侧栏" title="关闭侧栏" />
          </div>
          <Button ref={newConversationRef} type="button" className="w-full justify-start" onClick={() => setCreating(true)}>
            <PlusIcon aria-hidden="true" />新建会话
          </Button>
        </SidebarHeader>
        <SidebarContent className="gap-3 px-2 pb-3">
          <SidebarGroup className="shrink-0">
            <SidebarGroupContent>
              <nav aria-label="工作空间视图">
                <SidebarMenu>
                  {views.map(([view, label, Icon]) => (
                    <SidebarMenuItem key={view}>
                      <SidebarMenuButton
                        type="button"
                        className="h-9"
                        isActive={workspace.view === view}
                        aria-current={workspace.view === view ? "page" : undefined}
                        onClick={() => { workspace.setView(view); setOpenMobile(false) }}
                      >
                        <Icon aria-hidden="true" />
                        <span className="flex-1">{label}</span>
                        {view === "tasks" && Boolean(workspace.data?.tasks?.length) ? <Badge variant="secondary" className="h-5 min-w-5 px-1 text-xs">{workspace.data?.tasks.length}</Badge> : null}
                        {view === "notifications" && workspace.notifications.length > 0 ? <Badge variant="secondary" className="h-5 min-w-5 px-1 text-xs">{workspace.notifications.length}</Badge> : null}
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  ))}
                </SidebarMenu>
              </nav>
            </SidebarGroupContent>
          </SidebarGroup>
          <SidebarGroup className="pt-0">
            <SidebarGroupLabel>最近会话</SidebarGroupLabel>
            <SidebarGroupContent className="space-y-2">
              {creating ? (
                <form key={workspace.pendingConversation?.conversation_id ?? "new"}
                  className="space-y-3 rounded-lg border bg-background p-3"
                  onSubmit={(event) => {
                    event.preventDefault()
                    const form = event.currentTarget
                    if (!form.reportValidity()) return
                    setCreatingConversation(true)
                    void workspace.run(async () => {
                      await workspace.createConversation(newId(), String(new FormData(form).get("title") ?? "").trim())
                      form.reset()
                      setCreating(false)
                    }).finally(() => setCreatingConversation(false))
                  }}
                >
                  <Field>
                    <FieldLabel htmlFor="conversation-title">会话名称</FieldLabel>
                    <Input defaultValue={workspace.pendingConversation?.title ?? ""} readOnly={Boolean(workspace.pendingConversation)} ref={titleRef} id="conversation-title" name="title" required maxLength={200} placeholder="输入名称" />
                  </Field>
                  <div className="flex gap-2">
                    <SubmitButton busy={creatingConversation} disabled={!workspace.writable}>创建</SubmitButton>
                    <Button type="button" size="sm" variant="ghost" onClick={() => { setCreating(false); newConversationRef.current?.focus() }}>取消</Button>
                  </div>
                </form>
              ) : null}
              <SidebarMenu>
                {workspace.conversations.length ? workspace.conversations.map((conversation) => (
                  <SidebarMenuItem key={conversation.conversation_id}>
                    <SidebarMenuButton
                      type="button"
                      className="h-9"
                      isActive={workspace.view === "chat" && conversation.conversation_id === workspace.conversationId}
                      aria-current={workspace.view === "chat" && conversation.conversation_id === workspace.conversationId ? "page" : undefined}
                      title={conversation.title || "未命名会话"}
                      onClick={() => {
                        setOpenMobile(false)
                        void workspace.run(async () => {
                          workspace.selectConversation(conversation.conversation_id)
                          workspace.setView("chat")
                          await workspace.refresh()
                        })
                      }}
                    >
                      <span>{conversation.title || "未命名会话"}</span>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                )) : <SidebarMenuItem className="px-2 py-3 text-xs text-muted-foreground">暂无会话</SidebarMenuItem>}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="max-h-[45%] shrink-0 gap-2 overflow-y-auto border-t px-4 py-3">
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
            <span>{workspace.connectionOnline ? "已连接" : "连接已中断"}</span>
            {workspace.info.node_id ? <span className="ml-auto truncate" title={workspace.info.node_id}>{nodeName(workspace.info.node_id)}</span> : null}
          </div>
          <details className="text-xs">
            <summary className="cursor-pointer rounded-md py-1 text-muted-foreground">切换连接入口</summary>
            <SidebarMenu className="mt-1 max-h-32 overflow-y-auto">
              {device.entries.map((entry) => {
                const current = device.ready?.entry.address === entry.address
                  && device.ready.entry.node_id === entry.node_id
                  && device.ready.entry.relay === entry.relay
                return (
                  <SidebarMenuItem key={`${entry.address}:${entry.node_id}:${entry.relay}`}>
                    <SidebarMenuButton
                      type="button"
                      size="sm"
                      isActive={current}
                      aria-current={current ? "location" : undefined}
                      title={`${entry.node_id}${entry.relay ? " · 经中继连接" : ""}`}
                      onClick={() => { if (!current) void workspace.run(() => device.connect(entry)) }}
                    >
                      <span>{entry.address.replace(/^https?:\/\//, "")}</span>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                )
              })}
            </SidebarMenu>
          </details>
          <Button type="button" size="sm" variant="ghost" className="h-8 justify-start px-0 text-muted-foreground" onClick={device.disconnect}>
            <Settings2Icon aria-hidden="true" />管理连接
          </Button>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset id="main" tabIndex={-1} className="min-h-0 min-w-0 overflow-hidden">
        <header className="flex h-14 shrink-0 items-center gap-3 border-b px-4 sm:px-6">
          <SidebarTrigger aria-label="切换侧栏" title="切换侧栏" />
          <h1 className="min-w-0 flex-1 truncate text-sm font-semibold" title={title}>{title}</h1>
          <Button type="button" size="icon" variant="ghost" aria-label="刷新状态" title="刷新状态" aria-busy={workspace.refreshing} onClick={() => { void workspace.run(async () => { await workspace.refresh(); workspace.announce("已刷新。") }) }}>
            {workspace.refreshing ? <Spinner /> : <RefreshCwIcon aria-hidden="true" />}
          </Button>
        </header>
        {workspace.writeNotice || workspace.noticeText || workspace.error || workspace.data && workspace.incomplete ? (
          <div className="workspace-notices space-y-2">
            {workspace.writeNotice ? <Alert role="status"><AlertDescription>{workspace.writeNotice}</AlertDescription></Alert> : null}
            {workspace.noticeText ? <Alert role="status"><AlertDescription>{workspace.noticeText}</AlertDescription></Alert> : null}
            {workspace.error ? <Alert variant="destructive" role="alert"><AlertDescription>{workspace.error}</AlertDescription></Alert> : null}
            {workspace.data && workspace.incomplete ? <Alert role="status" data-incomplete="true"><AlertDescription>{workspace.syncText}</AlertDescription></Alert> : null}
          </div>
        ) : null}
        <ConversationView />
        <TasksScreen />
        <SessionsScreen />
        <DevicesScreen />
        <NotificationsScreen />
      </SidebarInset>
    </>
  )
}
