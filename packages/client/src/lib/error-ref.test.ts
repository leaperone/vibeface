import assert from 'node:assert/strict'
import test from 'node:test'
import { failureCopyText, failureRef, lastFailure, newRef, problemReport, recordFailure, requestConversation } from './error-ref.ts'

test('an error number is eight base-36 characters', () => {
  assert.match(newRef(), /^[0-9a-z]{8}$/)
  assert.equal(newRef(new Uint8Array([0, 1, 35, 36, 71, 255, 10, 11])), '01z0z3ab')
})

test('a failure is found by its thrown value, or by the words shown for it', () => {
  const cause = new Error('peer did not answer')
  const failure = recordFailure(cause, '另一台设备没有回应。', 'conv-1', 1_000)
  assert.equal(failureRef(cause), failure)
  assert.equal(failureRef('另一台设备没有回应。', 2_000), failure)
  assert.equal(failureRef('另一台设备没有回应。', 1_000 + 11 * 60_000), undefined, 'old words name no failure')
  assert.equal(failureRef(new Error('other')), undefined)
  assert.equal(lastFailure(), failure)
  assert.equal(failureCopyText(failure), `错误编号 ${failure.ref}\n时间 1970-01-01T00:00:01.000Z\n会话 conv-1`)
})

test('a request names its conversation directly or inside rpc params', () => {
  assert.equal(requestConversation({ conversation_id: 'c1' }), 'c1')
  assert.equal(requestConversation({ method: 'history', params: { conversation_id: 'c2' } }), 'c2')
  assert.equal(requestConversation({ conversation_id: 'bad id' }), undefined)
})

test('a problem report leaves out what the person did not attach', () => {
  assert.equal(problemReport({ description: ' 发不出消息 ', conversation_id: 'c1', ref: 'a1b2c3d4' }), '[问题报告] 发不出消息\n会话：c1\n错误编号：a1b2c3d4')
  assert.equal(problemReport({ description: '发不出消息' }), '[问题报告] 发不出消息')
  assert.throws(() => problemReport({ description: '  ' }))
})
