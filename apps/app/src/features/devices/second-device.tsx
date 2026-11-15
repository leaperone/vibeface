import { cn } from "@/lib/utils"
import { Link,useRouterState } from "@tanstack/react-router"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { entryAddress } from "@vibeface/client/lib/node.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { isLoopback } from "@vibeface/client/workspace/labels.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Laptop,Smartphone,X } from "lucide-react"
import { useSyncExternalStore } from "react"
import { useDeviceRows } from "./devices"

const key = "vibeface.second-device-hint"
const listeners = new Set<() => void>()
let closed = false
let replied = false
const dismissedBefore = () => { try { return closed || localStorage.getItem(key) === "dismissed" } catch { return closed } }
// The sidebar and the narrow-screen list each show the hint; closing one closes both, and other tabs.
const subscribe = (listener: () => void) => {
  listeners.add(listener); window.addEventListener("storage", listener)
  return () => { listeners.delete(listener); window.removeEventListener("storage", listener) }
}

/** Once a Bot has replied, while the network holds only this computer and this browser, a quiet way into 添加设备. */
export function SecondDeviceHint({ className }: { className?: string }) {
  const { ready, directory, entrances } = useDevice()
  const { bots } = useWorkspace()
  // Kept for the page's life so the hint does not blink out while the next reply is pending.
  replied ||= bots.some((bot) => bot.last_message?.role === "assistant")
  const rows = useDeviceRows()
  const onDevices = useRouterState({ select: (state) => state.location.pathname.startsWith("/devices") })
  const dismissed = useSyncExternalStore(subscribe, dismissedBefore, () => false)
  if (!replied || dismissed || onDevices || !ready?.principal.can_manage || !directory || directory.revoked.length || rows.length > 2) return null
  // A loopback-only link opens on no phone; another computer still joins it with `vibeface join`.
  const localOnly = isLoopback(entryAddress(ready.entry)) && !entrances.some((entrance) => entrance.public)
  const Icon = localOnly ? Laptop : Smartphone
  function dismiss() {
    closed = true
    try { localStorage.setItem(key, "dismissed") } catch {}
    listeners.forEach((listener) => listener())
  }
  return <div className={cn("flex min-w-0 items-center rounded-control text-sm text-secondary hover:bg-surface-muted/50", className)}>
    <Link to="/devices" search={{ add: localOnly ? "computer" : true }} className="flex min-h-10 min-w-0 flex-1 items-center gap-2 ps-2 hover:text-foreground max-md:min-h-11">
      <Icon aria-hidden="true" className="size-4 shrink-0" /><span className="truncate">{localOnly ? "在另一台电脑上使用" : "在手机或其他电脑上使用"}</span>
    </Link>
    <Button type="button" variant="ghost" size="md" className="shrink-0" aria-label="不再提示" title="不再提示" onClick={dismiss}><X aria-hidden="true" /></Button>
  </div>
}
