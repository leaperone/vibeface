import assert from 'node:assert/strict';
import { insideTaskArtifacts } from '../support/artifact-paths';
import type { Caller, FixtureReady, NativeRequest } from '../support/fixture-contract';
import { withControlledFixture } from '../support/fixture-lifecycle';
import { createFixtureTransport, type FixtureRejectionCase } from '../support/fixture-transport';
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, readdir, readlink, unlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { join, relative, resolve } from 'node:path';
import { pipeline } from 'node:stream/promises';

const root = resolve(import.meta.dirname, '../..');
export function record(value: unknown): Record<string, unknown> {
  const valid = (input: unknown): input is Record<string, unknown> => input !== null && typeof input === 'object' && !Array.isArray(input);
  assert(valid(value), 'Expected bounded fixture JSON');
  return value;
}
export function text(value: unknown): string { assert(typeof value === 'string' && value.length > 0); return value; }
type NativeKind = 'delivery' | 'workspace';
export type NativeFixture<K extends NativeKind> = {
  fixture: FixtureReady<K>;
  invitation: string;
  call: Caller<NativeRequest<K>, K>;
  expectFixtureOperationRejection(request: K extends 'delivery' ? FixtureRejectionCase : never): Promise<void>;
};
export async function nativeFixture<K extends NativeKind>(kind: K, artifacts: string, use: (network: NativeFixture<K>) => Promise<void>): Promise<void> {
  const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
  assert(binary && crypto, 'Native acceptance needs the actual Node and its matching WASM');
  await mkdir(artifacts, { recursive: true, mode: 0o700 });
  for (const name of kind === 'workspace' ? ['acceptance.json', 'cleanup.json'] : ['cleanup.json']) {
    await unlink(resolve(artifacts, name)).catch(error => { if (error.code !== 'ENOENT') throw error; });
  }
  const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(artifacts, 'invitation.txt');
  const invitationRoot = kind === 'delivery' ? resolve(root, '.artifacts/native') : artifacts;
  assert(insideTaskArtifacts(invitationRoot, resolve(invitation), { allowRoot: false }), 'Only this task can receive an invitation');
  await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
  const listener = createServer();
  await new Promise<void>((done, fail) => { listener.once('error', fail); listener.listen(0, '127.0.0.1', done); });
  const address = listener.address();
  assert(address && typeof address === 'object');
  await new Promise<void>((done, fail) => listener.close(error => error ? fail(error) : done()));
  const args = [resolve(root, 'scripts/verify-node.py'), '--e2e-fixture', '--fixture-kind', kind,
    '--fixture-port', String(address.port), '--binary', binary, '--crypto-module', crypto, '--artifacts', artifacts];
  if (process.env.VIBEFACE_E2E_APP_DIR) args.push('--app-dir', process.env.VIBEFACE_E2E_APP_DIR);
  if (kind === 'delivery' && process.env.VIBEFACE_E2E_PROFILE_FILE) args.push('--fixture-profile', process.env.VIBEFACE_E2E_PROFILE_FILE);
  const child = spawn('python3', args, { cwd: root, stdio: ['pipe', 'pipe', 'pipe'] });
  const transport = createFixtureTransport(child, {
    kind, artifacts, origin: `http://127.0.0.1:${address.port}`, surface: 'native',
    requestTimeoutMs: 45000,
    shutdownTimeoutsMs: [30000, 30000, 5000],
  });
  let logResult: Promise<Error | undefined> | undefined;
  await withControlledFixture({
    run: async () => {
      const log = createWriteStream(resolve(artifacts, `fixture-${randomUUID()}.log`), { flags: 'wx', mode: 0o600 });
      logResult = pipeline(child.stderr, log).then(() => undefined, () => {
        const error = new Error('Native fixture stderr log failed');
        transport.fail(error.message);
        return error;
      });
      await use({ fixture: await transport.ready, invitation, call: transport.call, expectFixtureOperationRejection: transport.expectFixtureOperationRejection });
    },
    stop: async () => {
      const failures: unknown[] = [];
      if (!logResult) child.stderr.resume();
      try {
        const { code, signal } = await transport.stop();
        if (code !== 0 || signal !== null) {
          failures.push(new Error('The task fixture and its own Node processes failed cleanup'));
        }
      } catch (error) {
        failures.push(error);
        child.stderr.destroy();
      }
      try {
        await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
      } catch (error) {
        failures.push(error);
      }
      const logFailure = await logResult;
      if (logFailure) failures.push(logFailure);
      if (failures.length === 1) throw failures[0];
      if (failures.length > 1) throw new AggregateError(failures, 'Native fixture cleanup failed');
      await writeFile(resolve(artifacts, 'cleanup.json'), `${JSON.stringify({ fixture_stopped: true })}\n`, { mode: 0o600 });
    },
  });
}

export async function bundleManifest(directory: string): Promise<Record<string, string>> {
  const files: Record<string, string> = {};
  async function visit(path: string): Promise<void> {
    for (const entry of (await readdir(path, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
      const file = join(path, entry.name), name = relative(directory, file);
      if (entry.isDirectory()) await visit(file);
      else files[name] = entry.isSymbolicLink() ? `link:${await readlink(file)}` : createHash('sha256').update(await readFile(file)).digest('hex');
    }
  }
  await visit(directory);
  assert(Object.keys(files).length > 0, 'The actual bundle has files to compare');
  return files;
}
