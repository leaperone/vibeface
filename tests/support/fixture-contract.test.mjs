import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseFixtureReset } from './fixture-contract.ts';

test('reset accepts the five fixture kinds with exact fields', () => {
  for (const kind of ['single', 'delivery', 'workspace', 'mesh', 'cursor']) {
    assert.deepEqual(parseFixtureReset({ op: 'reset', kind }), { op: 'reset', kind });
  }
});

test('reset rejects an unknown kind, missing fields, or extra fields', () => {
  for (const request of [
    { op: 'reset' },
    { kind: 'mesh' },
    { op: 'restart', kind: 'single' },
    { op: 'reset', kind: 1 },
    { op: 'reset', kind: 'native' },
    { op: 'reset', kind: 'single', role: 'source' },
  ]) {
    assert.throws(() => parseFixtureReset(request));
  }
});
