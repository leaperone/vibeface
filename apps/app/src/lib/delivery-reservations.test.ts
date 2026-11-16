import assert from 'node:assert/strict'
import test from 'node:test'
import type { TestContext } from 'node:test'
import { Worker } from 'node:worker_threads'
import { createDeliveryClient } from '@vibeface/client/delivery/client.ts'
import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import { nativeDeliveryReservations } from '../../../mobile/src/platform/delivery-reservations.ts'
import { completed, connection, content, draft, file, fingerprint, intentKey, oldId, responseLost, status, wireBytes } from './delivery-reservations.fixture.ts'

// These workers exercise Node 24's real process-wide LockManager, not browser
// localStorage scheduling, native MMKV across runtimes, Core orders or payment.
type ActorOptions = { restart?: boolean; network?: string; name?: string }
type Call = { actor: number; params: Record<string, unknown> }
type Message =
  | { op: 'ready'; key: string }
  | { op: 'get'; key: string }
  | { op: 'set'; key: string; value: string }
  | { op: 'rpc'; method: string; params: Record<string, unknown> }
  | { op: 'done'; error: string | null }

async function actors(t: TestContext, records: Map<string, string>, options: ActorOptions[], holdReads = false) {
  const ready: { worker: Worker; key: string }[] = [], finds: Worker[] = [], reads: { key: string; value: string | null }[] = []
  const calls: Call[] = [], writes: { key: string; value: string }[] = [], errors: (string | null)[] = []
  const heldReads: (() => void)[] = [], heldPrepares: Worker[] = []
  const original = completed(await sha256Hex(content))
  const workers: Worker[] = []
  t.after(async () => { await Promise.all(workers.map(worker => worker.terminate())) })
  await Promise.all(options.map((option, actor) => new Promise<void>((resolve, reject) => {
    const buffer = new SharedArrayBuffer(16384)
    const worker = new Worker(new URL('./delivery-reservations.worker.mjs', import.meta.url), { workerData: { buffer, ...option } })
    workers.push(worker)
    let done = false
    worker.on('error', reject)
    worker.on('exit', code => code === 0 && done ? resolve() : reject(new Error(`worker ${actor} exited ${code} before completion`)))
    const reply = (value: string | null) => {
      const bytes = new TextEncoder().encode(JSON.stringify(value))
      new Uint8Array(buffer, 8).set(bytes)
      const sync = new Int32Array(buffer, 0, 2)
      Atomics.store(sync, 1, bytes.length); Atomics.store(sync, 0, 1); Atomics.notify(sync, 0)
    }
    worker.on('message', (message: Message) => {
      try {
        switch (message.op) {
          case 'ready':
            ready.push({ worker, key: message.key })
            if (ready.length === options.length) for (const item of ready) item.worker.postMessage({ op: 'go' })
            break
          case 'get': {
            const value = records.get(message.key) ?? null
            reads.push({ key: message.key, value })
            if (holdReads && reads.length <= options.length) {
              heldReads.push(() => reply(value))
              if (heldReads.length === options.length) for (const release of heldReads) release()
            } else reply(value)
            break
          }
          case 'set':
            records.set(message.key, message.value); writes.push({ key: message.key, value: message.value }); reply(null)
            break
          case 'rpc':
            assert.equal(message.method, 'delivery')
            calls.push({ actor, params: message.params })
            if (message.params.op === 'find_intent') {
              assert.equal(message.params.intent_id, oldId)
              finds.push(worker)
              // Both actors must validate the same original before either replaces it.
              if (finds.length === options.length) for (const finder of finds) finder.postMessage({ op: 'rpc-result', value: { order: original } })
            } else {
              assert.equal(message.params.op, 'prepare', 'reservation never implicitly confirms')
              assert.equal(message.params.content_base64, wireBytes)
              heldPrepares.push(worker)
              // On ordinary sends, keep the first RPC pending until the next
              // actor submits. It must acquire the same reservation meanwhile.
              if (option.restart || heldPrepares.length === options.length) for (const preparing of heldPrepares.splice(0)) preparing.postMessage({ op: 'rpc-result', error: responseLost })
            }
            break
          case 'done': done = true; errors[actor] = message.error; break
        }
      } catch (error) { reject(error) }
    })
  })))
  return { reads, writes, calls, errors, keys: ready.map(item => item.key) }
}

