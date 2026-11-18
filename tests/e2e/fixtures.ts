import assert from 'node:assert/strict';
import { open } from 'node:fs/promises';
import { constants } from 'node:fs';
import { resolve } from 'node:path';
import { test as base } from '@e2e-dev/web';
import {
  createFixtureContract, loopbackOrigin, parseFixtureRequest,
  type ExactRequest, type Caller, type FixtureKind, type FixtureRequest, type NodeInfo, type OwnerNode, type ResultByOp, type WebRequest, type WorkspaceGate,
} from '../support/fixture-contract';
import { withControlledFixture } from '../support/fixture-lifecycle';

export type { NodeInfo, OwnerNode };
export type WorkspaceEvidence = ResultByOp<'workspace'>['workspace_inspect'];
const artifacts = resolve(import.meta.dirname, '../../.artifacts/e2e');

function fields(value: unknown, keys: string[]): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) &&
    Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}
async function responseJSON(response: Response): Promise<unknown> {
  const reader = response.body?.getReader();
  assert(reader, 'Fixture control returned an empty response');
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) break;
      length += chunk.value.byteLength;
      assert(length <= 64 * 1024, 'Fixture control response exceeds 64 KiB');
      chunks.push(chunk.value);
    }
  } finally {
    await reader.cancel();
  }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks)));
}
async function control<K extends FixtureKind>(baseUrl: string | undefined, kind: K): Promise<Caller<WebRequest<K>, K>> {
  assert(baseUrl, 'The e2e app has an allocated URL');
  const page = new URL(baseUrl);
  assert(/^[0-9]+$/.test(page.port));
  const path = resolve(artifacts, `control/${page.port}.json`);
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  let credentials: unknown;
  try {
    const stat = await file.stat();
    assert(stat.isFile() && (stat.mode & 0o077) === 0 && stat.size <= 4096, 'Fixture control material is private and bounded');
    credentials = JSON.parse(await file.readFile('utf8'));
  } finally { await file.close(); }
  assert(fields(credentials, ['url', 'token']) && typeof credentials.token === 'string' && /^[a-f0-9]{64}$/.test(credentials.token));
  const url = loopbackOrigin(credentials.url), token = credentials.token;
  const contract = createFixtureContract({ kind, artifacts, origin: page.origin });
  return async function call<R extends FixtureRequest>(request: ExactRequest<R>) {
    const parsed = parseFixtureRequest<R>(request, 'web', kind);
    const response = await fetch(url, { method: 'POST', redirect: 'error',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify(parsed), signal: AbortSignal.timeout(90000) });
    if (!response.ok) {
      if (!response.headers.get('content-type')?.includes('application/json')) assert.fail(`Fixture ${request.op} returned ${response.status}`);
      const failure = await responseJSON(response);
      assert(fields(failure, ['error']) && typeof failure.error === 'string' && failure.error.length > 0);
      assert.fail(`Fixture ${request.op} returned ${response.status}: ${failure.error}`);
    }
    return contract.result<R>(parsed, await responseJSON(response));
  };
}

export const test = base.extend<{ node: OwnerNode }>({
  node: async ({ app }, use) => {
    const call = await control(app.baseUrl, 'single');
    await withControlledFixture({
      run: async () => { await use((await call({ op: 'reset', kind: 'single' })).nodes.source); },
      stop: () => call({ op: 'stop' }),
    });
  },
});
export const cursorTest = base.extend<{ cursor: {
  node: OwnerNode;
  login(): Promise<void>;
  inspect(): Promise<ResultByOp<'cursor'>['cursor_inspect']>;
} }>({
  cursor: async ({ app }, use) => {
    const call = await control(app.baseUrl, 'cursor');
    await withControlledFixture({
      run: async () => {
        const { nodes } = await call({ op: 'reset', kind: 'cursor' });
        await use({ node: nodes.source,
          async login() { await call({ op: 'cursor_login' }); },
          inspect: () => call({ op: 'cursor_inspect' }),
        });
      },
      stop: () => call({ op: 'stop' }),
    });
  },
});
export type DeliveryFixture = {
  source: OwnerNode; receiver: OwnerNode; provider: NodeInfo;
  restart(role: 'source' | 'receiver' | 'provider', crash?: boolean): Promise<void>;
  inspect(): Promise<ResultByOp<'delivery'>['inspect']>;
};
export const deliveryTest = base.extend<{ network: DeliveryFixture }>({
  network: async ({ app }, use) => {
    const call = await control(app.baseUrl, 'delivery');
    await withControlledFixture({
      run: async () => {
        const { nodes } = await call({ op: 'reset', kind: 'delivery' });
        await use({ ...nodes,
          async restart(role, crash = false) { await call({ op: 'restart', role, crash }); },
          inspect: () => call({ op: 'inspect' }),
        });
      },
      stop: () => call({ op: 'stop' }),
    });
  },
});

type WorkspaceFixture = {
  source: OwnerNode;
  gates: Record<WorkspaceGate, string>;
  release(gate: WorkspaceGate): Promise<void>;
  inspect(): Promise<WorkspaceEvidence>;
};
export const workspaceTest = base.extend<{ workspace: WorkspaceFixture }>({
  workspace: async ({ app }, use) => {
    const call = await control(app.baseUrl, 'workspace');
    await withControlledFixture({
      run: async () => {
        const { nodes, gates } = await call({ op: 'reset', kind: 'workspace' });
        await use({ source: nodes.source, gates,
          async release(gate) { await call({ op: 'workspace_release', gate }); },
          inspect: () => call({ op: 'workspace_inspect' }),
        });
      },
      stop: () => call({ op: 'stop' }),
    });
  },
});
type MeshFixture = {
  invitation: string; member: NodeInfo; gateway: NodeInfo;
  pairing(): Promise<ResultByOp<'mesh'>['mesh_pairing']>;
  approve(pairing: ResultByOp<'mesh'>['mesh_pairing']): Promise<void>;
};
export const meshTest = base.extend<{ mesh: MeshFixture }>({
  mesh: async ({ app }, use) => {
    const call = await control(app.baseUrl, 'mesh');
    await withControlledFixture({
      run: async () => {
        await call({ op: 'reset', kind: 'mesh' });
        const { invitation, nodes } = await call({ op: 'mesh_info' });
        await use({ invitation, member: nodes.member, gateway: nodes.public,
          pairing: () => call({ op: 'mesh_pairing' }),
          async approve(pairing) { await call({ op: 'mesh_approve', ...pairing }); },
        });
      },
      stop: () => call({ op: 'stop' }),
    });
  },
});
