import { createFileRoute, Link } from '@tanstack/react-router'
import { ChevronRight, Inbox } from 'lucide-react'
import { EmptyState } from '@/components/app/kit'
import { useInbox } from '@/features/workspace/inbox'
import { QuestionCard } from '@/features/workspace/question-card'
import { JoinCard } from '@/features/devices/join'
import { WriteCard } from '@/features/workspace/write-card'
import { ConfirmCard } from '@/features/workspace/task-activity'
import { RecoveryPendingCard, RotateHintCard } from '@/features/identity/recovery-controls'

export const Route = createFileRoute('/_app/inbox')({ component: InboxPage })

function InboxPage() {
  const items = useInbox()
  if (!items.length) return <EmptyState icon={Inbox} title="没有待处理" />
  return <ul className="mx-auto grid w-full max-w-(--page-width) gap-3">
    {items.map((item) => <li key={item.key} className="min-w-0 animate-in fade-in-0 slide-in-from-bottom-1 duration-200 ease-out">
      {item.kind === 'recovery' ? <RecoveryPendingCard /> : item.kind === 'rotate' ? <RotateHintCard /> : item.write ? <WriteCard kind={item.write} /> : item.join ? <JoinCard request={item.join} /> : item.question ? <QuestionCard question={item.question} inbox /> : item.check ? <ConfirmCard check={item.check} inbox /> : <Link to={item.to} className="flex min-h-14 items-center gap-3 interactive-row rounded-xl border bg-card shadow-xs px-4 py-3">
        <item.icon aria-hidden="true" className="size-5 shrink-0 text-warning" />
        <span className="min-w-0 flex-1 truncate font-medium">{item.title}</span>
        {item.meta ? <span className="max-w-[40%] shrink-0 truncate text-meta text-muted-foreground" title={item.meta}>{item.meta}</span> : null}
        <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      </Link>}
    </li>)}
  </ul>
}
