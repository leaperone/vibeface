import { useEffect, useState, useSyncExternalStore } from "react"
import { Link, useRouterState } from "@tanstack/react-router"
import { ChevronRight, X } from "lucide-react"
import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { terms } from "@/lib/glossary"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { deviceCopy } from "@vibeface/client/identity/copy.ts"
import { parseNodeSettings } from "@vibeface/client/lib/node.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"

const key = (device: string) => `vibeface.ai-off-hint.${device}`
const listeners = new Set<() => void>()
const subscribe = (listener: () => void) => {
  listeners.add(listener); window.addEventListener("storage", listener)
  return () => { listeners.delete(listener); window.removeEventListener("storage", listener) }
}
const dismissed = (device: string) => { try { return localStorage.getItem(key(device)) === "dismissed" } catch { return false } }

/**
 * The computer this browser goes through has a provider installed, as step 4 of setting up finds them, but runs no bot
 * and executes no task: how a computer starts after joining. A quiet way to its AI settings, for whoever may change them.
 */
export function AiOffHint({ className }: { className?: string }) {
  const { ready } = useDevice()
  const { nodes } = useWorkspace()
  const entry = ready?.entry.device_id ?? ""
  const canSetUp = ready?.principal.can_manage === true || ready?.principal.source === "local_session"
  const isComputer = nodes.some((node) => node.device_id === entry && !node.revoked)
  const onPage = useRouterState({ select: (state) => state.location.pathname === `/devices/${entry}` })
  const closed = useSyncExternalStore(subscribe, () => entry !== "" && dismissed(entry), () => false)
  const [off, setOff] = useState(false)
  useEffect(() => {
    if (!ready || !canSetUp || !isComputer || closed || onPage) return
    let live = true
    ready.request("node_settings").then((raw) => {
      const { providers } = parseNodeSettings(raw)
      if (live) setOff(providers.some((item) => item.installed) && providers.every((item) => item.roles.length === 0))
    }, () => { if (live) setOff(false) })
    return () => { live = false }
  }, [ready, canSetUp, isComputer, closed, onPage])
  if (!off || closed || onPage || !canSetUp) return null
  function dismiss() {
    try { localStorage.setItem(key(entry), "dismissed") } catch {}
    listeners.forEach((listener) => listener())
  }
  return <div className={cn("flex min-w-0 items-center rounded-lg text-meta text-info hover:bg-muted/50", className)}>
    <Link to="/devices/$deviceId" params={{ deviceId: entry }} className="flex min-h-10 min-w-0 flex-1 items-center gap-2 ps-2 max-md:min-h-11">
      <terms.ai.icon aria-hidden="true" className="size-4 shrink-0" /><span className="truncate">{deviceCopy.aiOff}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0" />
    </Link>
    <Button type="button" variant="ghost" size="icon-sm" className="shrink-0 text-muted-foreground hover:bg-transparent max-md:size-11" aria-label="不再提示" title="不再提示" onClick={dismiss}><X aria-hidden="true" /></Button>
  </div>
}
