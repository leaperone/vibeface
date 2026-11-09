import assert from 'node:assert/strict';
import { lstat, readFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import { test as base } from '@e2e-dev/web';

const root = resolve(import.meta.dirname, '../..');
const artifacts = resolve(root, '.artifacts/e2e');
export type NodeInfo = { url: string; entry: string | null; peer: string; directory: string };
/** A Node whose owner access link a browser opens; `url` is that link's origin. */
export type OwnerNode = NodeInfo & { entry: string };
type Role = 'source' | 'receiver' | 'provider';
type Control = (request: Record<string, unknown>) => Promise<unknown>;
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function record(value: unknown): Record<string, unknown> {
  assert(isRecord(value), 'Expected fixture object');
  return value;
}
function text(value: unknown): string { assert(typeof value === 'string' && value.length > 0, 'Expected fixture text'); return value; }
function node(value: unknown): NodeInfo {
  const fields = record(value);
  const url = new URL(text(fields.url));
  assert(url.protocol === 'http:' && url.hostname === '127.0.0.1' && url.port && url.pathname === '/' && !url.search && !url.hash, 'Fixture uses literal loopback');
  const entry = fields.entry === null ? null : new URL(text(fields.entry));
  assert(entry === null || entry.origin === url.origin && entry.pathname === '/' && !entry.search && /^#bootstrap=[A-Za-z0-9_-]+$/.test(entry.hash), 'The owner access link opens this Node with its bootstrap entry in the fragment');
  const peer = text(fields.peer), directory = text(fields.directory);
  assert(/^[a-f0-9]{64}$/.test(peer), 'Fixture peer is a Node identity');
  const inside = relative(artifacts, resolve(directory));
  assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Fixture data remains in this task');
  return { url: url.origin, entry: entry === null ? null : entry.href, peer, directory };
}
function owner(info: NodeInfo): OwnerNode {
  assert(info.entry !== null, 'A browser Node publishes its owner access link');
  return { ...info, entry: info.entry };
}
function nodes(value: unknown) { return record(record(value).nodes); }
async function control(baseUrl: string | undefined): Promise<Control> {
  assert(baseUrl, 'The e2e app has an allocated URL');
  const port = new URL(baseUrl).port;
  assert(/^[0-9]+$/.test(port));
  const path = resolve(artifacts, `control/${port}.json`);
  const stat = await lstat(path);
  assert(stat.isFile() && !stat.isSymbolicLink() && (stat.mode & 0o077) === 0, 'Fixture control material is private');
  const fields = record(JSON.parse(await readFile(path, 'utf8')));
  const url = new URL(text(fields.url)), token = text(fields.token);
  assert(url.protocol === 'http:' && url.hostname === '127.0.0.1' && url.port && url.pathname === '/' && !url.search && !url.hash, 'Control stays on literal loopback');
  assert(/^[a-f0-9]{64}$/.test(token));
  return async request => {
    const response = await fetch(url, { method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' }, body: JSON.stringify(request), signal: AbortSignal.timeout(90000) });
    if (!response.ok) {
      const failure = record(await response.json());
      assert.fail(`Fixture ${text(request.op)} returned ${response.status}: ${text(failure.error)}`);
    }
    return response.json();
  };
}
export const test = base.extend<{ node: OwnerNode }>({
  node: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const value = nodes(await call({ op: 'reset', kind: 'single' }));
      const source = owner(node(value.source));
      assert(app.baseUrl);
      assert.equal(source.url, new URL(app.baseUrl).origin);
      await use(source);
    } finally { await call({ op: 'stop' }); }
  },
});
export const cursorTest = base.extend<{ cursor: {
  node: OwnerNode;
  login(): Promise<void>;
  inspect(): Promise<{ selected_official: boolean; bot_enabled: boolean; execution: string[]; prompts: number }>;
} }>({
  cursor: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const value = nodes(await call({ op: 'reset', kind: 'cursor' }));
      await use({ node: owner(node(value.source)),
        async login() { assert.equal(record(await call({ op: 'cursor_login' })).signed_in, true); },
        async inspect() {
          const value = record(await call({ op: 'cursor_inspect' }));
          assert(typeof value.selected_official === 'boolean' && typeof value.bot_enabled === 'boolean');
          assert(Array.isArray(value.execution) && value.execution.every((item): item is string => typeof item === 'string'));
          assert(typeof value.prompts === 'number' && Number.isSafeInteger(value.prompts) && value.prompts >= 0);
          return { selected_official: value.selected_official, bot_enabled: value.bot_enabled, execution: value.execution, prompts: value.prompts };
        },
      });
    } finally { await call({ op: 'stop' }); }
  },
});
export type DeliveryFixture = {
  source: OwnerNode; receiver: OwnerNode; provider: NodeInfo;
  restart(role: Role, crash?: boolean): Promise<void>;
  inspect(): Promise<{ wallet_files: number; mesh_records: number; chain_rpc_calls: number }>;
};
export const deliveryTest = base.extend<{ network: DeliveryFixture }>({
  network: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const value = nodes(await call({ op: 'reset', kind: 'delivery' }));
      const source = owner(node(value.source)), receiver = owner(node(value.receiver)), provider = node(value.provider);
      assert(app.baseUrl);
      assert.equal(source.url, new URL(app.baseUrl).origin);
      assert(new Set([source.peer, receiver.peer, provider.peer]).size === 3, 'Each Node has its own identity');
      await use({ source, receiver, provider,
        async restart(role, crash = false) {
          const restarted = nodes(await call({ op: 'restart', role, crash }));
          assert.deepEqual(node(restarted[role]), { source, receiver, provider }[role], 'Restart preserves the exact Node');
        },
        async inspect() {
          const result = record(await call({ op: 'inspect' }));
          const integer = (value: unknown) => { assert(typeof value === 'number' && Number.isSafeInteger(value) && value >= 0); return value; };
          return { wallet_files: integer(result.wallet_files), mesh_records: integer(result.mesh_records), chain_rpc_calls: integer(result.chain_rpc_calls) };
        },
      });
    } finally { await call({ op: 'stop' }); }
  },
});

