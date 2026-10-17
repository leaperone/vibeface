"use client"

import { useEffect, useRef, useState, type ReactNode } from "react"
import type { Session } from "@/lib/node"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Separator } from "@/components/ui/separator"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
} from "@/components/ui/sidebar"
import { Spinner } from "@/components/ui/spinner"
import { ConversationView } from "./conversation-screen"
import { WorkspaceProvider, useWorkspace } from "./context"
import { DevicesScreen } from "./devices-screen"
import { nodeName } from "./labels"
import { NotificationsScreen } from "./notifications-screen"
import { SubmitButton } from "./present"
import { SessionsScreen } from "./sessions-screen"
import { newId } from "./storage"
import { TasksScreen } from "./tasks-screen"
import type { ChatSlotState, WorkspaceView } from "./types"

const views: Array<[WorkspaceView, string]> = [
  ["chat", "会话"],
  ["tasks", "工单"],
  ["sessions", "执行会话"],
  ["devices", "设备"],
  ["notifications", "通知"],
]

export function WorkspaceShell({
  session,
  onLogout,
  chat,
}: {
  session: Session
  onLogout: () => void
  chat?: ReactNode | ((slot: ChatSlotState) => ReactNode)
}) {
  return (
    <WorkspaceProvider session={session} onLogout={onLogout}>
      <WorkspaceFrame chat={chat} />
    </WorkspaceProvider>
  )
}

