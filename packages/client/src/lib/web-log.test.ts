import assert from 'node:assert/strict'
import test from 'node:test'
import { Worker } from 'node:worker_threads'
import { MAX_BATCH, MAX_RECORD_BYTES, createBufferedReporter, createReporter, failureFields, fit, pageIds, type WebRecord } from './web-log.ts'

const size = (record: WebRecord) => new TextEncoder().encode(JSON.stringify(record)).length

test('records shrink to what the device accepts', () => {
  const huge = fit({ level: 'error', event: 'web.uncaught', message: '错'.repeat(5000), fields: { stack: 'x'.repeat(9000) } })
  assert.ok(size(huge) <= MAX_RECORD_BYTES)
  assert.ok(size(huge) > MAX_RECORD_BYTES * 0.8, 'UTF-8 bytes shrink only as far as needed')
  assert.deepEqual(Object.keys(huge.fields ?? {}), ['truncated', 'stack'], 'the start of the stack stays')
  assert.match(String(huge.fields?.stack), /^x+…$/)
  assert.ok(huge.message.endsWith('…'))
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

const memory = () => {
  const saved = new Map<string, string>()
  return { getItem: (key: string) => saved.get(key) ?? null, setItem: (key: string, value: string) => { saved.set(key, value) } }
}

test('the phone keeps numbered records until the device accepts them, across restarts and unknown results', async () => {
  const storage = memory()
  let clock = 1_000
  const timers: Array<() => void> = []
  const options = { now: () => clock, later: (run: () => void) => { timers.push(run) } }
  const first = createBufferedReporter(storage, options)
  for (let index = 0; index < 3; index++) first.report({ level: 'info', event: 'mobile.connection', message: `${index}` })
  assert.equal(createBufferedReporter(storage, options).pending(), 0, 'a burst is saved once, a moment later')
  timers.splice(0).forEach(run => run())
  const reopened = createBufferedReporter(storage, options)
  assert.equal(reopened.pending(), 3, 'a restart keeps unsent records')
  const sent: unknown[][] = []
  let answer: 'unknown' | 'limited' | 'ok' = 'unknown'
  reopened.connect(async (records, source) => {
    assert.equal(source, 'mobile')
    sent.push(records.map(record => [record.fields?.client_seq, record.fields?.client_ts_ms]))
    if (answer === 'unknown') throw new Error('连接超时。')
    if (answer === 'limited') throw Object.assign(new Error('too many log records; try again in a minute'), { responded: true, code: 'log_rate_limited' })
  })
  assert.deepEqual(await reopened.send(), { sent: 0, left: 3, connected: true }, 'an unknown result keeps them')
  answer = 'limited'
  assert.deepEqual(await reopened.send(), { sent: 0, left: 3, connected: true })
  assert.deepEqual(await reopened.send(), { sent: 0, left: 3, connected: true }, 'a refused budget waits a minute')
  answer = 'ok'
  clock += 60_000
  assert.deepEqual(await reopened.send(), { sent: 3, left: 0, connected: true })
  const numbered = [[1000, 1000], [1001, 1000], [1002, 1000]]
  assert.deepEqual(sent, [numbered, numbered, numbered], 'the same numbers go again')
  timers.splice(0).forEach(run => run())
  assert.equal(createBufferedReporter(storage, options).pending(), 0, 'accepted records leave storage')
})

test('the phone keeps a batch the device answered with anything but an invalid record', async () => {
  let clock = 0
  const reporter = createBufferedReporter(memory(), { now: () => clock, later: () => {} })
  reporter.report({ level: 'info', event: 'mobile.connection', message: 'a' })
  let refusal: { message: string; code?: string } = { message: 'the Node is updating; try again shortly' }
  reporter.connect(async () => { throw Object.assign(new Error(refusal.message), { responded: true, code: refusal.code }) })
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true }, 'maintenance keeps it')
  refusal = { message: 'this device is not a member' }
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true }, 'a lost permission keeps it')
  refusal = { message: 'too many log records; try again in a minute' }
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true }, 'the words alone are not a rate limit')
  refusal = { message: 'send between 1 and 20 log records at a time', code: 'log_rejected' }
  assert.deepEqual(await reporter.send(), { sent: 0, left: 0, connected: true }, 'an invalid batch is dropped')
})

test('a rate-limited batch waits a minute before it goes again', async () => {
  let clock = 0
  const waits: number[] = []
  const reporter = createBufferedReporter(memory(), { now: () => clock, later: (_run, ms) => { waits.push(ms) } })
  reporter.report({ level: 'info', event: 'mobile.connection', message: 'a' })
  let calls = 0
  reporter.connect(async () => {
    calls++
    if (clock < 60_000) throw Object.assign(new Error('slow down'), { responded: true, code: 'log_rate_limited' })
  })
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true })
  assert.ok(waits.includes(60_000), 'the next try is a minute away')
  clock = 59_999
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true }, 'not sent again within the minute')
  assert.equal(calls, 1)
  clock = 60_000
  assert.deepEqual(await reporter.send(), { sent: 1, left: 0, connected: true })
})

