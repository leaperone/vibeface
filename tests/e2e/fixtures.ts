import assert from 'node:assert/strict';
import { lstat, readFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import { test as base } from '@e2e-dev/web';
import publicBootstrap from '../../config/public-bootstrap.json' with { type: 'json' };

const root = resolve(import.meta.dirname, '../..');
const artifacts = resolve(root, '.artifacts/e2e');
base.beforeEach(async ({ browser }) => {
  await browser.route(`${publicBootstrap.https_base}/**`, route => route.abort());
  await browser.addInitScript(() => {
    if (location.protocol !== 'http:' && location.protocol !== 'https:') return;
    const key = 'vibeface.bootstrap-public-mode';
    if (localStorage.getItem(key) === null) localStorage.setItem(key, 'off');
  });
});
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
  const inside = relative(artifacts, resolve(directory));
  assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Fixture data remains in this task');
  return { url: url.origin, peer, directory };
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

type BootstrapInfo = {
  site: string;
  invitation: string;
  issuer: string;
  member: NodeInfo;
  gateway: NodeInfo;
  via: 'udp' | 'https';
  catalogReads: number;
  memberHasOperatorToken: boolean;
};
type BootstrapFixture = BootstrapInfo & {
  pairing(): Promise<{ peer: string; code: string }>;
  approve(pairing: { peer: string; code: string }): Promise<void>;
  inspect(): Promise<BootstrapInfo>;
};
function bootstrapInfo(value: unknown): BootstrapInfo {
  const fields = record(value), members = nodes(value), status = record(fields.bootstrap);
  const site = new URL(text(fields.site));
  assert(site.protocol === 'http:' && site.hostname === '127.0.0.1' && site.port);
  const member = node(members.member), gateway = node(members.entry), discovery = node(members.discovery);
  const issuer = text(fields.issuer);
  assert.equal(issuer, discovery.peer);
  assert.equal(status.state, 'admitted');
  assert.equal(status.gateway_url, gateway.url);
  assert(status.via === 'udp' || status.via === 'https');
  assert(typeof fields.member_has_operator_token === 'boolean');
  assert(typeof fields.catalog_reads === 'number' && Number.isSafeInteger(fields.catalog_reads) && fields.catalog_reads >= 0);
  return { site: site.origin, invitation: text(fields.invitation), issuer, member, gateway, via: status.via,
    catalogReads: fields.catalog_reads, memberHasOperatorToken: fields.member_has_operator_token };
}
function bootstrapFixture(kind: 'bootstrap' | 'bootstrap-http') {
  return base.extend<{ bootstrap: BootstrapFixture }>({
    bootstrap: async ({ app, browser }, use) => {
      const call = await control(app.baseUrl);
      try {
        await call({ op: 'reset', kind });
        const info = bootstrapInfo(await call({ op: 'bootstrap_info' }));
        assert(app.baseUrl);
        assert.equal(info.site, new URL(app.baseUrl).origin);
        assert.notEqual(info.gateway.url, info.site, 'Only the signed catalogue names the working gateway');
        await browser.addInitScript(({ base, issuer }) => {
          if (location.protocol !== 'http:' && location.protocol !== 'https:') return;
          localStorage.setItem('vibeface.bootstrap-public-mode', 'custom');
          localStorage.setItem('vibeface.bootstrap-public-custom-base', base);
          localStorage.setItem('vibeface.bootstrap-public-custom-issuer', issuer);
        }, { base: `${info.site}/bootstrap`, issuer: info.issuer });
        await use({ ...info,
          async pairing() {
            const result = record(await call({ op: 'bootstrap_pairing' }));
            const peer = text(result.peer), code = text(result.code);
            assert(/^[a-f0-9]{64}$/.test(peer) && /^[0-9]{6}$/.test(code));
            return { peer, code };
          },
          async approve(pairing) {
            const result = record(await call({ op: 'bootstrap_approve', ...pairing }));
            assert.equal(result.approved, true);
          },
          async inspect() { return bootstrapInfo(await call({ op: 'bootstrap_info' })); },
        });
      } finally { await call({ op: 'stop' }); }
    },
  });
}
export const bootstrapTest = bootstrapFixture('bootstrap');
export const httpBootstrapTest = bootstrapFixture('bootstrap-http');
