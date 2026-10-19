import assert from 'node:assert/strict'
import test from 'node:test'
import { mergeProgress, parseChanged } from './progress.ts'

const manager = (sequence: number, progress: unknown, trigger = 'turn-a', invocation_id?: string) => parseChanged({
  type: 'manager_progress', conversation_id: 'chat-a', trigger, source_node: 'scheduler-a', invocation_id, sequence, progress,
})

test('only the normalized progress contract crosses the changed boundary', () => {
  const event = manager(1, { type: 'text_snapshot', text: '', item_id: 'reply-a' })
  assert.equal(event.type, 'manager_progress')
  for (const value of [
    null, [], { type: 'unknown' },
    { type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', progress: { provider: 'arbitrary-json' } },
    { type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', sequence: '1', progress: { type: 'text_delta', text: 'text' } },
    { type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', invocation_id: {}, progress: { type: 'text_delta', text: 'text' } },
    { type: 'execution_progress', conversation_id: 'chat-a', session_id: 'removed', progress: { type: 'status', text: 'running' } },
    { type: 'native_progress', conversation_id: 'chat-a', work_id: {}, thread_id: 'thread-a', target_node: 'node-a', progress: { type: 'status', text: 'running' } },
    { type: 'native_progress', conversation_id: 'chat-a', work_id: 'work-a', thread_id: 'thread-a', target_node: 'node-a', progress: { type: 'tool', text: { secret: 'value' } } },
    { type: 'native_progress', conversation_id: 'chat-a', work_id: 'work-a', thread_id: 'thread-a', progress: { type: 'status', text: 'running' } },
  ]) assert.deepEqual(parseChanged(value), { type: 'invalidate' })
})

test('the final invocation replaces initial progress and accepts its restarted sequence', () => {
  let initial = mergeProgress([], manager(1, { type: 'text_snapshot', text: '初始片段' }, 'turn-a', 'attempt'))
  initial = mergeProgress(initial, manager(2, { type: 'status', text: '正在处理' }, 'turn-a', 'attempt'))
  initial = mergeProgress(initial, manager(3, { type: 'text_delta', text: '，继续' }, 'turn-a', 'attempt'))
  const final = manager(1, { type: 'status', text: '正在汇报' }, 'turn-a', 'attempt:final')
  assert.equal(final.type === 'manager_progress' && final.invocation_id, 'attempt:final')
  let state = mergeProgress(initial, final)
  assert.equal(state.length, 1)
  assert.equal(state[0].kind === 'manager' && state[0].invocation_id, 'attempt:final')
  assert.equal(state[0].key, initial[0].key)
  assert.equal(state[0].sequence, 1)
  assert.equal(state[0].text, '')
  assert.deepEqual(state[0].items, [])
  assert.equal(state[0].status, '正在汇报')
  assert.equal(state[0].needsRefresh, false)
  state = mergeProgress(state, manager(2, { type: 'text_delta', text: '最终' }, 'turn-a', 'attempt:final'))
  state = mergeProgress(state, manager(3, { type: 'text_delta', text: '报告' }, 'turn-a', 'attempt:final'))
  assert.equal(state.length, 1)
  assert.equal(state[0].text, '最终报告')
  assert.equal(mergeProgress(state, manager(2, { type: 'text_delta', text: '重复' }, 'turn-a', 'attempt:final')), state)
  assert.equal(initial[0].text, '初始片段，继续')
})

test('a new attempt resets its source and trigger slot while unrelated progress remains', () => {
  let state = mergeProgress([], manager(4, { type: 'status', text: '旧调用状态' }, 'turn-a', 'attempt-a'))
  state = mergeProgress(state, manager(1, { type: 'text_snapshot', text: '其他任务' }, 'turn-b', 'other-attempt'))
  const previous = state
  state = mergeProgress(state, manager(1, { type: 'text_delta', text: '重试片段' }, 'turn-a', 'attempt-b'))
  assert.equal(state.length, 2)
  assert.equal(state[0].kind === 'manager' && state[0].invocation_id, 'attempt-b')
  assert.equal(state[0].text, '重试片段')
  assert.equal(state[0].status, undefined)
  assert.equal(state[0].sequence, 1)
  assert.equal(state[0].needsRefresh, false)
  assert.equal(state[1], previous[1])
  state = mergeProgress(state, parseChanged({ type: 'manager_progress', conversation_id: 'chat-a', trigger: 'turn-a', source_node: 'scheduler-b', invocation_id: 'attempt-b', sequence: 1, progress: { type: 'text_snapshot', text: '另一台设备' } }))
  assert.equal(state.length, 3)
  assert.deepEqual(state.map(item => item.text), ['重试片段', '其他任务', '另一台设备'])
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

test('each invocation and native turn keeps its own sequence and task association', () => {
  let state = mergeProgress([], manager(1, { type: 'text_delta', text: '第一轮' }))
  state = mergeProgress(state, manager(1, { type: 'text_delta', text: '第二轮' }, 'turn-b'))
  for (const turn_id of ['turn-a', 'turn-b']) {
    state = mergeProgress(state, parseChanged({ type: 'native_progress', conversation_id: 'chat-a', work_id: 'work-a', thread_id: 'thread-a', turn_id, task_ref: 'task-a', target_node: 'node-a', sequence: 1, progress: { type: 'text_snapshot', text: turn_id } }))
  }
  assert.equal(state.length, 4)
  assert.deepEqual(state.map(item => item.text), ['第一轮', '第二轮', 'turn-a', 'turn-b'])
  assert.deepEqual(state.map(item => item.kind), ['manager', 'manager', 'native', 'native'])
  assert.equal(state[2].kind === 'native' && state[2].task_ref, 'task-a')
  assert.equal(state[2].kind === 'native' && state[2].work_id, 'work-a')
  const next = mergeProgress(state, parseChanged({ type: 'native_progress', conversation_id: 'chat-a', work_id: 'work-a', thread_id: 'thread-a', turn_id: 'turn-a', task_ref: null, target_node: 'node-b', sequence: 1, progress: { type: 'text_snapshot', text: 'other device' } }))
  assert.equal(next.length, 5)
  assert.equal(next[4].text, 'other device')
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
