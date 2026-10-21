import { Eye, Link2 } from "lucide-react"
import { Button } from "@/components/ui/button"

/** A local session can look but not sign; one strip says so and leads to the join page. */
export function NotJoined({ onJoin }: { onJoin(): void }) {
  return <div role="status" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b bg-info-soft px-4 py-2 sm:px-6">
    <Eye aria-hidden="true" className="size-4 shrink-0 text-info" />
    <p className="min-w-0 flex-1"><span className="font-medium">这个浏览器还没加入</span><span className="text-muted-foreground max-sm:hidden"> · 只能查看</span></p>
    <Button type="button" variant="outline" className="max-md:min-h-11" onClick={onJoin}><Link2 aria-hidden="true" />用加入链接加入</Button>
  </div>
}
