import { Laptop, Link2 } from "lucide-react"
import { Button } from "@/components/ui/button"

/** This computer's temporary session can look and change its settings but not sign; one strip says so and leads to the join page. */
export function LocalSession({ onJoin }: { onJoin(): void }) {
  return <div role="status" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b bg-info-soft px-4 py-2 sm:px-6">
    <Laptop aria-hidden="true" className="size-4 shrink-0 text-info" />
    <p className="min-w-0 flex-1"><span className="font-medium">本机管理</span><span className="ms-2 text-meta text-muted-foreground">加入后才能发消息</span></p>
    <Button type="button" variant="outline" className="max-md:min-h-11" onClick={onJoin}><Link2 aria-hidden="true" />用加入链接加入</Button>
  </div>
}
