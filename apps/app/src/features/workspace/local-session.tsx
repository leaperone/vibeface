import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Laptop,Link2 } from "lucide-react"

/** What a session opened on this computer, without joining, can do; the connect page and the strip say it the same way. */
export const localCopy = { manage: "本机管理", note: "能看和改设置，加入后才能聊天", join: "加入" }

/** This computer's temporary session can look and change its settings but not sign; one strip says so and opens the join form. */
export function LocalSession({ onJoin }: { onJoin(): void }) {
  return <div role="status" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b bg-info-subtle px-4 py-2 sm:px-6">
    <Laptop aria-hidden="true" className="size-4 shrink-0 text-info" />
    <p className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-2"><span className="font-medium">{localCopy.manage}</span><span className="text-sm text-secondary">{localCopy.note}</span></p>
    <Button type="button" variant="secondary" className="" onClick={onJoin}><Link2 aria-hidden="true" />{localCopy.join}</Button>
  </div>
}
