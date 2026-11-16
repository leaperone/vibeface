import assert from 'node:assert/strict'
import { locks, parentPort, workerData } from 'node:worker_threads'
import { createDeliveryClient } from '@vibeface/client/delivery/client.ts'
import { webDeliveryReservations } from './delivery-reservations.ts'
import { connection, draft, file, oldId, status } from './delivery-reservations.fixture.ts'

if (workerData.adapter) {
  let invoked = false
  const value = { intent_id: oldId, fingerprint: 'test-fingerprint' }
  const callback = () => { invoked = true; return value }
  await assert.rejects(webDeliveryReservations(undefined)('unsupported', callback), /更新浏览器或使用原生应用/)
  assert.equal(invoked, false)
  const failed = new Error('acquisition failed')
  const fake = { request(name, options) {
    assert.equal(this, fake)
    assert.equal(name, 'vibeface-delivery-intent:full-key')
    assert.deepEqual(options, { mode: 'exclusive' })
    return Promise.reject(failed)
  } }
  await assert.rejects(webDeliveryReservations(fake)('full-key', callback), error => error === failed)
  assert.equal(invoked, false)
  const reserve = webDeliveryReservations(locks), key = 'callback-release-' + crypto.randomUUID()
  await assert.rejects(reserve(key, () => { throw new Error('callback failed') }), /callback failed/)
  assert.equal(await reserve(key, callback), value)
  assert.equal(invoked, true)
  parentPort.close()
} else {
  // Same parent-owned linearizable synchronous KeyValue as the original CLIENT-18
  // proof. Node 24's real LockManager is shared across these independent workers.
  const sync = new Int32Array(workerData.buffer, 0, 2)
  const bytes = new Uint8Array(workerData.buffer, 8)
  function invoke(op, key, value) {
    Atomics.store(sync, 0, 0)
    parentPort.postMessage({ op, key, value })
    assert.notEqual(Atomics.wait(sync, 0, 0, 10000), 'timed-out', 'parent storage response stalled')
    return JSON.parse(new TextDecoder().decode(bytes.subarray(0, Atomics.load(sync, 1))))
  }
  const storage = { getItem: key => invoke('get', key), setItem: (key, value) => { invoke('set', key, value) } }
  const webReserve = webDeliveryReservations(locks)
  const go = new Promise(resolve => {
    parentPort.on('message', message => { if (message.op === 'go') resolve() })
  })
  const reserve = async (key, callback) => {
    parentPort.postMessage({ op: 'ready', key })
    await go
    return webReserve(key, callback)
  }
  const transport = connection((method, params) => new Promise((resolve, reject) => {
    const response = message => {
      if (message.op !== 'rpc-result') return
      parentPort.off('message', response)
      if (message.error) reject(new Error(message.error))
      else resolve(message.value)
    }
    parentPort.on('message', response)
    parentPort.postMessage({ op: 'rpc', method, params })
  }), workerData.network)
  try {
    await createDeliveryClient(transport, storage, reserve).prepare(draft, { ...file, name: workerData.name ?? file.name }, status, workerData.restart ? oldId : undefined)
    parentPort.postMessage({ op: 'done', error: null })
  } catch (error) { parentPort.postMessage({ op: 'done', error: error.message }) }
  parentPort.close()

}
