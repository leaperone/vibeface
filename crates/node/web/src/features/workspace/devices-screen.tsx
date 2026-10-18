"use client"

import { DeviceControls } from "@/features/identity/device-controls"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { useWorkspace } from "./context"
import type { ConversationSummary } from "./types"

export function DevicesScreen({ onCreateBot }: { onCreateBot(conversation: ConversationSummary): void }) {
  const workspace = useWorkspace()
  const oldConversations = workspace.conversations.filter((conversation) => !workspace.bots.some((bot) => bot.conversation_id === conversation.conversation_id))
  return (
    <section className="workspace-screen space-y-5" aria-label="设置" hidden={workspace.view !== "settings"}>
      <Card>
        <CardHeader><CardTitle>浏览器提醒</CardTitle></CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">需要你确认或工作完成时，接收此浏览器的通知。</p>
          <Button type="button" className="min-h-11 w-fit" variant="secondary" disabled={workspace.notificationButton.disabled} onClick={() => { void workspace.run(() => workspace.enableBrowserNotifications()) }}>
            {workspace.notificationButton.label}
          </Button>
        </CardContent>
      </Card>
      <DeviceControls />
      {oldConversations.length ? <Card>
        <CardHeader><CardTitle>旧聊天记录</CardTitle></CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">可以只读查看旧聊天，或接入一个机器人继续聊天。</p>
          <ul className="space-y-4">{oldConversations.map((conversation) => <li key={conversation.conversation_id} className="flex flex-wrap items-center justify-between gap-3">
            <p className="min-w-0 flex-1 break-words text-sm">{conversation.title || "未命名聊天"}</p>
            <div className="flex flex-wrap gap-2">
              <Button type="button" className="min-h-11" variant="outline" disabled={!workspace.connectionOnline} onClick={() => { void workspace.run(async () => { workspace.selectConversation(conversation.conversation_id); workspace.setView("chat"); await workspace.refresh() }) }}>只读查看</Button>
              <Button type="button" className="min-h-11" variant="secondary" disabled={!workspace.writable || Boolean(workspace.pendingBot)} onClick={() => onCreateBot(conversation)}>接入新机器人</Button>
            </div>
          </li>)}</ul>
          {workspace.pendingBot ? <p className="text-sm text-muted-foreground">先核对待创建的机器人，再接入其他聊天。</p> : null}
        </CardContent>
      </Card> : null}
    </section>
  )
}
