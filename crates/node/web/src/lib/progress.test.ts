import assert from 'node:assert/strict'
import test from 'node:test'
import { mergeProgress, parseChanged } from './progress.ts'

const manager = (sequence: number, progress: unknown, trigger = 'turn-a') => parseChanged({
  type: 'manager_progress', conversation_id: 'chat-a', trigger, source_node: 'scheduler-a', sequence, progress,
})

test('only the normalized progress contract crosses the changed boundary', () => {
  const event = manager(1, { type: 'text_snapshot', text: '', item_id: 'reply-a' })
  assert.equal(event.type, 'manager_progress')
  for (const value of [
    null, [], { type: 'unknown' },
    { type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', progress: { provider: 'arbitrary-json' } },
    { type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', sequence: '1', progress: { type: 'text_delta', text: 'text' } },
    { type: 'execution_progress', conversation_id: 'chat-a', session_id: {}, progress: { type: 'status', text: 'running' } },
    { type: 'execution_progress', conversation_id: 'chat-a', session_id: 'session-a', progress: { type: 'tool', text: { secret: 'value' } } },
  ]) assert.deepEqual(parseChanged(value), { type: 'invalidate' })
})

test('text snapshots replace their item while deltas append once in sequence', () => {
  const first = mergeProgress([], manager(1, { type: 'text_snapshot', item_id: 'reply-a', text: '你好' }))
  const second = mergeProgress(first, manager(2, { type: 'text_delta', item_id: 'reply-a', text: '，继续' }))
  assert.equal(first[0].text, '你好')
  assert.equal(second[0].text, '你好，继续')
  assert.equal(mergeProgress(second, manager(2, { type: 'text_delta', item_id: 'reply-a', text: '重复' })), second)
  assert.equal(mergeProgress(second, manager(1, { type: 'text_snapshot', item_id: 'reply-a', text: '过期' })), second)
  const third = mergeProgress(second, manager(3, { type: 'text_snapshot', item_id: 'reply-b', text: '第二段' }))
  const fourth = mergeProgress(third, manager(4, { type: 'text_snapshot', item_id: 'reply-a', text: '已修订' }))
  assert.deepEqual(fourth[0].items, [{ item_id: 'reply-a', text: '已修订' }, { item_id: 'reply-b', text: '第二段' }])
  assert.equal(fourth[0].text, '已修订\n\n第二段')
  assert.equal(fourth[0].needsRefresh, false)
})

test('each invocation and execution operation keeps its own sequence and text', () => {
  let state = mergeProgress([], manager(1, { type: 'text_delta', text: '第一轮' }))
  state = mergeProgress(state, manager(1, { type: 'text_delta', text: '第二轮' }, 'turn-b'))
  for (const operation_id of ['operation-a', 'operation-b']) {
    state = mergeProgress(state, parseChanged({ type: 'execution_progress', conversation_id: 'chat-a', session_id: 'session-a', operation_id, task_id: 'task-a', target_node: 'executor-a', sequence: 1, progress: { type: 'text_snapshot', text: operation_id } }))
  }
  assert.equal(state.length, 4)
  assert.deepEqual(state.map(item => item.text), ['第一轮', '第二轮', 'operation-a', 'operation-b'])
  assert.deepEqual(state.map(item => item.kind), ['manager', 'manager', 'execution', 'execution'])
  assert.equal(state[2].task_id, 'task-a')
  assert.equal(state[2].operation_id, 'operation-a')
})

test('sequence gaps request a snapshot and status and tool descriptions remain visible', () => {
  let state = mergeProgress([], manager(1, { type: 'text_delta', text: '实时片段' }))
  state = mergeProgress(state, manager(3, { type: 'status', text: '等待设备' }))
  assert.equal(state[0].needsRefresh, true)
  assert.equal(state[0].status, '等待设备')
  state = mergeProgress(state, manager(4, { type: 'tool', name: '读取文件' }))
  assert.equal(state[0].status, '读取文件')
  assert.equal(state[0].text, '实时片段')
  assert.equal(mergeProgress(state, { type: 'invalidate' }), state)
  assert.equal(mergeProgress([], manager(5, { type: 'text_snapshot', text: '错过开头' }))[0].needsRefresh, true)
})

test('legacy events without a sequence still preserve snapshots and deltas', () => {
  const event = (type: string, text: string) => parseChanged({ type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', progress: { type, text } })
  const state = mergeProgress(mergeProgress([], event('text_snapshot', '开头')), event('text_delta', '结尾'))
  assert.equal(state[0].text, '开头结尾')
  assert.equal(state[0].sequence, undefined)
})
test('canonical manager status and tool events remain typed progress', () => {
  const samples = [
    { type: 'status', status: 'running' },
    { type: 'status', status: 'finishing' },
    { type: 'status', status: 'completed' },
    { type: 'tool', item_id: 't1', status: 'completed', name: 'command_execution' },
  ]
  let state: ReturnType<typeof mergeProgress> = []
  for (const [index, progress] of samples.entries()) {
    const event = manager(index + 1, progress)
    assert.equal(event.type, 'manager_progress')
    state = mergeProgress(state, event)
  }
  assert.equal(state[0].sequence, samples.length)
  assert.equal(state[0].needsRefresh, false)
  assert.equal(state[0].status, 'completed')
})
