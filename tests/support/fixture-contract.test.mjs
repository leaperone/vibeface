import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { createFixtureContract, parseFixtureRequest } from './fixture-contract.ts';
import { context, inspection, readyDetails } from './fixture-samples.mjs';

const corpus = JSON.parse(readFileSync(new URL('./fixture-requests.json', import.meta.url), 'utf8'));
test('the shared corpus enforces all request fields, topologies and caller scopes', () => {
  assert.equal(new Set(corpus.filter(([, surfaces]) => surfaces).map(([, , request]) => request.op)).size, 22);
  for (const [kind, surfaces, request] of corpus) {
    for (const surface of ['web', 'native', 'child']) {
      const parse = () => parseFixtureRequest(request, surface, kind);
      if (surfaces.split(' ').includes(surface)) assert.deepEqual(parse(), request);
      else assert.throws(parse, undefined, JSON.stringify([kind, surface, request]));
    }
  }
});

const pairing = { peer: 'a'.repeat(64), code: '012345' };
const make = kind => createFixtureContract({ ...context, kind });
const inspectWorkspace = { op: 'workspace_inspect' };
const provider = {
  manager: { provider: 'codex', model: '', reasoning_effort: null }, roles: ['agent', 'execution'], execution_model: '',
  setup: { directory: '/task/run/receiver/providers/codex', command: 'controlled login' },
  login_count: 0, app_server_link_state: 'connected', readiness: 'unverified', catalog: 'unavailable',
  authentication: 'unverified', checked_at_ms: null, jobs: [], deferred_input: null,
  total_invocations: 0, running_invocations: 0, deferred_invocations: 0,
};
const [webEvidence, nativeEvidence] = JSON.parse(execFileSync('python3', [
  '-B', fileURLToPath(new URL('./fixture-contract.test.py', import.meta.url)), '--workspace-example',
], { encoding: 'utf8' }));
const mesh = readyDetails('mesh');
const meshInfo = { ...mesh, invitation: 'join', mesh: {
  enabled: true, network: 't-123456abcdef', bootstrap: [`vibeface://node/${mesh.nodes.public.peer}?addr=127.0.0.1:8787`],
  allow_private: true, advertise: [],
}, relays: [], announced_relays: ['http://127.0.0.1:9999'] };

test('every operation validates its complete success result and rejects missing, wrong-type and extra fields', () => {
  const examples = {
    reset: readyDetails('single'), restart: readyDetails('single'), stop: { stopped: true }, inspect: inspection,
    cursor_login: { signed_in: true }, cursor_inspect: { selected_official: false, bot_enabled: true, execution: ['codex'], prompts: 0 },
    workspace_release: { released: 'main' }, workspace_inspect: webEvidence,
    mesh_info: meshInfo, mesh_pairing: pairing, mesh_approve: { approved: true },
    native_setup: readyDetails('delivery'), workspace_native_setup: readyDetails('workspace'),
    native_join: pairing, native_approve: { approved: true }, native_provider_setup: { configured: true },
    native_provider_clipboard: { pasted: true }, native_provider_login: { executed: true, private_root_verified: true, controlled_bin_verified: true, login_count: 1 },
    native_provider_stage: { stage: 'ready' }, native_provider_inspect: provider,
    native_provider_queue: { bot_id: 'bot', conversation_id: 'chat', running_trigger: 'message:m', deferred_message_id: 'later' },
    native_send: { order_id: 'b'.repeat(64) },
  };
  const covered = new Set();
  for (const [kind, surfaces, request] of corpus) {
    if (!surfaces) continue;
    const { op } = request, contract = make(kind), ready = readyDetails(kind);
    contract.ready(ready);
    const result = op === 'reset' || op === 'restart' ? ready : op === 'workspace_release' ? { released: request.gate } :
      op === 'native_provider_stage' ? { stage: request.stage } : examples[op];
    assert.deepEqual(contract.result(request, result), result);
    covered.add(op);
    for (const key of Object.keys(result)) {
      const missing = { ...result };
      delete missing[key];
      for (const malformed of [missing, { ...result, [key]: 1.5 }, { ...result, extra: true }]) {
        assert.throws(() => contract.result(request, malformed), undefined, op + '.' + key);
      }
    }
  }
  assert.deepEqual([...covered].sort(), Object.keys(examples).sort());
});