test('actual Web adapter rejects unsupported/acquisition failures and releases after a callback throws', { timeout: 10000 }, async t => {
  const worker = new Worker(new URL('./delivery-reservations.worker.mjs', import.meta.url), { workerData: { adapter: true } })
  t.after(async () => { await worker.terminate() })
  await new Promise<void>((resolve, reject) => {
    worker.on('error', reject)
    worker.on('exit', code => code === 0 ? resolve() : reject(new Error(`adapter worker exited ${code}`)))
  })
})

test('two real Web workers reserve one immutable ID, reuse it after response loss and release before prepare returns', { timeout: 10000 }, async t => {
  const records = new Map<string, string>()
  const first = await actors(t, records, [{}, {}])
  assert.deepEqual(first.keys, [intentKey, intentKey])
  assert.equal(first.reads.length, 2)
  assert.equal(first.reads.filter(read => read.value === null).length, 1)
  const saved = records.get(intentKey)
  assert.ok(saved)
  const id: unknown = JSON.parse(saved).intent_id
  assert.deepEqual(first.calls.map(call => call.params.intent_id), [id, id])
  assert.deepEqual(first.errors, [responseLost, responseLost])
  assert.equal(first.writes.filter(write => write.key === intentKey).length, 2)
  assert.ok(first.writes.filter(write => write.key === intentKey).every(write => write.value === saved))
  const retry = await actors(t, records, [{}, {}])
  assert.deepEqual(retry.calls.map(call => call.params.intent_id), [id, id])
  assert.equal(records.get(intentKey), saved)
  assert.deepEqual(retry.errors, [responseLost, responseLost])
})

test('two explicit restarts of the same old ID permit one replacement and reject the loser without overwriting it', { timeout: 10000 }, async t => {
  const records = new Map([[intentKey, JSON.stringify({ intent_id: oldId, fingerprint })]])
  const result = await actors(t, records, [{ restart: true }, { restart: true }])
  assert.equal(result.calls.filter(call => call.params.op === 'find_intent').length, 2)
  const prepares = result.calls.filter(call => call.params.op === 'prepare')
  assert.equal(prepares.length, 1)
  assert.notEqual(prepares[0].params.intent_id, oldId)
  const saved = records.get(intentKey)
  assert.ok(saved)
  assert.equal(JSON.parse(saved).intent_id, prepares[0].params.intent_id)
  assert.equal(result.writes.filter(write => write.key === intentKey).length, 1)
  assert.equal(result.errors.filter(error => error === responseLost).length, 1)
  assert.equal(result.errors.filter(error => error?.includes('已在另一处更新')).length, 1)
  const retry = await actors(t, records, [{}, {}])
  assert.ok(retry.calls.every(call => call.params.intent_id === prepares[0].params.intent_id))
  assert.equal(records.get(intentKey), saved)
})

test('different fingerprints and namespaces can reserve while another reservation callback is blocked', { timeout: 10000 }, async t => {
  for (const second of [{ name: 'other.txt' }, { network: 'other-network' }]) {
    const records = new Map<string, string>()
    const result = await actors(t, records, [{}, second], true)
    assert.notEqual(result.keys[0], result.keys[1])
    assert.equal(result.calls.length, 2)
    assert.equal(new Set(result.calls.map(call => call.params.intent_id)).size, 2)
    assert.equal([...records.keys()].filter(key => key.includes('.intent.')).length, 2)
    assert.deepEqual(result.errors, [responseLost, responseLost])
  }
})

test('the actual native adapter immediately runs a synchronous transition and two clients in one runtime share one ID', async () => {
  let entered = false
  const value = { intent_id: oldId, fingerprint }
  const reservation = nativeDeliveryReservations(intentKey, () => { entered = true; return value })
  assert.equal(entered, true, 'no await or queue before the MMKV transition')
  assert.equal(await reservation, value)
  const records = new Map<string, string>(), calls: Record<string, unknown>[] = []
  const storage = { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => { records.set(key, value) } }
  const make = () => createDeliveryClient(connection(async (_method, params = {}) => { calls.push(params); throw new Error(responseLost) }), storage, nativeDeliveryReservations)
  await Promise.all([make(), make()].map(client => assert.rejects(client.prepare(draft, file, status), { message: responseLost })))
  assert.equal(calls.length, 2)
  assert.equal(calls[0].intent_id, calls[1].intent_id)
  assert.ok(calls.every(call => call.op === 'prepare' && call.content_base64 === wireBytes))
  const saved = records.get(intentKey)
  assert.ok(saved)
  assert.equal(JSON.parse(saved).intent_id, calls[0].intent_id)
  await assert.rejects(nativeDeliveryReservations(intentKey, () => { throw new Error('storage callback failed') }), /storage callback failed/)
})
