"use client"

import { DeviceControls } from "@/features/identity/device-controls"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { useWorkspace } from "./context"

export function DevicesScreen() {
  const workspace = useWorkspace()
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
    </section>
  )
}
