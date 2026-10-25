import { createFileRoute } from '@tanstack/react-router'
import { TaskDetail } from '@/features/workspace/task-detail'

export const Route = createFileRoute('/_app/b/$botId/work/$workId')({
  validateSearch: (search: Record<string, unknown>): { from?: 'inbox' } => search.from === 'inbox' ? { from: 'inbox' } : {},
  component: WorkRoute,
})

function WorkRoute() {
  const { botId, workId } = Route.useParams()
  const { from } = Route.useSearch()
  return <TaskDetail key={workId} botId={botId} workId={workId} from={from} />
}