test('ready and subsequent results bind the original process, origin, private paths, Node identities and routes', () => {
  for (const kind of ['single', 'cursor', 'delivery', 'workspace', 'mesh']) {
    const initial = readyDetails(kind), contract = make(kind);
    const role = Object.keys(initial.nodes)[0];
    assert.deepEqual(contract.ready(initial), initial);
    const bad = mutate => { const copy = structuredClone(initial); mutate(copy); assert.throws(() => make(kind).ready(copy)); };
    bad(copy => { copy.run = '/other'; });
    bad(copy => { copy.nodes[role].directory = '/task/run-sibling/data'; });
    bad(copy => { delete copy.nodes[role]; });
    bad(copy => { copy.nodes.surplus = copy.nodes[role]; });
    bad(copy => { copy.nodes[role].peer = 'wrong'; });
    for (const url of ['http://127.1:8100', 'http://user@127.0.0.1:8100', 'http://127.0.0.1:8100/a/..', 'http://127.0.0.1:9999']) {
      bad(copy => { copy.nodes[role].url = url; });
    }
    if (kind === 'workspace') bad(copy => { copy.gates.main = '/task/run/../outside/gate'; });
    if (kind === 'delivery') {
      bad(copy => { copy.nodes.provider.peer = copy.nodes.source.peer; });
      bad(copy => { copy.nodes.receiver.entry = 'http://user@127.0.0.1:8201/#bootstrap=abc'; });
      bad(copy => { copy.nodes.receiver.entry = copy.nodes.source.entry; });
    }
    const changed = structuredClone(initial);
    changed.nodes[role].peer = 'f'.repeat(64);
    assert.throws(() => contract.result({ op: 'restart', role }, changed));
  }
  const contract = make('mesh');
  contract.ready(mesh);
  for (const change of [
    { mesh: { ...meshInfo.mesh, bootstrap: [`vibeface://node/${pairing.peer}?addr=127.0.0.1:8787`] } },
    { mesh: { ...meshInfo.mesh, network: 'fixed' } }, { relays: ['http://127.0.0.1:9999'] },
    { announced_relays: [] }, { announced_relays: ['https://example.org'] },
  ]) assert.throws(() => contract.result({ op: 'mesh_info' }, { ...meshInfo, ...change }));
});

test('workspace projects real Python nonempty rows and validates input targets only after native setup', () => {
  const contract = make('workspace'), ready = readyDetails('workspace');
  contract.ready(ready);
  assert.deepEqual(contract.result(inspectWorkspace, webEvidence).turns, [{ thread_id: 'thread', turn_id: 'turn', state: 'inProgress' }]);
  assert.throws(() => contract.result(inspectWorkspace, nativeEvidence));
  contract.result({ op: 'workspace_native_setup', invitation_file: '/task/invitation' }, ready);
  assert.throws(() => contract.result(inspectWorkspace, webEvidence));
  assert.deepEqual(contract.result(inspectWorkspace, nativeEvidence), nativeEvidence);
  for (const target of [
    { kind: 'bot_main', bot_id: 'b' }, { kind: 'task', task_id: 't' }, { kind: 'work', work_id: 'w' },
    { kind: 'routine', bot_id: 'b', trigger: 'r' }, { kind: 'channel_member', channel_id: 'c', bot_id: 'b' },
    { kind: 'channel_task', task_id: 't', bot_id: 'b', channel_id: 'c' },
  ]) {
    const row = { ...nativeEvidence.inputs[0], target };
    assert.equal(contract.result(inspectWorkspace, { ...nativeEvidence, inputs: [row] }).inputs[0].text, '');
    assert.throws(() => contract.result(inspectWorkspace, { ...nativeEvidence, inputs: [{ ...row, target: { ...target, extra: 1 } }] }));
  }
  assert.throws(() => contract.result({ op: 'workspace_release', gate: 'main' }, { released: 'channel' }));
});

test('provider inspection validates nested rows, nullable states, zero counts and request echoes', () => {
  const contract = make('delivery');
  const request = { op: 'native_provider_inspect' };
  for (const row of [provider, { ...provider, manager: null, checked_at_ms: 0, jobs: [{ trigger: 'm', state: 'running' }],
    deferred_input: { message_id: 'm', target: { kind: 'bot_main', bot_id: 'b' }, deferred: true, cancelled: false } }]) {
    assert.deepEqual(contract.result(request, row), row);
  }
  for (const fields of [{ checked_at_ms: -1 }, { roles: ['operator'] }, { catalog: 'unknown' }, { app_server_link_state: 'disconnected' },
    { jobs: [{ trigger: 'm', state: 42 }] }, { setup: { ...provider.setup, command: '' } }, { manager: { ...provider.manager, extra: true } },
    { deferred_input: { message_id: 'm', target: { kind: 'task', task_id: 'b' }, deferred: true, cancelled: false } },
  ]) assert.throws(() => contract.result(request, { ...provider, ...fields }));
  assert.throws(() => contract.result({ op: 'native_provider_stage', stage: 'ready' }, { stage: 'signed_out' }));
  assert.throws(() => contract.result({ op: 'native_join' }, { ...pairing, code: 'oops' }));
});
