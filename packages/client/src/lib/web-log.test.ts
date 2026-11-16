import assert from 'node:assert/strict'
import test from 'node:test'
import { Worker } from 'node:worker_threads'
import { MAX_BATCH, MAX_RECORD_BYTES, createReporter, failureFields, fit, pageIds, type WebRecord } from './web-log.ts'

const size = (record: WebRecord) => new TextEncoder().encode(JSON.stringify(record)).length

test('records shrink to what the device accepts', () => {
  const huge = fit({ level: 'error', event: 'web.uncaught', message: '错'.repeat(5000), fields: { stack: 'x'.repeat(9000) } })
  assert.ok(size(huge) <= MAX_RECORD_BYTES)
  assert.equal(size(huge), 6228, 'UTF-8 bytes retain the existing message reduction')
  assert.deepEqual(huge.fields, { truncated: true })
  const small = { level: 'info', event: 'web.action', message: 'ok', fields: { label: '导出' } } as const
  assert.deepEqual(fit(small), small)
})

test('the 7000-byte boundary preserves records and shrinks the first byte over it', () => {
  const exact: WebRecord = { level: 'info', event: 'web.action', message: '错'.repeat(2316) + 'xx' }
  assert.equal(size(exact), 7000)
  assert.equal(fit(exact), exact)
  const above: WebRecord = { level: 'info', event: 'web.action', message: 'x'.repeat(6951) }
  assert.equal(size(above), 7001)
  assert.equal(size(fit(above)), 5613)
})

test('fixed-field overflow rejects or drops within a worker deadline and later records send once', async () => {
  const worker = new Worker(`
    const assert = require('node:assert/strict')
    const { parentPort, workerData } = require('node:worker_threads')
    ;(async () => {
      const { fit, createReporter } = await import(workerData)
      parentPort.postMessage('ready')
      const reporter = createReporter()
      const sent = []
      reporter.connect(async records => { sent.push(...records) })
      for (const key of ['event', 'conversation_id', 'work_id']) {
        const record = { level: 'info', event: 'web.action', message: 'ok', [key]: 'x'.repeat(7001) }
        assert.throws(() => fit(record), { name: 'Error', message: 'Log record exceeds size limit.' })
        assert.doesNotThrow(() => reporter.report(record))
      }
      assert.equal(reporter.pending(), 0)
      const valid = { level: 'info', event: 'web.action', message: 'after overflow' }
      reporter.report(valid)
      await reporter.flush()
      await reporter.flush()
      assert.deepEqual(sent, [valid])
      assert.equal(reporter.pending(), 0)
    })().catch(error => { throw error })
  `, { eval: true, workerData: new URL('./web-log.ts', import.meta.url).href })
  let deadline: ReturnType<typeof setTimeout> | undefined
  try {
    await new Promise<void>((resolve, reject) => {
      deadline = setTimeout(() => reject(new Error('log worker did not start')), 5000)
      worker.on('message', (message: unknown) => {
        if (message !== 'ready') return
        clearTimeout(deadline)
        deadline = setTimeout(() => reject(new Error('log fitting did not terminate')), 1000)
      })
      worker.once('error', reject)
      worker.once('exit', code => code === 0 ? resolve() : reject(new Error(`log worker exited with ${code}`)))
    })
  } finally {
    clearTimeout(deadline)
    await worker.terminate()
  }
})

test('circular fields reject direct fitting, but reporting drops them and sends the next record once', async () => {
  const fields: Record<string, unknown> = {}
  fields.self = fields
  const malformed: WebRecord = { level: 'error', event: 'web.uncaught', message: 'circular', fields }
  assert.throws(() => fit(malformed), TypeError)
  const reporter = createReporter()
  const sent: WebRecord[] = []
  reporter.connect(async records => { sent.push(...records) })
  assert.doesNotThrow(() => reporter.report(malformed))
  assert.equal(reporter.pending(), 0)
  const valid: WebRecord = { level: 'info', event: 'web.action', message: 'after circular' }
  reporter.report(valid)
  await reporter.flush()
  await reporter.flush()
  assert.deepEqual(sent, [valid])
  assert.equal(reporter.pending(), 0)
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
  let attempts = 0
  reporter.connect(async () => { attempts++; throw new Error('offline') })
  reporter.report({ level: 'error', event: 'web.uncaught', message: 'boom' })
  await reporter.flush()
  assert.equal(reporter.pending(), 0)
  await reporter.flush()
  assert.equal(attempts, 1)
})

test('only the newest 100 queued records reach the device', async () => {
  const reporter = createReporter()
  for (let index = 0; index < 105; index++) reporter.report({ level: 'info', event: 'web.action', message: `${index}` })
  assert.equal(reporter.pending(), 100)
  const sent: WebRecord[] = []
  const batches: number[] = []
  reporter.connect(async records => { sent.push(...records); batches.push(records.length) })
  while (reporter.pending()) await reporter.flush()
  assert.deepEqual(batches, [20, 20, 20, 20, 20])
  assert.equal(sent.length, 100)
  assert.equal(sent[0]?.message, '5')
  assert.equal(sent[99]?.message, '104')
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