type WorkspaceGate = 'taskA' | 'taskB' | 'main' | 'channel';
export type WorkspaceEvidence = {
  invocations: { label: string; conversation_id: string; trigger: string; thread_id: string; messages: string[] }[];
  steers: { message_id: string; thread_id: string; turn_id: string; text: string }[];
  starts: { thread_id: string; turn_id: string }[];
  interrupts: { thread_id: string; turn_id: string }[];
  turns: { thread_id: string; turn_id: string; state: string }[];
};
type WorkspaceFixture = {
  source: OwnerNode;
  gates: Record<WorkspaceGate, string>;
  release(gate: WorkspaceGate): Promise<void>;
  inspect(): Promise<WorkspaceEvidence>;
};
function rows(value: unknown): Record<string, unknown>[] {
  assert(Array.isArray(value), 'Expected fixture rows');
  return value.map(record);
}
function workspaceEvidence(value: unknown): WorkspaceEvidence {
  const fields = record(value);
  const turn = (row: Record<string, unknown>) => ({ thread_id: text(row.thread_id), turn_id: text(row.turn_id) });
  return {
    invocations: rows(fields.invocations).map(row => {
      assert(Array.isArray(row.messages) && row.messages.every(item => typeof item === 'string'), 'Fixture context contains text only');
      return { label: text(row.label), conversation_id: text(row.conversation_id), trigger: text(row.trigger), thread_id: text(row.thread_id), messages: row.messages };
    }),
    steers: rows(fields.steers).map(row => ({ ...turn(row), message_id: text(row.message_id), text: text(row.text) })),
    starts: rows(fields.starts).map(turn),
    interrupts: rows(fields.interrupts).map(turn),
    turns: rows(fields.turns).map(row => ({ ...turn(row), state: text(row.state) })),
  };
}
export const workspaceTest = base.extend<{ workspace: WorkspaceFixture }>({
  workspace: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const ready = record(await call({ op: 'reset', kind: 'workspace' }));
      const source = owner(node(nodes(ready).source));
      assert(app.baseUrl);
      assert.equal(source.url, new URL(app.baseUrl).origin);
      const paths = record(ready.gates);
      const gate = (name: WorkspaceGate) => {
        const path = text(paths[name]);
        const inside = relative(artifacts, resolve(path));
        assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Controlled gates remain in this task');
        return path;
      };
      await use({ source, gates: { taskA: gate('taskA'), taskB: gate('taskB'), main: gate('main'), channel: gate('channel') },
        async release(gate) {
          assert.deepEqual(await call({ op: 'workspace_release', gate }), { released: gate });
        },
        async inspect() { return workspaceEvidence(await call({ op: 'workspace_inspect' })); },
      });
    } finally { await call({ op: 'stop' }); }
  },
});

type MeshInfo = { invitation: string; member: NodeInfo; gateway: NodeInfo };
type MeshFixture = MeshInfo & {
  pairing(): Promise<{ peer: string; code: string }>;
  approve(pairing: { peer: string; code: string }): Promise<void>;
};
function meshInfo(value: unknown): MeshInfo {
  const fields = record(value), members = nodes(value), mesh = record(fields.mesh);
  const member = node(members.member), gateway = node(members.public);
  assert.equal(mesh.enabled, true);
  assert.match(text(mesh.network), /^t-[0-9a-f]{12}$/, 'The fixture mesh has its own random name');
  assert(Array.isArray(mesh.bootstrap) && mesh.bootstrap.length === 1, 'The member has one bootstrap node');
  assert.match(text(mesh.bootstrap[0]), new RegExp(`^vibeface://node/${gateway.peer}\\?addr=127\\.0\\.0\\.1:[0-9]+$`),
    'The member configures nothing but the page Node as its bootstrap node');
  assert.deepEqual(fields.relays, [], 'The member configures no relay');
  const announced = fields.announced_relays;
  assert(Array.isArray(announced) && announced.length === 1, 'The page Node announces its volunteer relay on the mesh');
  return { invitation: text(fields.invitation), member, gateway };
}
export const meshTest = base.extend<{ mesh: MeshFixture }>({
  mesh: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    let primary: unknown;
    try {
      await call({ op: 'reset', kind: 'mesh' });
      const info = meshInfo(await call({ op: 'mesh_info' }));
      assert(app.baseUrl);
      assert.equal(info.gateway.url, new URL(app.baseUrl).origin, 'The page is served by the public node');
      await use({ ...info,
        async pairing() {
          const result = record(await call({ op: 'mesh_pairing' }));
          const peer = text(result.peer), code = text(result.code);
          assert(/^[a-f0-9]{64}$/.test(peer) && /^[0-9]{6}$/.test(code));
          return { peer, code };
        },
        async approve(pairing) {
          const result = record(await call({ op: 'mesh_approve', ...pairing }));
          assert.equal(result.approved, true);
        },
      });
    } catch (error) {
      primary = error;
      throw error;
    } finally {
      try { await call({ op: 'stop' }); }
      catch (cleanup) {
        if (primary !== undefined) throw new AggregateError([primary, cleanup], `${String(primary)}; cleanup also failed: ${String(cleanup)}`);
        throw cleanup;
      }
    }
  },
});
