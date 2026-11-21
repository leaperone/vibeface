import assert from 'node:assert/strict'
import { parentPort, workerData } from 'node:worker_threads'
import { createDeliveryClient } from '@vibeface/client/delivery/client.ts'
import { webDeliveryReservations } from './delivery-reservations.ts'
import { connection, draft, file, oldId, status } from './delivery-reservations.fixture.ts'

// Storage and lock ownership live in the parent so workers model one browser origin.
const sync = new Int32Array(workerData.buffer, 0, 2)
const bytes = new Uint8Array(workerData.buffer, 8)
function invoke(op, key, value) {
  Atomics.store(sync, 0, 0)
  parentPort.postMessage({ op, key, value })
  assert.notEqual(Atomics.wait(sync, 0, 0, 10000), 'timed-out', 'parent storage response stalled')
  return JSON.parse(new TextDecoder().decode(bytes.subarray(0, Atomics.load(sync, 1))))
}
const storage = { getItem: key => invoke('get', key), setItem: (key, value) => { invoke('set', key, value) } }
const grants = new Map()
parentPort.on('message', message => {
  if (message.op === 'lock-grant') grants.get(message.id)?.resolve()
  if (message.op === 'lock-error') grants.get(message.id)?.reject(new Error(message.error))
})
const parentLocks = {
  request(name, options, callback) {
    if (options?.mode !== 'exclusive') return Promise.reject(new Error('delivery reservation requires an exclusive lock'))
    const id = `lock-${crypto.randomUUID()}`
    const granted = new Promise((resolve, reject) => {
      grants.set(id, { resolve, reject })
      parentPort.postMessage({ op: 'lock-acquire', id, name })
    })
    return granted.then(async () => {
      try { return await callback() }
      finally { parentPort.postMessage({ op: 'lock-release', id }) }
    }).finally(() => grants.delete(id))
  },
}
const webReserve = webDeliveryReservations(parentLocks)
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
