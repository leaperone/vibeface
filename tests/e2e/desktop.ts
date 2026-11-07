import { readPrivateControlJson } from '../../scripts/private-control.mjs';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { lstat, mkdir, open, readFile, rm, writeFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { test as base } from 'e2e';
import type { LauncherStatus } from '../../apps/desktop/src/shared/contract';
export type { LauncherStatus } from '../../apps/desktop/src/shared/contract';

const root = resolve(import.meta.dirname, '../..');
const artifacts = resolve(root, '.artifacts/e2e/desktop');
const screens = resolve(import.meta.dirname, '.artifacts/desktop/screens');

export type Page = { type: string; url: string; title: string; webSocketDebuggerUrl: string };
export type Inspection = {
  pid: number;
  profile: string;
  runtime: Record<string, unknown>;
  cores: number[];
  status: LauncherStatus;
  pages: Page[];
};
export type FixtureNode = { dataDir: string; peerPort: number; localPort: number; deviceId: string; running: boolean };
export type Desktop = {
  inspect(): Promise<Inspection>;
  status(): Promise<LauncherStatus>;
  invoke(method: string, ...args: unknown[]): Promise<LauncherStatus>;
  quit(): Promise<{ pid: number | null; owned: number[]; surviving: number[]; forced: boolean }>;
  closeWindows(): Promise<Array<{ url: string; visibility: string }>>;
  start(): Promise<Inspection>;
  createFixture(start?: boolean): Promise<FixtureNode>;
  startFixture(dataDir: string): Promise<Record<string, unknown>>;
  stopFixture(dataDir: string): Promise<Record<string, unknown>>;
  snapshotNode(dataDir: string): Promise<{ deviceId: string; status: string; version: string | null; coreVersion: string; values: Record<string, string | null> }>;
  serviceStatus(deviceId: string): Promise<Record<string, unknown>>;
  detachSourceRuntime(deviceId: string): Promise<Record<string, unknown>>;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function record(value: unknown): Record<string, unknown> {
  assert(isRecord(value), 'Expected a control object');
  return value;
}
function text(value: unknown): string {
  assert(typeof value === 'string' && value.length > 0, 'Expected control text');
  return value;
}
function identifier(value: unknown): number {
  assert(typeof value === 'number' && Number.isSafeInteger(value) && value > 0, 'Expected a process identifier');
  return value;
}
function identifiers(value: unknown): number[] {
  assert(Array.isArray(value), 'Expected process identifiers');
  return value.map(identifier);
}
function page(value: unknown): Page {
  const fields = record(value);
  return { type: text(fields.type), url: text(fields.url), title: typeof fields.title === 'string' ? fields.title : '', webSocketDebuggerUrl: text(fields.webSocketDebuggerUrl) };
}
function inspection(value: unknown): Inspection {
  const fields = record(value);
  const pages = fields.pages;
  assert(Array.isArray(pages), 'Expected the application pages');
  return {
    pid: identifier(fields.pid),
    profile: text(fields.profile),
    runtime: record(fields.runtime),
    cores: identifiers(fields.cores),
    status: record(fields.status) as LauncherStatus,
    pages: pages.map(page),
  };
}

/** A free loopback port the application serves its DevTools endpoint on. */
async function freePort(): Promise<number> {
  const server = createServer();
  try {
    await new Promise<void>((done, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', done); });
    const address = server.address();
    assert(address && typeof address === 'object', 'Expected an allocated port');
    return address.port;
  } finally {
    await new Promise<void>(done => server.close(() => done()));
  }
}

/** The private control API scripts/e2e-desktop.mjs publishes once the application window is up. */
async function control(port: number) {
  const path = resolve(artifacts, `control/${port}.json`);
  const fields = record(await readPrivateControlJson(path, { root: resolve(artifacts, 'control'), maxBytes: 4096 }));
  const url = new URL(text(fields.url));
  const token = text(fields.token);
  assert(url.protocol === 'http:' && url.hostname === '127.0.0.1' && url.port && url.pathname === '/' && !url.search && !url.hash, 'Control stays on literal loopback');
  assert(/^[a-f0-9]{64}$/.test(token));
  return async (request: Record<string, unknown>): Promise<unknown> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 120000);
    let response: Response;
    let body: unknown;
    try {
      response = await fetch(url, {
        method: 'POST',
        headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
        body: JSON.stringify(request),
        signal: controller.signal,
      });
      body = await response.json();
    } finally { clearTimeout(timer); }
    assert(response.ok, `The desktop control operation returned ${response.status}: ${isRecord(body) && typeof body.error === 'string' ? body.error : 'see driver log'}`);
    return body;
  };
}

/** One DevTools command on one existing Electron page, answered by value. */
async function command(target: Page, method: string, params: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  try {
    return await new Promise<Record<string, unknown>>((done, fail) => {
      const finish = (error?: unknown, value?: Record<string, unknown>) => {
        clearTimeout(timer);
        socket.removeEventListener('error', onError);
        socket.removeEventListener('open', onOpen);
        socket.removeEventListener('message', onMessage);
        socket.removeEventListener('close', onClose);
        if (error) fail(error); else done(value as Record<string, unknown>);
      };
      const onError = () => finish(new Error(`${method} could not reach the page`));
      const onClose = () => finish(new Error(`${method} target closed before answering`));
      const onOpen = () => socket.send(JSON.stringify({ id: 1, method, params }));
      const onMessage = (event: MessageEvent) => {
        try {
          assert(typeof event.data === 'string', 'Expected a DevTools message');
          const message = record(JSON.parse(event.data));
          if (message.id !== 1) return;
          assert(message.error === undefined, `${method} failed on the page`);
          finish(undefined, record(message.result));
        } catch (error) { finish(error); }
      };
      const timer = setTimeout(() => finish(new Error(`${method} did not answer within 60000 ms`)), 60000);
      socket.addEventListener('error', onError);
      socket.addEventListener('open', onOpen);
      socket.addEventListener('message', onMessage);
      socket.addEventListener('close', onClose);
    });
  } finally {
    if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) socket.close();
  }
}

/** Reads one expression inside an existing page, exactly as that window's own scripts see it. */
export async function evaluate(target: Page, expression: string): Promise<unknown> {
  const result = record(await command(target, 'Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }));
  assert(result.exceptionDetails === undefined, 'The page evaluated the expression without throwing');
  return record(result.result).value;
}

/** Saves what the real window shows, as this run's evidence. */
export async function screenshot(target: Page, label: string): Promise<string> {
  const result = await command(target, 'Page.captureScreenshot', { format: 'png' });
  const path = resolve(screens, `${label}.png`);
  await mkdir(screens, { recursive: true });
  await writeFile(path, Buffer.from(text(result.data), 'base64'));
  return path;
}

/** Runs the application driver for one test and stops it again, whatever the test did. */
async function driver(port: number) {
  await mkdir(artifacts, { recursive: true });
  const log = await open(resolve(artifacts, `driver-${port}.log`), 'a');
  const child = spawn(process.execPath, [resolve(root, 'scripts/e2e-desktop.mjs'), String(port)],
    { cwd: root, stdio: ['ignore', log.fd, log.fd] });
  const exited = new Promise<number | null>(done => child.once('exit', code => done(code)));
  try {
    for (let attempt = 0; attempt < 240; attempt += 1) {
      assert(child.exitCode === null, `The desktop driver exited ${child.exitCode}; see ${resolve(artifacts, `driver-${port}.log`)}`);
      try {
        return { call: await control(port), stop: async () => { child.kill('SIGTERM'); await exited; await log.close(); } };
      } catch {
        await sleep(500);
      }
    }
    throw new Error('The desktop driver never reported a running application');
  } catch (cause) {
    child.kill('SIGTERM');
    await exited;
    await log.close();
    throw cause;
  }
}

export const test = base.extend<{ desktop: Desktop }>({
  desktop: async ({}, use) => {
    const port = await freePort();
    await rm(resolve(artifacts, `control/${port}.json`), { force: true });
    const { call, stop } = await driver(port);
    try {
      const current = await inspection(await call({ op: 'inspect' }));
      const inside = relative(artifacts, resolve(current.profile));
      assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'The desktop profile stays in this task');
      await use({
        inspect: async () => inspection(await call({ op: 'inspect' })),
        status: async () => record(await call({ op: 'status' })) as LauncherStatus,
        invoke: async (method, ...args) => record(await call({ op: 'invoke', method, args })) as LauncherStatus,
        quit: async () => {
          const result = record(await call({ op: 'quit' }));
          assert(typeof result.forced === 'boolean', 'The driver reports whether application quit was forced');
          return { pid: result.pid === null ? null : identifier(result.pid), owned: identifiers(result.owned), surviving: identifiers(result.surviving), forced: result.forced };
        },
        closeWindows: async () => {
          const value = await call({ op: 'close-windows' });
          assert(Array.isArray(value));
          return value as Array<{ url: string; visibility: string }>;
        },
        start: async () => inspection(await call({ op: 'start' })),
        createFixture: async (start = true) => {
          const value = record(await call({ op: 'create-fixture', start }));
          assert(typeof value.dataDir === 'string' && typeof value.deviceId === 'string');
          assert(typeof value.peerPort === 'number' && typeof value.localPort === 'number');
          assert(typeof value.running === 'boolean');
          return value as FixtureNode;
        },
        startFixture: async dataDir => record(await call({ op: 'start-fixture', dataDir })),
        stopFixture: async dataDir => record(await call({ op: 'stop-fixture', dataDir })),
        snapshotNode: async dataDir => {
          const value = record(await call({ op: 'snapshot-node', dataDir }));
          assert(typeof value.deviceId === 'string' && isRecord(value.values));
          assert(typeof value.status === 'string' && (typeof value.version === 'string' || value.version === null) && typeof value.coreVersion === 'string');
          return value as { deviceId: string; status: string; version: string | null; coreVersion: string; values: Record<string, string | null> };
        },
        serviceStatus: async deviceId => record(await call({ op: 'service-status', deviceId })),
        detachSourceRuntime: async deviceId => record(await call({ op: 'detach-source-runtime', deviceId })),
      });
    } finally {
      await stop();
    }
  },
});
