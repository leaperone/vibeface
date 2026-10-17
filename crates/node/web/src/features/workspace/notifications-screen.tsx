"use client"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { useWorkspace } from "./context"
import { ActionButton, EmptyState } from "./present"

export function NotificationsScreen() {
  const workspace = useWorkspace()
  return (
    <section aria-label="通知" hidden={workspace.view !== "notifications"}>
      <div>
        <h2>需要你关注的进展</h2>
        <p>普通状态通知不会调用模型。离线期间的未读通知会在重连后显示。</p>
        <Button
          type="button"
          variant="secondary"
          disabled={workspace.notificationButton.disabled}
          onClick={() => { void workspace.run(() => workspace.enableBrowserNotifications()) }}
        >
          {workspace.notificationButton.label}
        </Button>
      </div>
      {workspace.notifications.length ? workspace.notifications.map((notification) => (
        <Card key={notification.notification_id}>
          <CardHeader>
            <CardTitle>{notification.kind === "approval" ? "等待人工审批" : "任务进展"}</CardTitle>
          </CardHeader>
          <CardContent>
            <p>{notification.text}</p>
            <ActionButton action={() => workspace.run(() => workspace.openNotification(notification))}>查看相关会话</ActionButton>
            <ActionButton variant="ghost" action={() => workspace.run(() => workspace.ack(notification.notification_id))}>标为已读</ActionButton>
          </CardContent>
        </Card>
      )) : <EmptyState title="暂时没有未读通知" detail="需要你处理的进展会在这里保留，直到你确认已读。" />}
    </section>
  )
}
