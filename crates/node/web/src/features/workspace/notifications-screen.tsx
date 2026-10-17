"use client"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { useWorkspace } from "./context"
import { ActionButton } from "./present"

export function NotificationsScreen() {
  const workspace = useWorkspace()
  return (
    <section className="workspace-screen space-y-5" aria-label="通知" hidden={workspace.view !== "notifications"}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">通知 <span className="text-sm font-normal text-muted-foreground">{workspace.notifications.length} 条未读</span></h2>
        <Button
          type="button"
          variant="secondary"
          disabled={workspace.notificationButton.disabled}
          onClick={() => { void workspace.run(() => workspace.enableBrowserNotifications()) }}
        >
          {workspace.notificationButton.label}
        </Button>
      </div>
      <div className="grid gap-4">
        {workspace.notifications.length ? workspace.notifications.map((notification) => (
          <Card key={notification.notification_id}>
            <CardHeader className="min-w-0">
              <CardTitle>{notification.kind === "approval" ? "等待人工审批" : "任务进展"}</CardTitle>
            </CardHeader>
            <CardContent>
              <p className="whitespace-pre-line break-words">{notification.text}</p>
              <div className="flex flex-wrap gap-2">
                <ActionButton action={() => workspace.run(() => workspace.openNotification(notification))}>查看相关会话</ActionButton>
                <ActionButton variant="ghost" action={() => workspace.run(() => workspace.ack(notification.notification_id))}>标为已读</ActionButton>
              </div>
            </CardContent>
          </Card>
        )) : <p className="py-10 text-center text-sm text-muted-foreground">没有未读通知。</p>}
      </div>
    </section>
  )
}
