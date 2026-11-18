/** The fixture RPC wire contract. External JSON is parsed here before callers use it. */

export type FixtureKind = 'single' | 'cursor' | 'delivery' | 'workspace' | 'mesh';
export type FixtureRole = 'source' | 'receiver' | 'provider' | 'public' | 'member';
export type WorkspaceGate = 'taskA' | 'taskB' | 'main' | 'channel';
export type ProviderStage = 'signed_out' | 'ready' | 'catalog_unknown';

export type FixtureNode = { url: string; entry: string | null; peer: string; directory: string };
export type FixtureReady = {
  run: string;
  nodes: Record<string, FixtureNode>;
  gates?: Record<WorkspaceGate, string>;
};
export type FixtureRequest =
  | { op: 'reset'; kind: FixtureKind }
  | { op: 'stop' }
  | { op: 'restart'; role: FixtureRole; crash?: boolean }
  | { op: 'inspect' }
  | { op: 'cursor_login' }
  | { op: 'cursor_inspect' }
  | { op: 'workspace_release'; gate: WorkspaceGate }
  | { op: 'workspace_inspect' }
  | { op: 'mesh_info' }
  | { op: 'mesh_pairing' }
  | { op: 'mesh_approve'; peer: string; code: string }
  | { op: 'native_setup'; invitation_file: string }
  | { op: 'workspace_native_setup'; invitation_file: string }
  | { op: 'native_join' }
  | { op: 'native_approve'; peer: string; code: string }
  | { op: 'native_provider_setup' }
  | { op: 'native_provider_clipboard' }
  | { op: 'native_provider_login'; command: string }
  | { op: 'native_provider_stage'; stage: ProviderStage }
  | { op: 'native_provider_queue' }
  | { op: 'native_provider_inspect' }
  | { op: 'native_send'; name: string; content_base64: string };

export type FixtureInspect = { wallet_files: number; mesh_records: number; chain_rpc_calls: number };
export type CursorInspect = { selected_official: boolean; bot_enabled: boolean; execution: string[]; prompts: number };
export type WorkspaceEvidence = {
  invocations: { label: string; conversation_id: string; trigger: string; thread_id: string; messages: string[] }[];
  steers: { message_id: string; thread_id: string; turn_id: string; text: string }[];
  starts: { thread_id: string; turn_id: string }[];
  interrupts: { thread_id: string; turn_id: string }[];
  turns: { thread_id: string; turn_id: string; state: string }[];
  inputs?: { message_id: string; conversation_id: string; target: Record<string, unknown>; text: string; deferred: boolean; cancelled: boolean }[];
};
export type MeshInfo = FixtureReady & { invitation: string; mesh: Record<string, unknown>; relays: unknown[]; announced_relays: unknown[] };
export type ProviderInspection = {
  manager: { provider: string; model: string; reasoning_effort: string | null } | null;
  roles: string[];
  execution_model: string;
  setup: { directory: string; command: string };
  login_count: number;
  app_server_link_state: string;
  readiness: string;
  catalog: string;
  authentication: string;
  checked_at_ms: number | null;
  jobs: { trigger: string; state: string }[];
  deferred_input: { message_id: string; target: Record<string, unknown>; deferred: boolean; cancelled: boolean } | null;
  total_invocations: number;
  running_invocations: number;
  deferred_invocations: number;
};

export type ResultByOp = {
  reset: FixtureReady;
  stop: { stopped: true };
  restart: FixtureReady;
  inspect: FixtureInspect;
  cursor_login: { signed_in: true };
  cursor_inspect: CursorInspect;
  workspace_release: { released: WorkspaceGate };
  workspace_inspect: WorkspaceEvidence;
  mesh_info: MeshInfo;
  mesh_pairing: { peer: string; code: string };
  mesh_approve: { approved: true };
  native_setup: FixtureReady;
  workspace_native_setup: FixtureReady;
  native_join: { peer: string; code: string };
  native_approve: { approved: true };
  native_provider_setup: { configured: true };
  native_provider_clipboard: { pasted: true };
  native_provider_login: { executed: true; private_root_verified: true; controlled_bin_verified: true; login_count: number };
  native_provider_stage: { stage: ProviderStage };
  native_provider_queue: { bot_id: string; conversation_id: string; running_trigger: string; deferred_message_id: string };
  native_provider_inspect: ProviderInspection;
  native_send: { order_id: string };
};

