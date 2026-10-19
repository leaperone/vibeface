import { createFileRoute, Link } from '@tanstack/react-router'
import { ChevronRight, Inbox } from 'lucide-react'
import { EmptyState } from '@/components/app/kit'
import { useInbox } from '@/features/workspace/inbox'

export const Route = createFileRoute('/_app/inbox')({ component: InboxPage })

function InboxPage() {
  const items = useInbox()
  if (!items.length) return <EmptyState icon={Inbox} title="没有待处理" description="需要你决定的事会出现在这里" />
  return <ul className="mx-auto grid max-w-3xl gap-3">
    {items.map((item) => <li key={item.key}>
      <Link to={item.to} className="flex min-h-14 items-center gap-3 rounded-xl border bg-card px-4 py-3 hover:bg-muted/50">
        <item.icon aria-hidden="true" className="size-5 shrink-0 text-warning" />
        <span className="min-w-0 flex-1 truncate font-medium">{item.title}</span>
        {item.meta ? <span className="shrink-0 truncate text-meta text-muted-foreground">{item.meta}</span> : null}
        <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      </Link>
    </li>)}
  </ul>
}
