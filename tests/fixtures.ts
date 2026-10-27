import assert from 'node:assert/strict';
import { lstat, readFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import { test as base } from '@e2e-dev/web';

export type NodeInfo = { url: string; peer: string; directory: string };
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
  const peer = text(fields.peer), directory = text(fields.directory);
  assert(/^[a-f0-9]{64}$/.test(peer), 'Fixture peer is a Node identity');
  const inside = relative(resolve('.artifacts/e2e'), resolve(directory));
  assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Fixture data remains in this task');
  return { url: url.origin, peer, directory };
}
function nodes(value: unknown) { return record(record(value).nodes); }
async function control(baseUrl: string | undefined): Promise<Control> {
  assert(baseUrl, 'The e2e app has an allocated URL');
  const port = new URL(baseUrl).port;
  assert(/^[0-9]+$/.test(port));
  const path = resolve(`.artifacts/e2e/control/${port}.json`);
  const stat = await lstat(path);
  assert(stat.isFile() && !stat.isSymbolicLink() && (stat.mode & 0o077) === 0, 'Fixture control material is private');
  const fields = record(JSON.parse(await readFile(path, 'utf8')));
  const url = new URL(text(fields.url)), token = text(fields.token);
  assert(url.protocol === 'http:' && url.hostname === '127.0.0.1' && url.port && url.pathname === '/' && !url.search && !url.hash, 'Control stays on literal loopback');
  assert(/^[a-f0-9]{64}$/.test(token));
  return async request => {
    const response = await fetch(url, { method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' }, body: JSON.stringify(request), signal: AbortSignal.timeout(30000) });
    assert(response.ok, `Fixture operation returned ${response.status}`);
    return response.json();
  };
}
export const test = base.extend<{ node: NodeInfo }>({
  node: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const value = nodes(await call({ op: 'reset', kind: 'single' }));
      const source = node(value.source);
      assert(app.baseUrl);
      assert.equal(source.url, new URL(app.baseUrl).origin);
      await use(source);
    } finally { await call({ op: 'stop' }); }
  },
});
export type DeliveryFixture = {
  source: NodeInfo; receiver: NodeInfo; provider: NodeInfo;
  restart(role: Role, crash?: boolean): Promise<void>;
  inspect(): Promise<{ wallet_files: number; pkarr_reads: number; pkarr_writes: number; chain_rpc_calls: number }>;
};
export const deliveryTest = base.extend<{ network: DeliveryFixture }>({
  network: async ({ app }, use) => {
    const call = await control(app.baseUrl);
    try {
      const value = nodes(await call({ op: 'reset', kind: 'delivery' }));
      const source = node(value.source), receiver = node(value.receiver), provider = node(value.provider);
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
          return { wallet_files: integer(result.wallet_files), pkarr_reads: integer(result.pkarr_reads), pkarr_writes: integer(result.pkarr_writes), chain_rpc_calls: integer(result.chain_rpc_calls) };
        },
      });
    } finally { await call({ op: 'stop' }); }
  },
});