export type FixtureOperationError = { error: string; frames: string[] };
export type FixtureResponse = { id: number; ok: true; result: unknown } | ({ id: number; ok: false } & FixtureOperationError);
export type FixtureRequestEnvelope = { id: number; request: FixtureRequest };

export function isFixtureRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function fail(message: string): never { throw new Error(message); }
function record(value: unknown, label = 'fixture object'): Record<string, unknown> {
  if (!isFixtureRecord(value)) fail(`Expected ${label}`);
  return value;
}
function exact(value: Record<string, unknown>, keys: readonly string[], label = 'fixture object'): void {
  const actual = Object.keys(value);
  if (actual.length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) fail(`Invalid ${label} fields`);
}
function text(value: unknown, label = 'fixture text'): string {
  if (typeof value !== 'string' || value.length === 0) fail(`Expected ${label}`);
  return value;
}
function stringValue(value: unknown, label = 'fixture string'): string {
  if (typeof value !== 'string') fail(`Expected ${label}`);
  return value;
}
function optionalText(value: unknown, label: string): string | null {
  if (value === null) return null;
  return text(value, label);
}
function bool(value: unknown, label = 'fixture boolean'): boolean {
  if (typeof value !== 'boolean') fail(`Expected ${label}`);
  return value;
}
function integer(value: unknown, label = 'fixture integer'): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) fail(`Expected ${label}`);
  return value;
}
function oneOf<T extends string>(value: unknown, values: readonly T[], label: string): T {
  if (typeof value !== 'string' || !values.includes(value as T)) fail(`Invalid ${label}`);
  return value as T;
}
function array<T>(value: unknown, parse: (value: unknown) => T, label: string): T[] {
  if (!Array.isArray(value)) fail(`Expected ${label}`);
  return value.map(parse);
}
function node(value: unknown): FixtureNode {
  const fields = record(value, 'fixture Node');
  exact(fields, ['url', 'entry', 'peer', 'directory'], 'fixture Node');
  const url = text(fields.url, 'Node URL');
  let parsed: URL;
  try { parsed = new URL(url); } catch { fail('Invalid Node URL'); }
  if (parsed.protocol !== 'http:' || parsed.hostname !== '127.0.0.1' || !parsed.port || parsed.pathname !== '/' || parsed.username || parsed.password || parsed.search || parsed.hash) fail('Fixture uses literal loopback URL');
  const entry = optionalText(fields.entry, 'fixture entry');
  if (entry !== null) {
    let parsedEntry: URL;
    try { parsedEntry = new URL(entry); } catch { fail('Invalid Node entry'); }
    if (parsedEntry.origin !== parsed.origin || parsedEntry.pathname !== '/' || parsedEntry.search || !/^#bootstrap=[A-Za-z0-9_-]+$/.test(parsedEntry.hash)) fail('Invalid Node owner entry');
  }
  const peer = text(fields.peer, 'Node peer');
  if (!/^[a-f0-9]{64}$/.test(peer)) fail('Invalid Node peer');
  const directory = text(fields.directory, 'Node directory');
  if (!directory.startsWith('/')) fail('Fixture directory must be absolute');
  return { url: parsed.origin, entry: entry === null ? null : new URL(entry).href, peer, directory };
}
function ready(value: unknown, kind?: FixtureKind): FixtureReady {
  const fields = record(value, 'fixture ready result');
  const keys = kind === 'workspace' ? ['run', 'nodes', 'gates'] : ['run', 'nodes'];
  if (kind === undefined) {
    const allowed = ['run', 'nodes', 'gates'];
    if (!Object.keys(fields).every(key => allowed.includes(key)) || !Object.hasOwn(fields, 'run') || !Object.hasOwn(fields, 'nodes')) fail('Invalid fixture ready result fields');
  } else exact(fields, keys, 'fixture ready result');
  const rawNodes = record(fields.nodes, 'fixture nodes');
  const inferredRoles = Object.keys(rawNodes);
  const roles = kind === 'delivery' ? ['source', 'receiver', 'provider'] : kind === 'mesh' ? ['public', 'member'] : kind === 'workspace' || kind === 'single' || kind === 'cursor' ? ['source'] : inferredRoles;
  const nodes: Record<string, FixtureNode> = {};
  if (kind !== undefined) exact(rawNodes, roles, 'fixture nodes');
  if (roles.length === 0 || !roles.every(role => inferredRoles.includes(role))) fail('Invalid fixture nodes');
  for (const role of roles) nodes[role] = node(rawNodes[role]);
  const run = text(fields.run, 'fixture run');
  if (!run.startsWith('/')) fail('Fixture run must be absolute');
  const result: FixtureReady = { run, nodes };
  if (kind === 'workspace' || (kind === undefined && Object.hasOwn(fields, 'gates'))) {
    const gates = record(fields.gates, 'workspace gates');
    exact(gates, ['taskA', 'taskB', 'main', 'channel'], 'workspace gates');
    result.gates = { taskA: text(gates.taskA), taskB: text(gates.taskB), main: text(gates.main), channel: text(gates.channel) };
  }
  return result;
}

const kinds = ['single', 'cursor', 'delivery', 'workspace', 'mesh'] as const;
const roles = ['source', 'receiver', 'provider', 'public', 'member'] as const;
const gates = ['taskA', 'taskB', 'main', 'channel'] as const;
const stages = ['signed_out', 'ready', 'catalog_unknown'] as const;
/** Parse a caller request, rejecting unknown keys and malformed variants. */
export function parseFixtureRequest(value: unknown): FixtureRequest {
  const fields = record(value, 'fixture request');
  if (typeof fields.op !== 'string') fail('Fixture request requires an operation');
  switch (fields.op) {
    case 'reset': exact(fields, ['op', 'kind'], 'reset request'); return { op: 'reset', kind: oneOf(fields.kind, kinds, 'fixture kind') };
    case 'stop': case 'inspect': case 'cursor_login': case 'cursor_inspect': case 'workspace_inspect': case 'mesh_info': case 'mesh_pairing': case 'native_join': case 'native_provider_setup': case 'native_provider_clipboard': case 'native_provider_queue': case 'native_provider_inspect':
      exact(fields, ['op'], `${fields.op} request`); return { op: fields.op } as FixtureRequest;
    case 'restart':
      if (!Object.hasOwn(fields, 'crash')) exact(fields, ['op', 'role'], 'restart request'); else exact(fields, ['op', 'role', 'crash'], 'restart request');
      return { op: 'restart', role: oneOf(fields.role, roles, 'fixture role'), ...(Object.hasOwn(fields, 'crash') ? { crash: bool(fields.crash, 'restart crash') } : {}) };
    case 'workspace_release': exact(fields, ['op', 'gate'], 'workspace release request'); return { op: 'workspace_release', gate: oneOf(fields.gate, gates, 'workspace gate') };
    case 'mesh_approve': case 'native_approve':
      exact(fields, ['op', 'peer', 'code'], `${fields.op} request`); return { op: fields.op, peer: text(fields.peer, 'peer'), code: text(fields.code, 'pairing code') } as FixtureRequest;
    case 'native_setup': case 'workspace_native_setup':
      exact(fields, ['op', 'invitation_file'], `${fields.op} request`); return { op: fields.op, invitation_file: text(fields.invitation_file, 'invitation file') } as FixtureRequest;
    case 'native_provider_login': exact(fields, ['op', 'command'], 'native provider login request'); return { op: 'native_provider_login', command: text(fields.command, 'login command') };
    case 'native_provider_stage': exact(fields, ['op', 'stage'], 'native provider stage request'); return { op: 'native_provider_stage', stage: oneOf(fields.stage, stages, 'provider stage') };
    case 'native_send': { exact(fields, ['op', 'name', 'content_base64'], 'native send request'); const name = text(fields.name, 'file name'); const content_base64 = text(fields.content_base64, 'base64 content'); if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(content_base64)) fail('Invalid base64 content'); return { op: 'native_send', name, content_base64 }; }
    default: fail('Unknown fixture operation');
  }
}

