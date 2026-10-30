import assert from 'node:assert/strict'
import test from 'node:test'
import { hideRecovery, recoveryHidden } from './recovery-notice.ts'

test('是我，收起提醒 hides that recovery only until the app or page opens again', async () => {
  hideRecovery('n', 'c1')
  assert.equal(recoveryHidden('n', 'c1'), true)
  assert.equal(recoveryHidden('n', 'c2'), false, 'a later recovery still shows')
  assert.equal(recoveryHidden('m', 'c1'), false, 'and so does one on another network')
  const fresh = './recovery-notice.ts?reopened'
  const reopened: typeof import('./recovery-notice.ts') = await import(fresh)
  assert.equal(reopened.recoveryHidden('n', 'c1'), false, 'reopening shows the reminder again')
})
