import assert from 'node:assert/strict'
import test from 'node:test'
import { MAX_BATCH, MAX_RECORD_BYTES, createReporter, failureFields, fit, pageIds, type WebRecord } from './web-log.ts'

const size = (record: WebRecord) => new TextEncoder().encode(JSON.stringify(record)).length

test('records shrink to what the device accepts', () => {
  const huge = fit({ level: 'error', event: 'web.uncaught', message: '错'.repeat(5000), fields: { stack: 'x'.repeat(9000) } })
  assert.ok(size(huge) <= MAX_RECORD_BYTES)
  assert.deepEqual(huge.fields, { truncated: true })
  const small = { level: 'info', event: 'web.action', message: 'ok', fields: { label: '导出' } } as const
  assert.deepEqual(fit(small), small)
})

test('records wait for a device and go out in bounded batches', async () => {
  const reporter = createReporter()
  const sent: WebRecord[][] = []
  for (let index = 0; index < MAX_BATCH + 5; index++) reporter.report({ level: 'info', event: 'web.action', message: `${index}` })
  assert.equal(reporter.pending(), MAX_BATCH + 5)
  const detach = reporter.connect(async (records) => { sent.push(records) })
  await reporter.flush()
  await reporter.flush()
  assert.deepEqual(sent.map((batch) => batch.length), [MAX_BATCH, 5])
  detach()
  reporter.report({ level: 'info', event: 'web.action', message: 'later' })
  await reporter.flush()
  assert.equal(sent.length, 2, 'nothing is sent without a device')
})

test('a failing device drops the batch instead of throwing', async () => {
  const reporter = createReporter()
  reporter.connect(async () => { throw new Error('offline') })
  reporter.report({ level: 'error', event: 'web.uncaught', message: 'boom' })
  await reporter.flush()
  assert.equal(reporter.pending(), 0)
})

test('a failed request keeps the device code and its own words beside the worded message', () => {
  const refused = Object.assign(new Error('work not found'), { responded: true, code: 'codex_missing' })
  assert.deepEqual(failureFields(refused, '找不到这个任务，可能还在同步。'), { code: 'codex_missing', cause: 'work not found' })
  assert.deepEqual(failureFields(new Error('连接已断开。'), '连接已断开。'), {}, 'words already shown are not kept twice')
  assert.deepEqual(failureFields(Object.assign(new Error('x'.repeat(900)), { code: 7 }), '这次操作没有成功。再试一次。'), { cause: 'x'.repeat(500) })
  assert.deepEqual(failureFields('offline', '这次操作没有成功。再试一次。'), { cause: 'offline' })
})

test('page ids come from the conversation query and the task path', () => {
  assert.deepEqual(pageIds({ pathname: '/b/bot-1/work/w-9', search: '?conversation=c-1' }), { conversation_id: 'c-1', work_id: 'w-9' })
  assert.deepEqual(pageIds({ pathname: '/settings', search: '?conversation=../x' }), { conversation_id: undefined, work_id: undefined })
  assert.deepEqual(pageIds({ pathname: '/b/bot-1/work/%zz', search: '' }), { conversation_id: undefined, work_id: undefined })
})