function WorkspaceFrame({ chat }: { chat?: ReactNode | ((slot: ChatSlotState) => ReactNode) }) {
  const workspace = useWorkspace()
  const requestId = useRef<string | null>(null)
  const titleRef = useRef<HTMLInputElement>(null)
  const newConversationRef = useRef<HTMLButtonElement>(null)
  const [creating, setCreating] = useState(false)
  const [entryOpen, setEntryOpen] = useState(false)
  const [savingEntry, setSavingEntry] = useState(false)
  const [creatingConversation, setCreatingConversation] = useState(false)

  useEffect(() => {
    if (creating) titleRef.current?.focus()
  }, [creating])

  return (
    <>
      <a href="#main">跳到主要内容</a>
      <header>
        <a href="/" aria-label="Vibeface 首页">vibeface</a>
        <p data-connection={workspace.connectionOnline ? "online" : "offline"}>
          <span>{workspace.connectionText}</span>
          {workspace.info.node_id ? <span> {nodeName(workspace.info.node_id)}</span> : null}
        </p>
        <Button type="button" variant="ghost" onClick={() => { void workspace.run(() => workspace.logout()) }}>退出登录</Button>
      </header>
      {workspace.noticeText ? <Alert role="status"><AlertDescription>{workspace.noticeText}</AlertDescription></Alert> : null}
      {workspace.error ? <Alert variant="destructive" role="alert"><AlertDescription>{workspace.error}</AlertDescription></Alert> : null}
      <SidebarProvider>
        <Sidebar collapsible="none" aria-label="入口和会话">
          <SidebarContent>
            <SidebarGroup>
              <SidebarGroupLabel>我的入口</SidebarGroupLabel>
              <SidebarGroupContent>
                <p>此浏览器</p>
                <SidebarMenu>
                  {workspace.entries.map((origin) => {
                    const current = typeof window !== "undefined" && origin === window.location.origin
                    return (
                    <SidebarMenuItem key={origin}>
                      <SidebarMenuButton asChild isActive={current}>
                        <a
                          href={workspace.hrefForEntry(origin)}
                          aria-current={current ? "location" : undefined}
                          title={current ? "当前入口" : undefined}
                        >
                          {origin.replace(/^https?:\/\//, "")}
                        </a>
                      </SidebarMenuButton>
                      {current ? null : (
                        <Button type="button" variant="ghost" onClick={() => workspace.removeEntry(origin)}>移除</Button>
                      )}
                    </SidebarMenuItem>
                    )
                  })}
                </SidebarMenu>
                <Collapsible open={entryOpen} onOpenChange={setEntryOpen}>
                  <CollapsibleTrigger>添加入口</CollapsibleTrigger>
                  <CollapsibleContent>
                    <form
                      onSubmit={(event) => {
                        event.preventDefault()
                        const form = event.currentTarget
                        if (!form.reportValidity()) return
                        setSavingEntry(true)
                        void workspace.run(async () => {
                          workspace.saveEntry(String(new FormData(form).get("address") ?? ""))
                          form.reset()
                          setEntryOpen(false)
                        }).finally(() => setSavingEntry(false))
                      }}
                    >
                      <FieldGroup>
                        <Field>
                          <FieldLabel htmlFor="entry-address">入口地址</FieldLabel>
                          <Input id="entry-address" name="address" type="url" required placeholder="http://192.168.1.10:8080" />
                        </Field>
                        <SubmitButton busy={savingEntry} variant="secondary">保存地址</SubmitButton>
                      </FieldGroup>
                    </form>
                  </CollapsibleContent>
                </Collapsible>
              </SidebarGroupContent>
            </SidebarGroup>
            <Separator />
            <SidebarGroup>
              <SidebarGroupLabel>会话</SidebarGroupLabel>
              <SidebarGroupContent>
                <Button ref={newConversationRef} type="button" variant="ghost" onClick={() => setCreating(true)}>新建</Button>
                {creating ? (
                  <form
                    onSubmit={(event) => {
                      event.preventDefault()
                      const form = event.currentTarget
                      if (!form.reportValidity()) return
                      const id = requestId.current ?? (requestId.current = newId())
                      setCreatingConversation(true)
                      void workspace.run(async () => {
                        await workspace.createConversation(id, String(new FormData(form).get("title") ?? "").trim())
                        requestId.current = null
                        form.reset()
                        setCreating(false)
                      }).finally(() => setCreatingConversation(false))
                    }}
                  >
                    <FieldGroup>
                      <Field>
                        <FieldLabel htmlFor="conversation-title">会话名称</FieldLabel>
                        <Input ref={titleRef} id="conversation-title" name="title" required maxLength={200} placeholder="例如：重构支付模块" />
                      </Field>
                      <SubmitButton busy={creatingConversation} variant="secondary">创建</SubmitButton>
                      <Button type="button" variant="ghost" onClick={() => { setCreating(false); newConversationRef.current?.focus() }}>取消</Button>
                    </FieldGroup>
                  </form>
                ) : null}
                <ScrollArea>
                  <SidebarMenu>
                    {workspace.conversations.length ? workspace.conversations.map((conversation) => (
                      <SidebarMenuItem key={conversation.conversation_id}>
                        <SidebarMenuButton
                          type="button"
                          isActive={conversation.conversation_id === workspace.conversationId}
                          aria-current={conversation.conversation_id === workspace.conversationId ? "page" : undefined}
                          onClick={() => {
                            void workspace.run(async () => {
                              workspace.selectConversation(conversation.conversation_id)
                              await workspace.refresh()
                            })
                          }}
                        >
                          {conversation.title || "未命名会话"}
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    )) : <SidebarMenuItem>还没有会话</SidebarMenuItem>}
                  </SidebarMenu>
                </ScrollArea>
              </SidebarGroupContent>
            </SidebarGroup>
          </SidebarContent>
          <SidebarFooter>
            <span>{workspace.session.account.username || "我的账户"}</span>
            <span>仅管理你自己的设备</span>
          </SidebarFooter>
        </Sidebar>
        <SidebarInset id="main" tabIndex={-1}>
          <header>
            <p>个人工作空间</p>
            <h1>{workspace.data?.conversation?.title || "开始一个会话"}</h1>
            <Button type="button" variant="secondary" aria-busy={workspace.refreshing} onClick={() => { void workspace.run(async () => { await workspace.refresh(); workspace.announce("已刷新入口当前可读的状态。") }) }}>
              {workspace.refreshing ? <Spinner /> : null}
              刷新状态
            </Button>
          </header>
          <nav aria-label="工作空间视图">
            {views.map(([view, label]) => (
              <Button
                key={view}
                type="button"
                variant={workspace.view === view ? "default" : "ghost"}
                aria-current={workspace.view === view ? "page" : undefined}
                onClick={() => workspace.setView(view)}
              >
                {label}
                {view === "tasks" ? <Badge>{workspace.data?.tasks?.length ?? 0}</Badge> : null}
                {view === "notifications" ? <Badge>{workspace.notifications.length}</Badge> : null}
              </Button>
            ))}
          </nav>
          <Alert role="status" data-incomplete={workspace.incomplete ? "true" : "false"}>
            <AlertDescription>{workspace.syncText}</AlertDescription>
          </Alert>
          <ConversationView chat={chat} />
          <TasksScreen />
          <SessionsScreen />
          <DevicesScreen />
          <NotificationsScreen />
        </SidebarInset>
      </SidebarProvider>
      <footer>Vibeface {workspace.info.version ?? ""}<span>本地运行 · 由你控制</span></footer>
    </>
  )
}
