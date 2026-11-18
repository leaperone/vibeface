import assert from 'node:assert/strict';
import { isDeepStrictEqual } from 'node:util';
import { insideTaskArtifacts } from './artifact-paths.ts';

type Decoder<T> = (value: unknown) => T;
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** Decode every own field and reject omissions or extensions. */
function object<S extends Record<string, Decoder<unknown>>>(shape: S) {
  return (value: unknown): { [K in keyof S]: ReturnType<S[K]> } => {
    const keys = Object.keys(shape);
    assert(isRecord(value) && Object.keys(value).length === keys.length &&
      keys.every(key => Object.hasOwn(value, key)), 'Invalid fixture object fields');
    return Object.fromEntries(keys.map(key => [key, shape[key](value[key])])) as { [K in keyof S]: ReturnType<S[K]> };
  };
}

function list<T>(decode: Decoder<T>) {
  return (value: unknown) => {
    assert(Array.isArray(value), 'Expected a fixture array');
    return value.map(decode);
  };
}
function nullable<T>(decode: Decoder<T>) {
  return (value: unknown) => value === null ? null : decode(value);
}
function literal<const T extends string | number | boolean>(expected: T) {
  return (value: unknown) => { assert(value === expected, 'Unexpected fixture value'); return expected; };
}
function choice<const T extends string>(...values: T[]) {
  return (value: unknown): T => {
    const found = values.find(item => item === value);
    assert(found !== undefined, 'Unknown fixture value');
    return found;
  };
}
function string(value: unknown) { assert(typeof value === 'string', 'Expected fixture string'); return value; }
function text(value: unknown) { const result = string(value); assert(result.length > 0, 'Expected fixture text'); return result; }
function boolean(value: unknown) { assert(typeof value === 'boolean', 'Expected fixture boolean'); return value; }
function count(value: unknown) {
  assert(typeof value === 'number' && Number.isSafeInteger(value) && value >= 0, 'Expected nonnegative fixture count');
  return value;
}
function matches(pattern: RegExp) {
  return (value: unknown) => { const result = text(value); assert(pattern.test(result), 'Invalid fixture string format'); return result; };
}
function base64(value: unknown) {
  const encoded = string(value);
  assert(Buffer.from(encoded, 'base64').toString('base64') === encoded, 'Invalid base64 content');
  return encoded;
}
const peer = matches(/^[0-9a-f]{64}$/);
const code = matches(/^[0-9]{6}$/);
const gate = choice('taskA', 'taskB', 'main', 'channel');
const stage = choice('signed_out', 'ready', 'catalog_unknown');
const role = choice('source', 'receiver', 'provider', 'public', 'member');
const all = ['single', 'cursor', 'delivery', 'workspace', 'mesh'] as const;
export type FixtureKind = typeof all[number];
export type WorkspaceGate = ReturnType<typeof gate>;
type Surface = 'web' | 'native' | 'child';
type KindList = readonly FixtureKind[];
const empty = object({});
const pairing = object({ peer, code });
const invitationFile = object({ invitation_file: text });
function restartFields(value: unknown): { role: ReturnType<typeof role>; crash?: boolean } {
  return isRecord(value) && Object.hasOwn(value, 'crash')
    ? object({ role, crash: boolean })(value) : object({ role })(value);
}
function operation<T, const W extends KindList, const N extends KindList>(fields: Decoder<T>, web: W, native: N) {
  return { fields, web, native };
}
const requests = {
  reset: operation(object({ kind: choice(...all) }), all, []),
  stop: operation(empty, all, []),
  restart: operation(restartFields, all, []),
  inspect: operation(empty, all, ['delivery']),
  cursor_login: operation(empty, ['cursor'], []),
  cursor_inspect: operation(empty, ['cursor'], []),
  workspace_release: operation(object({ gate }), ['workspace'], ['workspace']),
  workspace_inspect: operation(empty, ['workspace'], ['workspace']),
  mesh_info: operation(empty, ['mesh'], []),
  mesh_pairing: operation(empty, ['mesh'], []),
  mesh_approve: operation(pairing, ['mesh'], []),
  native_setup: operation(invitationFile, [], ['delivery']),
  workspace_native_setup: operation(invitationFile, [], ['workspace']),
  native_join: operation(empty, [], ['delivery', 'workspace']),
  native_approve: operation(pairing, [], ['delivery', 'workspace']),
  native_provider_setup: operation(empty, [], ['delivery']),
  native_provider_clipboard: operation(empty, [], ['delivery']),
  native_provider_login: operation(object({ command: text }), [], ['delivery']),
  native_provider_stage: operation(object({ stage }), [], ['delivery']),
  native_provider_queue: operation(empty, [], ['delivery']),
  native_provider_inspect: operation(empty, [], ['delivery']),
  native_send: operation(object({ name: text, content_base64: base64 }), [], ['delivery']),
};
type Op = keyof typeof requests;
type ByOp = { [O in Op]: { op: O } & ReturnType<typeof requests[O]['fields']> };
export type FixtureRequest = ByOp[Op];
type ScopedRequest<S extends 'web' | 'native', K extends FixtureKind> = {
  [O in Op]: K extends typeof requests[O][S][number] ? ByOp[O] : never
}[Op];
const topology = {
  single: ['source'], cursor: ['source'], workspace: ['source'],
  delivery: ['source', 'receiver', 'provider'], mesh: ['public', 'member'],
} as const;
export type WebRequest<K extends FixtureKind> =
  Exclude<ScopedRequest<'web', K>, ByOp['restart' | 'reset']> |
  { op: 'reset'; kind: K } | { op: 'restart'; role: typeof topology[K][number]; crash?: boolean };
