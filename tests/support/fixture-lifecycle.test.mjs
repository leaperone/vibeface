// Run offline with Node 24: node --test tests/support/fixture-lifecycle.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { withControlledFixture } from './fixture-lifecycle.ts';

const resetError = new Error('reset failed');
const useError = new Error('use failed');
const stopError = new Error('stop failed');
const scenarios = [
  ['success', {}],
  ['reset fails', { reset: resetError }],
  ['use fails', { use: useError }],
  ['stop fails', { stop: stopError }],
  ['reset and stop fail', { reset: resetError, stop: stopError }],
  ['use and stop fail', { use: useError, stop: stopError }],
  ['reset throws undefined', { reset: undefined }],
  ['use throws undefined', { use: undefined }],
  ['stop throws undefined', { stop: undefined }],
  ['reset throws undefined and stop fails', { reset: undefined, stop: stopError }],
  ['use and stop both throw undefined', { use: undefined, stop: undefined }],
  ['use fails and stop throws undefined', { use: useError, stop: undefined }],
];

for (const [name, failures] of scenarios) {
  test(name, async () => {
    const calls = [];
    const call = async op => {
      calls.push(op);
      if (Object.hasOwn(failures, op)) throw failures[op];
    };
    const result = await withControlledFixture({
      run: async () => {
        await call('reset');
        await call('use');
      },
      stop: () => call('stop'),
    }).then(
      () => ({ ok: true }),
      error => ({ ok: false, error }),
    );

    assert.deepEqual(calls, Object.hasOwn(failures, 'reset') ? ['reset', 'stop'] : ['reset', 'use', 'stop']);
    const errors = Object.values(failures);
    if (errors.length === 0) {
      assert.deepEqual(result, { ok: true });
    } else {
      assert.equal(result.ok, false);
      if (errors.length === 1) {
        assert.equal(result.error, errors[0]);
      } else {
        assert(result.error instanceof AggregateError);
        assert.equal(result.error.errors.length, 2);
        assert.equal(result.error.errors[0], errors[0]);
        assert.equal(result.error.errors[1], errors[1]);
        assert.equal(result.error.message, `${String(errors[0])}; cleanup also failed: ${String(errors[1])}`);
      }
    }
  });
}

test('waits for use before stopping and waits for stop before completing', async () => {
  const use = Promise.withResolvers();
  const stop = Promise.withResolvers();
  const stopping = Promise.withResolvers();
  let completed = false;
  const result = withControlledFixture({
    run: () => use.promise,
    stop: () => {
      stopping.resolve();
      return stop.promise;
    },
  }).then(() => { completed = true; });

  let stopStarted = false;
  const observedStop = stopping.promise.then(() => { stopStarted = true; });
  await Promise.resolve();
  assert.equal(stopStarted, false);
  use.resolve();
  await observedStop;
  assert.equal(completed, false);
  stop.resolve();
  await result;
  assert.equal(completed, true);
});

test('keeps both thrown values if either cannot be converted to text', async () => {
  const unprintable = { toString() { throw new Error('cannot render'); } };
  for (const [primary, cleanup] of [[unprintable, stopError], [useError, unprintable]]) {
    await assert.rejects(
      withControlledFixture({
        run: () => { throw primary; },
        stop: () => { throw cleanup; },
      }),
      error => {
        assert(error instanceof AggregateError);
        assert.equal(error.errors[0], primary);
        assert.equal(error.errors[1], cleanup);
        assert.equal(error.message, 'Fixture failed and cleanup also failed');
        return true;
      },
    );
  }
});
