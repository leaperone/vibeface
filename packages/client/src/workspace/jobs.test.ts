import assert from 'node:assert/strict'
import test from 'node:test'
import { parseHistoryPage } from './model.ts'

function managerJob(value: unknown) {
  const page = parseHistoryPage({ conversation_id: 'chat', history_authority: 'owner', section: 'jobs',
    items: [value], next_cursor: null, complete: true }, 'chat', 'jobs')
  if (page.section !== 'jobs') throw new Error('wrong history section')
  return page.items[0]
}

const job = { trigger: 'message:input', scope: { kind: 'main' }, state: 'steered' }

test('steered jobs accept only the protocol outcomes', () => {
  for (const outcome of ['answered', 'unfinished', 'unknown']) {
    assert.equal(managerJob({ ...job, outcome }).outcome, outcome)
  }
  for (const outcome of ['made_up_outcome', '', 1, false, {}, []]) {
    assert.throws(() => managerJob({ ...job, outcome }))
  }
  assert.equal(managerJob(job).outcome, undefined)
  assert.equal(managerJob({ ...job, outcome: null }).outcome, undefined)
})

test('manager and scoped jobs accept the declared observation states and reject unknown states', () => {
  const scoped = { conversation_id: 'chat', trigger: 'message:input', scope: { kind: 'workspace_task', task_id: 'task' },
    sequence: 1, kind: 'human', lane: 'task', source_node: 'node', owner: 'node', message_id: 'input', reply_message_id: 'reply' }
  const readScoped = (state: unknown) => {
    const page = parseHistoryPage({ conversation_id: 'chat', history_authority: 'owner', section: 'scope_jobs',
      items: [{ ...scoped, state }], next_cursor: null, complete: true }, 'chat', 'scope_jobs')
    if (page.section !== 'scope_jobs') throw new Error('wrong history section')
    return page.items[0]
  }
  for (const state of ['queued', 'pending_delivery', 'claimed', 'running', 'steering', 'cancelling', 'publishing',
    'reconciling', 'completed', 'interrupted', 'cancelled', 'steered', 'rejected', 'failed', 'unknown', 'unverified']) {
    assert.equal(managerJob({ ...job, state }).state, state)
    assert.equal(readScoped(state).state, state)
  }
  for (const state of ['made_up_state', '', 1, null, undefined, {}, []]) {
    assert.throws(() => managerJob({ ...job, state }))
    assert.throws(() => readScoped(state))
  }
})