export type NativeRequest<K extends FixtureKind> = ScopedRequest<'native', K>;

function isOp(value: unknown): value is Op {
  return typeof value === 'string' && Object.hasOwn(requests, value);
}
export function parseFixtureRequest<R extends FixtureRequest>(value: R, surface: Surface, kind?: FixtureKind): R;
export function parseFixtureRequest(value: unknown, surface: Surface, kind?: FixtureKind): FixtureRequest;
export function parseFixtureRequest(value: unknown, surface: Surface, kind?: FixtureKind): FixtureRequest {
  assert(isRecord(value) && Object.hasOwn(value, 'op') && isOp(value.op), 'Unknown fixture operation');
  const { op, ...fields } = value;
  const spec = requests[op];
  if (surface === 'child') assert(op !== 'reset' && op !== 'stop', 'Controller-only operation');
  const allowed: KindList = surface === 'child' ? [...spec.web, ...spec.native] : spec[surface];
  assert(kind === undefined ? surface === 'web' && (op === 'reset' || op === 'stop') : allowed.includes(kind),
    'Operation outside fixture scope');
  const result = { op, ...spec.fields(fields) } as FixtureRequest;
  if (result.op === 'restart') {
    assert(kind && topology[kind].some(role => role === result.role), 'Unknown role for this fixture');
  }
  return result;
}

