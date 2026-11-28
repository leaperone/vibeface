import assert from 'node:assert/strict'
import test from 'node:test'
import { routineState } from './companion-recap.ts'
import { completeHistory } from '../workspace/history-fixture.ts'
import { parseConversation } from '../workspace/model.ts'

const snapshot = (extra: Record<string, unknown> = {}) => parseConversation({
  conversation: { conversation_id: 'chat', owner: 'node', owner_epoch: 1, title: '' }, history_authority: 'authority', history: completeHistory(),
  messages: [], works: [], deliveries: [], checks: [], local_node: 'node', changes: [], overwritten: [],
  scheduler: { jobs: [], scope_jobs: [], unanswered: [] }, questions: [], sessions: [], sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 },
  bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, ...extra,
}, 'chat')

test('routine state reads its latest scope job; offline execution stays uncertain', () => {
  const data = snapshot()
  const job = { conversation_id: 'chat', trigger: 'schedule:daily:1', scope: { kind: 'routine', trigger: 'schedule:daily:1' }, state: 'running', sequence: 1, kind: 'schedule', lane: 'schedule_routine', source_node: 'node', owner: 'node', message_id: null, reply_message_id: 'future' } satisfies ReturnType<typeof snapshot>['scopeJobs'][number]
  data.scopeJobs = [job, job]
  assert.equal(routineState(data, job.trigger, false), '状态待确认')
  data.scopeJobs = [{ ...job, state: 'completed' }]
  assert.equal(routineState(data, job.trigger, true), '本轮已结束')
  assert.equal(routineState(data, 'schedule:other:1', true), null)
})

