import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseFixtureRequest, parseFixtureReset, parseFixtureResult } from './fixture-contract.ts';

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

test('all fixture request variants reject wrong types and extra keys at the boundary', () => {
  const valid = [
    { op: 'stop' }, { op: 'inspect' }, { op: 'cursor_login' }, { op: 'cursor_inspect' },
    { op: 'workspace_inspect' }, { op: 'mesh_info' }, { op: 'mesh_pairing' }, { op: 'native_join' },
    { op: 'native_provider_setup' }, { op: 'native_provider_clipboard' }, { op: 'native_provider_queue' },
    { op: 'native_provider_inspect' }, { op: 'restart', role: 'source' }, { op: 'restart', role: 'source', crash: false },
    { op: 'workspace_release', gate: 'main' }, { op: 'mesh_approve', peer: 'a'.repeat(64), code: '123456' },
    { op: 'native_approve', peer: 'a'.repeat(64), code: '123456' }, { op: 'native_setup', invitation_file: '/tmp/invitation' },
    { op: 'workspace_native_setup', invitation_file: '/tmp/invitation' }, { op: 'native_provider_login', command: 'codex login' },
    { op: 'native_provider_stage', stage: 'ready' }, { op: 'native_send', name: 'file.bin', content_base64: 'AA==' },
  ];
  for (const request of valid) assert.deepEqual(parseFixtureRequest(request), request);
  for (const request of [
    { op: 'stop', extra: true }, { op: 'restart', role: 'source', crash: 1 },
    { op: 'workspace_release', gate: 'other' }, { op: 'native_provider_stage', stage: 'other' },
    { op: 'native_send', name: '', content_base64: 'AA==' }, { op: 'native_send', name: 'a', content_base64: 'not base64' }, { op: 'native_approve', peer: 'x', code: 1 },
    { op: 'unknown' }, { op: 1 }, null,
  ]) assert.throws(() => parseFixtureRequest(request));
});

test('typed result parsing rejects malformed fields and keeps legal zero/null values', () => {
  const request = { op: 'native_provider_inspect' };
  const result = parseFixtureResult(request, {
    manager: null, roles: [], execution_model: '', setup: { directory: '/private', command: 'codex login' },
    login_count: 0, app_server_link_state: 'disconnected', readiness: 'unverified', catalog: 'unknown',
    authentication: 'unverified', checked_at_ms: null, jobs: [], deferred_input: null,
    total_invocations: 0, running_invocations: 0, deferred_invocations: 0,
  });
  assert.equal(result.checked_at_ms, null);
  assert.deepEqual(parseFixtureResult({ op: 'stop' }, { stopped: true }), { stopped: true });
  assert.throws(() => parseFixtureResult(request, { manager: null }));
  assert.throws(() => parseFixtureResult({ op: 'inspect' }, { wallet_files: -1, mesh_records: 0, chain_rpc_calls: 0 }));
  assert.throws(() => parseFixtureResult({ op: 'workspace_release', gate: 'main' }, { released: 'main', extra: true }));
});