export function loopbackOrigin(value: unknown) {
  const raw = text(value);
  assert(/^http:\/\/127\.0\.0\.1:[1-9][0-9]{0,4}\/?$/.test(raw), 'Expected literal loopback listener');
  const url = new URL(raw);
  assert(url.port, 'Expected an explicit fixture port');
  return url.origin;
}
function pathBelow(root: string) {
  return (value: unknown) => {
    const path = text(value);
    assert(insideTaskArtifacts(root, path), 'Fixture path outside the task directory');
    return path;
  };
}
const nodeFields = { url: loopbackOrigin, entry: nullable(text), peer, directory: text };
function fixtureNode(run: string) {
  return (value: unknown) => {
    const node = object({ ...nodeFields, directory: pathBelow(run) })(value);
    if (node.entry !== null) {
      const prefix = node.url + '/#bootstrap=';
      assert(node.entry.startsWith(prefix) && /^[A-Za-z0-9_-]+$/.test(node.entry.slice(prefix.length)),
        'Invalid owner link for this Node');
    }
    return node;
  };
}
export type NodeInfo = ReturnType<ReturnType<typeof fixtureNode>>;
export type OwnerNode = NodeInfo & { entry: string };
function ownerNode(run: string) {
  return (value: unknown): OwnerNode => {
    const node = fixtureNode(run)(value);
    assert(node.entry !== null, 'Expected owner link for this Node');
    return { ...node, entry: node.entry };
  };
}
function readyShapes(run: string) {
  const node = fixtureNode(run), owner = ownerNode(run), path = pathBelow(run);
  const source = object({ run: literal(run), nodes: object({ source: owner }) });
  return {
    single: source, cursor: source,
    workspace: object({ run: literal(run), nodes: object({ source: owner }),
      gates: object({ taskA: path, taskB: path, main: path, channel: path }) }),
    delivery: object({ run: literal(run), nodes: object({ source: owner, receiver: owner, provider: node }) }),
    mesh: object({ run: literal(run), nodes: object({ public: node, member: node }) }),
  };
}
export type FixtureReady<K extends FixtureKind> = ReturnType<ReturnType<typeof readyShapes>[K]>;
type Generation<K extends FixtureKind> = { kind: K; artifacts: string; origin: string };
function parseReady<K extends FixtureKind>(value: unknown, context: Generation<K>): FixtureReady<K> {
  assert(isRecord(value), 'Expected fixture readiness');
  const run = pathBelow(context.artifacts)(value.run);
  const ready = readyShapes(run)[context.kind](value);
  const page = 'public' in ready.nodes ? ready.nodes.public : ready.nodes.source;
  assert(page.url === context.origin, 'Fixture page origin changed');
  const members = Object.values(ready.nodes);
  assert(new Set(members.map(node => node.peer)).size === members.length, 'Fixture identities must be distinct');
  return ready as FixtureReady<K>;
}

const turn = { thread_id: text, turn_id: text };
const evidence = {
  invocations: list(object({ label: text, conversation_id: text, trigger: text, thread_id: text, messages: list(string) })),
  steers: list(object({ message_id: text, ...turn, text })),
  starts: list(object(turn)), interrupts: list(object(turn)), turns: list(object({ ...turn, state: text })),
};
const botTarget = object({ kind: literal('bot_main'), bot_id: text });
const targets = {
  bot_main: botTarget, task: object({ kind: literal('task'), task_id: text }),
  work: object({ kind: literal('work'), work_id: text }),
  routine: object({ kind: literal('routine'), bot_id: text, trigger: text }),
  channel_member: object({ kind: literal('channel_member'), channel_id: text, bot_id: text }),
  channel_task: object({ kind: literal('channel_task'), channel_id: text, task_id: text, bot_id: text }),
};
function inputTarget(value: unknown) {
  assert(isRecord(value), 'Expected workspace input target');
  const kind = choice('bot_main', 'task', 'work', 'routine', 'channel_member', 'channel_task')(value.kind);
  return targets[kind](value);
}
const inputs = list(object({ message_id: text, conversation_id: text, target: inputTarget, text: string, deferred: boolean, cancelled: boolean }));
const providerInspection = object({
  manager: nullable(object({ provider: literal('codex'), model: string, reasoning_effort: nullable(string) })),
  roles: list(choice('agent', 'execution')), execution_model: string, setup: object({ directory: text, command: text }),
  login_count: count, app_server_link_state: choice('connected', 'starting', 'off', 'not_found', 'handshake_failed', 'launch_failed'),
  readiness: choice('starting', 'ready', 'unverified', 'needs_setup'), catalog: choice('unavailable', 'available'),
  authentication: choice('unverified', 'authenticated', 'not_required', 'sign_in_required'), checked_at_ms: nullable(count),
  jobs: list(object({ trigger: text, state: text })),
  deferred_input: nullable(object({ message_id: text, target: botTarget, deferred: boolean, cancelled: boolean })),
  total_invocations: count, running_invocations: count, deferred_invocations: count,
});
const inspect = object({ wallet_files: count, mesh_records: count, chain_rpc_calls: count });
const approved = object({ approved: literal(true) });
const meshConfig = object({
  enabled: literal(true), network: matches(/^t-[0-9a-f]{12}$/), bootstrap: list(text), allow_private: literal(true), advertise: list(text),
});