test('a skipped batch above the phone numbering is numbered again and resent; one below it was written', async () => {
  const storage = memory()
  const reporter = createBufferedReporter(storage, { now: () => 10, later: () => {} })
  for (const message of ['a', 'b']) reporter.report({ level: 'info', event: 'mobile.action', message })
  const sent: unknown[] = []
  let written = 500
  reporter.connect(async records => {
    const seqs = records.map(record => record.fields?.client_seq as number)
    sent.push(seqs)
    if (seqs[0]! <= written) return { accepted: 0, skipped: seqs.length, max_seq: written }
    written = seqs[seqs.length - 1]!
    return { accepted: seqs.length }
  })
  assert.deepEqual(await reporter.send(), { sent: 2, left: 0, connected: true })
  assert.deepEqual(sent, [[10, 11], [501, 502]], 'new numbers above what the device wrote')
  written = 600
  sent.length = 0
  reporter.report({ level: 'info', event: 'mobile.action', message: 'c' })
  assert.deepEqual(await reporter.send(), { sent: 1, left: 0, connected: true })
  assert.deepEqual(sent, [[503], [601]], 'the next number continues above the last renumbering')
  let lost = true
  reporter.connect(async records => {
    const seq = records[records.length - 1]!.fields?.client_seq
    // The device wrote it, but the answer was lost.
    if (lost) { lost = false; throw new Error('连接超时。') }
    return { accepted: 0, skipped: records.length, max_seq: seq }
  })
  reporter.report({ level: 'info', event: 'mobile.action', message: 'd' })
  assert.deepEqual(await reporter.send(), { sent: 0, left: 1, connected: true })
  assert.deepEqual(await reporter.send(), { sent: 1, left: 0, connected: true }, 'a resend the device already wrote leaves')
})

test('a partly skipped batch numbered below the device is renumbered; what the device wrote is not sent again', async () => {
  const reporter = createBufferedReporter(memory(), { now: () => 10, later: () => {} })
  for (const message of ['a', 'b', 'c', 'd', 'e']) reporter.report({ level: 'info', event: 'mobile.action', message })
  // The device already wrote up to 12 from this phone, before its numbering went back.
  let last = 12
  const written: string[] = []
  const batches: number[][] = []
  reporter.connect(async records => {
    const seqs = records.map(record => record.fields?.client_seq as number)
    batches.push(seqs)
    const fresh = records.filter(record => (record.fields?.client_seq as number) > last)
    written.push(...fresh.map(record => record.message))
    last = Math.max(last, ...seqs)
    const skipped = records.length - fresh.length
    return skipped ? { accepted: fresh.length, skipped, max_seq: last } : { accepted: fresh.length }
  })
  assert.deepEqual(await reporter.send(), { sent: 5, left: 0, connected: true })
  assert.deepEqual(batches, [[10, 11, 12, 13, 14], [15, 16, 17]])
  assert.deepEqual(written, ['d', 'e', 'a', 'b', 'c'], 'every record is written once')
})

test('a fatal record with a long stack is saved at once and keeps fatal', () => {
  const storage = memory()
  const reporter = createBufferedReporter(storage, { now: () => 1, later: () => {} })
  reporter.report({ level: 'error', event: 'mobile.uncaught', message: 'TypeError: boom', fields: { fatal: true, stack: 'at f\n'.repeat(3000) } })
  const saved = createBufferedReporter(storage, { now: () => 1, later: () => {} })
  assert.equal(saved.pending(), 1)
  const stored = JSON.parse(storage.getItem('vibeface.log-buffer')!).records[0].fields
  assert.equal(stored.fatal, true)
  assert.equal(stored.truncated, true)
  assert.ok(String(stored.stack).startsWith('at f\nat f'))
})

test('saving now writes what the save delay would', () => {
  const storage = memory()
  const reporter = createBufferedReporter(storage, { now: () => 1, later: () => {} })
  reporter.report({ level: 'info', event: 'mobile.action', message: 'tap' })
  assert.equal(createBufferedReporter(storage, { now: () => 1, later: () => {} }).pending(), 0)
  reporter.save()
  assert.equal(createBufferedReporter(storage, { now: () => 1, later: () => {} }).pending(), 1)
})

test('a fatal error is saved at once; other records wait for the short save delay', () => {
  const storage = memory()
  const reporter = createBufferedReporter(storage, { now: () => 1, later: () => {} })
  reporter.report({ level: 'info', event: 'mobile.action', message: 'tap' })
  assert.equal(createBufferedReporter(storage, { now: () => 1, later: () => {} }).pending(), 0)
  reporter.report({ level: 'error', event: 'mobile.uncaught', message: 'boom', fields: { fatal: true } })
  assert.equal(createBufferedReporter(storage, { now: () => 1, later: () => {} }).pending(), 2)
})

test('the phone keeps the newest 300 and stays within 120 records a minute', async () => {
  let clock = 0
  const reporter = createBufferedReporter(memory(), { now: () => clock, later: () => {} })
  for (let index = 0; index < 310; index++) reporter.report({ level: 'info', event: 'mobile.action', message: `${index}` })
  assert.equal(reporter.pending(), 300)
  assert.deepEqual(await reporter.send(), { sent: 0, left: 300, connected: false })
  let count = 0
  reporter.connect(async records => { count += records.length })
  assert.deepEqual(await reporter.send(), { sent: 120, left: 180, connected: true })
  clock += 60_000
  assert.deepEqual(await reporter.send(), { sent: 120, left: 60, connected: true })
  assert.equal(count, 240)
})
