import { useLayoutEffect, useRef, useState } from 'react'
import { createFileRoute, Link } from '@tanstack/react-router'
import { CheckCheck, ChevronRight, EyeOff, Inbox, MessageCircle } from 'lucide-react'
import { claimFocus, EmptyState, Skeleton } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { useInbox } from '@/features/workspace/inbox'
import { QuestionCard } from '@/features/workspace/question-card'
import { JoinCard } from '@/features/devices/join'
import { WriteCard } from '@/features/workspace/write-card'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { RecoveryPendingCard, RotateHintCard } from '@/features/identity/recovery-controls'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'

export const Route = createFileRoute('/_app/inbox')({ component: InboxPage })

function InboxPage() {
  const items = useInbox()
  const workspace = useWorkspace()
  const attention = items.filter((item): item is Extract<typeof items[number], { kind: 'attention' }> => item.kind === 'attention')
  const [busy, setBusy] = useState<"ignore" | "read" | null>(null)
  // A handled card leaves with the button just pressed; the card now in its place, or the page title, takes the focus.
  const list = useRef<HTMLUListElement>(null)
  const held = useRef<{ key: string; index: number } | null>(null)
  const keys = items.map((item) => item.key).join('\n')
  useLayoutEffect(() => {
    const last = held.current
    if (!last || items.some((item) => item.key === last.key)) return
    held.current = null
    const cards = list.current?.children
    const next = cards?.length ? cards[Math.min(last.index, cards.length - 1)] : document.getElementById('main')?.querySelector('h1')
    if (next instanceof HTMLElement) claimFocus(next)
  }, [keys])
  // Every read lands with the device list, which always holds the computer this page reads from; until then 没有待处理 would be a guess.
  if (!items.length && !workspace.nodes.length && !workspace.error) return <div className="mx-auto grid w-full max-w-(--page-width) gap-3" aria-busy="true"><Skeleton className="h-14" /><Skeleton className="h-14" /></div>
  if (!items.length) return <EmptyState icon={Inbox} title="没有待处理" action={<Button asChild variant="outline"><Link to="/"><MessageCircle aria-hidden="true" />回到消息</Link></Button>} />
  const action = async (kind: "ignore" | "read") => {
    setBusy(kind)
    await workspace.run(async () => {
      const ids = attention.map((item) => item.notification.notification_id)
      if (kind === "ignore") workspace.ignoreNotifications(ids)
      else await workspace.markNotificationsRead(ids)
      workspace.announce(kind === "ignore" ? "已忽略提醒，仅此设备隐藏。" : "提醒已标记为已读。")
    }, kind === "ignore" ? "忽略待处理提醒" : "标记提醒已读")
    setBusy(null)
  }
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-3">
    {attention.length ? <div className="flex flex-wrap justify-end gap-2">
      <Button type="button" variant="ghost" disabled={busy !== null} aria-busy={busy === "ignore" || undefined} onClick={() => void action("ignore")}><EyeOff aria-hidden="true" />忽略提醒</Button>
      <Button type="button" variant="outline" disabled={busy !== null} aria-busy={busy === "read" || undefined} onClick={() => void action("read")}><CheckCheck aria-hidden="true" />全部已读</Button>
    </div> : null}
    <ul ref={list} className="grid gap-3" onFocus={(event) => {
      const index = [...event.currentTarget.children].findIndex((card) => card.contains(event.target))
      if (items[index]) held.current = { key: items[index].key, index }
    }}>
      {items.map((item) => <li key={item.key} tabIndex={-1} className="min-w-0 outline-none animate-in fade-in-0 slide-in-from-bottom-1 duration-200 ease-out">
        {item.kind === 'recovery' ? <RecoveryPendingCard /> : item.kind === 'rotate' ? <RotateHintCard /> : item.kind === 'write' ? <WriteCard kind={item.write} /> : item.kind === 'join' ? <JoinCard request={item.join} /> : item.kind === 'question' ? <QuestionCard question={item.question} inbox from="inbox" /> : item.kind === 'confirm' ? <ConfirmCard check={item.check} inbox from="inbox" /> : <Link to={item.to} className="flex min-h-14 items-center gap-3 interactive-row rounded-xl border bg-card shadow-xs px-4 py-3">
          <item.icon aria-hidden="true" className="size-5 shrink-0 text-warning" />
          <span className="min-w-0 flex-1 truncate font-medium">{item.title}</span>
          {item.meta ? <span className="max-w-[40%] shrink-0 truncate text-meta text-muted-foreground" title={item.meta}>{item.meta}</span> : null}
          <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        </Link>}
      </li>)}
    </ul>
  </div>
}