function resultDecoders<K extends FixtureKind>(context: Generation<K>) {
  let initial: FixtureReady<K> | undefined;
  let workspaceInitialized = false;
  function ready(value: unknown) {
    const result = parseReady(value, context);
    initial = structuredClone(result);
    workspaceInitialized = false;
    return result;
  }
  function sameGeneration(value: unknown): FixtureReady<K> {
    const result = parseReady(value, context);
    assert(initial && isDeepStrictEqual(initial, result), 'Fixture generation identity changed');
    return result;
  }
  function meshInfo(value: unknown) {
    const details = object({
      run: text, nodes: (value: unknown) => value, invitation: text, mesh: meshConfig, relays: list(text), announced_relays: list(text),
    })(value);
    assert(initial && context.kind === 'mesh', 'Expected a ready mesh generation');
    const mesh = parseReady({ run: details.run, nodes: details.nodes }, { ...context, kind: 'mesh' });
    assert(isDeepStrictEqual(initial, mesh), 'Mesh identity changed');
    const { bootstrap, advertise } = details.mesh;
    assert(bootstrap.length === 1 && advertise.length === 0 && details.relays.length === 0 &&
      details.announced_relays.length === 1, 'Invalid fixture mesh routes');
    const prefix = 'vibeface://node/' + mesh.nodes.public.peer + '?addr=';
    assert(bootstrap[0].startsWith(prefix), 'Mesh bootstrap must be the public Node');
    const address = bootstrap[0].slice(prefix.length);
    assert(!address.includes('/'), 'Invalid bootstrap socket address');
    loopbackOrigin('http://' + address);
    loopbackOrigin(details.announced_relays[0]);
    return { ...details, ...mesh };
  }

  const results = {
    reset: ready, stop: object({ stopped: literal(true) }), restart: sameGeneration, inspect,
    cursor_login: object({ signed_in: literal(true) }),
    cursor_inspect: object({ selected_official: boolean, bot_enabled: boolean, execution: list(text), prompts: count }),
    workspace_release: object({ released: gate }),
    workspace_inspect: (value: unknown) => workspaceInitialized ? object({ ...evidence, inputs })(value) : object(evidence)(value),
    mesh_info: meshInfo, mesh_pairing: pairing, mesh_approve: approved,
    native_setup: sameGeneration, workspace_native_setup: (value: unknown): FixtureReady<K> => {
      const initialized = sameGeneration(value);
      workspaceInitialized = true;
      return initialized;
    }, native_join: pairing, native_approve: approved,
    native_provider_setup: object({ configured: literal(true) }), native_provider_clipboard: object({ pasted: literal(true) }),
    native_provider_login: object({ executed: literal(true), private_root_verified: literal(true), controlled_bin_verified: literal(true), login_count: literal(1) }),
    native_provider_stage: object({ stage }), native_provider_inspect: providerInspection,
    native_provider_queue: object({ bot_id: text, conversation_id: text, running_trigger: text, deferred_message_id: text }),
    native_send: object({ order_id: peer }),
  } satisfies Record<Op, Decoder<unknown>>;

  return { results, ready };
}
export type ResultByOp<K extends FixtureKind> = {
  [O in Op]: ReturnType<ReturnType<typeof resultDecoders<K>>['results'][O]>
};
export function createFixtureContract<K extends FixtureKind>(context: Generation<K>) {
  const { results, ready } = resultDecoders(context);
  function result<R extends FixtureRequest>(request: R, value: unknown): ResultByOp<K>[R['op']];
  function result(request: FixtureRequest, value: unknown) {
    if (request.op === 'workspace_release') assert(isRecord(value) && value.released === request.gate, 'Wrong workspace gate');
    if (request.op === 'native_provider_stage') assert(isRecord(value) && value.stage === request.stage, 'Wrong provider stage');
    if (request.op === 'reset') assert(request.kind === context.kind, 'Wrong fixture reset kind');
    return results[request.op](value);
  }
  return { ready, result };
}
export type ExactRequest<R extends FixtureRequest> = R & NoInfer<Record<Exclude<keyof R, keyof ByOp[R['op']]>, never>>;
export type Caller<Allowed extends FixtureRequest, K extends FixtureKind> =
  <const R extends Allowed>(request: ExactRequest<R>) => Promise<ResultByOp<K>[R['op']]>;