export function parseFixtureReset(request: unknown): Extract<FixtureRequest, { op: 'reset' }> {
  const parsed = parseFixtureRequest(request);
  if (parsed.op !== 'reset') fail('Fixture reset requires an operation and kind');
  return parsed;
}

function row(value: unknown, keys: readonly string[]): Record<string, unknown> {
  const fields = record(value, 'fixture row'); exact(fields, keys, 'fixture row'); return fields;
}
function turn(value: unknown): { thread_id: string; turn_id: string } {
  const fields = row(value, ['thread_id', 'turn_id']); return { thread_id: text(fields.thread_id), turn_id: text(fields.turn_id) };
}
function evidence(value: unknown): WorkspaceEvidence {
  const fields = record(value, 'workspace inspection');
  const keys = Object.keys(fields); const allowed = ['invocations', 'steers', 'starts', 'interrupts', 'turns', 'inputs'];
  if (!keys.every(key => allowed.includes(key)) || !['invocations', 'steers', 'starts', 'interrupts', 'turns'].every(key => keys.includes(key))) fail('Invalid workspace inspection fields');
  const invocations = array(fields.invocations, item => { const r = row(item, ['label', 'conversation_id', 'trigger', 'thread_id', 'messages']); return { label: text(r.label), conversation_id: text(r.conversation_id), trigger: text(r.trigger), thread_id: text(r.thread_id), messages: array(r.messages, x => text(x), 'invocation messages') }; }, 'invocations');
  const steers = array(fields.steers, item => { const r = row(item, ['message_id', 'thread_id', 'turn_id', 'text']); return { message_id: text(r.message_id), thread_id: text(r.thread_id), turn_id: text(r.turn_id), text: text(r.text) }; }, 'steers');
  const starts = array(fields.starts, turn, 'starts');
  const interrupts = array(fields.interrupts, turn, 'interrupts');
  const turns = array(fields.turns, item => { const r = row(item, ['thread_id', 'turn_id', 'state']); return { ...turn(r), state: text(r.state) }; }, 'turns');
  let inputs: WorkspaceEvidence['inputs'];
  if (Object.hasOwn(fields, 'inputs')) inputs = array(fields.inputs, item => { const r = row(item, ['message_id', 'conversation_id', 'target', 'text', 'deferred', 'cancelled']); return { message_id: text(r.message_id), conversation_id: text(r.conversation_id), target: record(r.target), text: text(r.text), deferred: bool(r.deferred), cancelled: bool(r.cancelled) }; }, 'inputs');
  return { invocations, steers, starts, interrupts, turns, ...(inputs ? { inputs } : {}) };
}
function resultFor(request: FixtureRequest, value: unknown): ResultByOp[FixtureRequest['op']] {
  const op = request.op;
  switch (op) {
    case 'reset': return ready(value, request.kind) as ResultByOp[typeof op];
    case 'restart': return ready(value) as ResultByOp[typeof op];
    case 'native_setup': return ready(value, 'delivery') as ResultByOp[typeof op];
    case 'workspace_native_setup': return ready(value, 'workspace') as ResultByOp[typeof op];
    case 'stop': { const r = row(value, ['stopped']); if (r.stopped !== true) fail('Invalid stop result'); return { stopped: true } as ResultByOp[typeof op]; }
    case 'inspect': { const r = row(value, ['wallet_files', 'mesh_records', 'chain_rpc_calls']); return { wallet_files: integer(r.wallet_files), mesh_records: integer(r.mesh_records), chain_rpc_calls: integer(r.chain_rpc_calls) } as ResultByOp[typeof op]; }
    case 'cursor_login': { const r = row(value, ['signed_in']); if (r.signed_in !== true) fail('Invalid cursor login result'); return { signed_in: true } as ResultByOp[typeof op]; }
    case 'cursor_inspect': { const r = row(value, ['selected_official', 'bot_enabled', 'execution', 'prompts']); return { selected_official: bool(r.selected_official), bot_enabled: bool(r.bot_enabled), execution: array(r.execution, x => text(x), 'execution'), prompts: integer(r.prompts) } as ResultByOp[typeof op]; }
    case 'workspace_release': { const r = row(value, ['released']); return { released: oneOf(r.released, gates, 'workspace gate') } as ResultByOp[typeof op]; }
    case 'workspace_inspect': return evidence(value) as ResultByOp[typeof op];
    case 'mesh_info': { const r = record(value, 'mesh info'); exact(r, ['run', 'nodes', 'invitation', 'mesh', 'relays', 'announced_relays'], 'mesh info'); const base = ready({ run: r.run, nodes: r.nodes }, 'mesh'); const mesh = row(r.mesh, ['enabled', 'network', 'bootstrap', 'allow_private', 'advertise']); if (mesh.enabled !== true || !/^t-[0-9a-f]{12}$/.test(text(mesh.network, 'mesh network')) || !Array.isArray(mesh.bootstrap) || mesh.bootstrap.length !== 1 || !mesh.bootstrap.every(item => typeof item === 'string') || !new RegExp(`^vibeface://node/${base.nodes.public.peer}\\?addr=127\\.0\\.0\\.1:[0-9]+$`).test(mesh.bootstrap[0]) || mesh.allow_private !== true || !Array.isArray(mesh.advertise) || !mesh.advertise.every(item => typeof item === 'string')) fail('Invalid mesh configuration'); const relays = array(r.relays, x => text(x), 'relays'); const announced = array(r.announced_relays, x => text(x), 'announced relays'); if (relays.length !== 0 || announced.length !== 1) fail('Invalid mesh relay configuration'); return { ...base, invitation: text(r.invitation), mesh, relays, announced_relays: announced } as ResultByOp[typeof op]; }
    case 'mesh_pairing': case 'native_join': { const r = row(value, ['peer', 'code']); return { peer: text(r.peer), code: text(r.code) } as ResultByOp[typeof op]; }
    case 'mesh_approve': case 'native_approve': { const r = row(value, ['approved']); if (r.approved !== true) fail('Invalid approval result'); return { approved: true } as ResultByOp[typeof op]; }
    case 'native_provider_setup': { const r = row(value, ['configured']); if (r.configured !== true) fail('Invalid provider setup result'); return { configured: true } as ResultByOp[typeof op]; }
    case 'native_provider_clipboard': { const r = row(value, ['pasted']); if (r.pasted !== true) fail('Invalid provider clipboard result'); return { pasted: true } as ResultByOp[typeof op]; }
    case 'native_provider_login': { const r = row(value, ['executed', 'private_root_verified', 'controlled_bin_verified', 'login_count']); if (r.executed !== true || r.private_root_verified !== true || r.controlled_bin_verified !== true) fail('Invalid provider login result'); return { executed: true, private_root_verified: true, controlled_bin_verified: true, login_count: integer(r.login_count) } as ResultByOp[typeof op]; }
    case 'native_provider_stage': { const r = row(value, ['stage']); return { stage: oneOf(r.stage, stages, 'provider stage') } as ResultByOp[typeof op]; }
    case 'native_provider_queue': { const r = row(value, ['bot_id', 'conversation_id', 'running_trigger', 'deferred_message_id']); return { bot_id: text(r.bot_id), conversation_id: text(r.conversation_id), running_trigger: text(r.running_trigger), deferred_message_id: text(r.deferred_message_id) } as ResultByOp[typeof op]; }
    case 'native_provider_inspect': { const r = row(value, ['manager', 'roles', 'execution_model', 'setup', 'login_count', 'app_server_link_state', 'readiness', 'catalog', 'authentication', 'checked_at_ms', 'jobs', 'deferred_input', 'total_invocations', 'running_invocations', 'deferred_invocations']); const manager = r.manager === null ? null : (() => { const m = row(r.manager, ['provider', 'model', 'reasoning_effort']); return { provider: text(m.provider), model: stringValue(m.model, 'model'), reasoning_effort: m.reasoning_effort === null ? null : stringValue(m.reasoning_effort, 'reasoning effort') }; })(); const setup = row(r.setup, ['directory', 'command']); const deferred = r.deferred_input === null ? null : (() => { const d = row(r.deferred_input, ['message_id', 'target', 'deferred', 'cancelled']); return { message_id: text(d.message_id), target: record(d.target), deferred: bool(d.deferred), cancelled: bool(d.cancelled) }; })(); return { manager, roles: array(r.roles, x => text(x), 'provider roles'), execution_model: stringValue(r.execution_model, 'execution model'), setup: { directory: text(setup.directory), command: text(setup.command) }, login_count: integer(r.login_count), app_server_link_state: text(r.app_server_link_state), readiness: text(r.readiness), catalog: text(r.catalog), authentication: text(r.authentication), checked_at_ms: r.checked_at_ms === null ? null : integer(r.checked_at_ms, 'checked_at_ms'), jobs: array(r.jobs, x => { const j = row(x, ['trigger', 'state']); return { trigger: text(j.trigger), state: text(j.state) }; }, 'jobs'), deferred_input: deferred, total_invocations: integer(r.total_invocations), running_invocations: integer(r.running_invocations), deferred_invocations: integer(r.deferred_invocations) } as ResultByOp[typeof op]; }
    case 'native_send': { const r = row(value, ['order_id']); return { order_id: text(r.order_id) } as ResultByOp[typeof op]; }
    default: { const exhaustive: never = op; return exhaustive; }
  }
}

export function parseFixtureResult<O extends FixtureRequest['op']>(request: Extract<FixtureRequest, { op: O }>, value: unknown): ResultByOp[O] {
  return resultFor(request, value) as ResultByOp[O];
}

export function parseFixtureReady(value: unknown, kind: FixtureKind): FixtureReady { return ready(value, kind); }
export function parseFixtureOperationError(value: unknown): FixtureOperationError {
  const fields = row(value, ['error', 'frames']);
  return { error: text(fields.error), frames: array(fields.frames, x => text(x), 'error frames') };
}
