import { useState } from "react"
import { Link, useRouterState } from "@tanstack/react-router"
import { Smartphone, X } from "lucide-react"
import { cn } from "cn"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useDeviceRows } from "./devices"

const key = "vibeface.second-device-hint"
const dismissedBefore = () => { try { return localStorage.getItem(key) === "dismissed" } catch { return false } }

/** While a new network holds only this computer and this browser, a quiet way into 添加设备. */
export function SecondDeviceHint({ className }: { className?: string }) {
  const { ready, directory } = useDevice()
  const rows = useDeviceRows()
  const onDevices = useRouterState({ select: (state) => state.location.pathname.startsWith("/devices") })
  const [dismissed, setDismissed] = useState(dismissedBefore)
  if (dismissed || onDevices || !ready?.principal.can_manage || !directory || directory.revoked.length || rows.length > 2) return null
  function dismiss() {
    try { localStorage.setItem(key, "dismissed") } catch {}
    setDismissed(true)
  }
  return <div className={cn("flex min-w-0 items-center rounded-lg text-meta text-muted-foreground hover:bg-muted/50", className)}>
    <Link to="/devices" search={{ add: true }} className="flex min-h-10 min-w-0 flex-1 items-center gap-2 ps-2 hover:text-foreground max-md:min-h-11">
      <Smartphone aria-hidden="true" className="size-4 shrink-0" /><span className="truncate">在手机或其他电脑上使用</span>
    </Link>
    <Button type="button" variant="ghost" size="icon-sm" className="shrink-0 hover:bg-transparent max-md:size-11" aria-label="不再提示" title="不再提示" onClick={dismiss}><X aria-hidden="true" /></Button>
  </div>
}
