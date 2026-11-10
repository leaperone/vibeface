import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, readdir, readlink, unlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { join, relative, resolve } from 'node:path';
import { createInterface } from 'node:readline';

const root = resolve(import.meta.dirname, '../..');
export function record(value: unknown): Record<string, unknown> {
  const valid = (input: unknown): input is Record<string, unknown> => input !== null && typeof input === 'object' && !Array.isArray(input);
  assert(valid(value), 'Expected bounded fixture JSON');
  return value;
}
export function text(value: unknown): string { assert(typeof value === 'string' && value.length > 0); return value; }
type NativeFixture = {
  fixture: Record<string, unknown>;
  invitation: string;
  call(request: Record<string, unknown>): Promise<Record<string, unknown>>;
};
export async function nativeFixture(kind: 'delivery' | 'workspace', artifacts: string, use: (network: NativeFixture) => Promise<void>): Promise<void> {
  const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
  assert(binary && crypto, 'Native acceptance needs the actual Node and its matching WASM');
  await mkdir(artifacts, { recursive: true, mode: 0o700 });
  for (const name of kind === 'workspace' ? ['acceptance.json', 'cleanup.json'] : ['cleanup.json']) {
    await unlink(resolve(artifacts, name)).catch(error => { if (error.code !== 'ENOENT') throw error; });
  }
  const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(artifacts, 'invitation.txt');
  const invitationRoot = kind === 'delivery' ? resolve(root, '.artifacts/native') : artifacts;
  const inside = relative(invitationRoot, resolve(invitation));
  assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Only this task can receive an invitation');
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
  let pending: { resolve(value: Record<string, unknown>): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> } | null = null;
  const wait = () => new Promise<Record<string, unknown>>((resolveValue, reject) => {
    assert(!pending, 'Native fixture accepts one operation at a time');
    const timer = setTimeout(() => { pending = null; reject(new Error('Native fixture operation timed out')); }, 45000);
    pending = { resolve: resolveValue, reject, timer };
  });
  const log = createWriteStream(resolve(artifacts, `fixture-${randomUUID()}.log`), { flags: 'wx', mode: 0o600 });
  child.stderr.pipe(log);
  const fail = (error: Error) => { if (pending) { clearTimeout(pending.timer); pending.reject(error); pending = null; } };
  child.once('error', fail);
  child.once('exit', () => fail(new Error('Native fixture stopped before completing its request')));
  const ready = wait();
  createInterface({ input: child.stdout }).on('line', line => {
    const current = pending;
    if (!current) return;
    pending = null; clearTimeout(current.timer);
    try {
      assert(line.length <= 65536, 'Native fixture response is bounded');
      const value = record(JSON.parse(line));
      if (value.ok === false) {
        const kind = typeof value.error === 'string' && /^[A-Za-z_]{1,50}$/.test(value.error) ? value.error : 'Error';
        const frames = Array.isArray(value.frames) ? value.frames.filter((frame: unknown): frame is string =>
          typeof frame === 'string' && /^verify-node\.py:[1-9][0-9]{0,4}:[A-Za-z_][A-Za-z0-9_]{0,70}$/.test(frame)).slice(0, 4) : [];
        current.reject(new Error(`Native fixture failure ${kind} [${frames.join(';')}]`));
        return;
      }
      assert('ready' in value || value.ok === true, 'Native fixture operation succeeds');
      current.resolve(record(value.ready ?? value.result));
    } catch { current.reject(new Error('Native fixture returned an invalid response')); }
  });
  const call = async (request: Record<string, unknown>) => {
    const result = wait();
    child.stdin.write(`${JSON.stringify(request)}\n`);
    return result;
  };
  const cancelPending = () => { if (pending) { clearTimeout(pending.timer); pending = null; } };
  try {
    await use({ fixture: await ready, invitation, call });
  } finally {
    cancelPending();
    child.stdin.end();
    const exited = (timeout: number) => new Promise<boolean>(done => {
      if (child.exitCode !== null || child.signalCode !== null) return done(true);
      const ended = () => { clearTimeout(timer); done(true); };
      const timer = setTimeout(() => { child.removeListener('exit', ended); done(false); }, timeout);
      child.once('exit', ended);
    });
    if (!await exited(30000)) {
      child.kill('SIGTERM');
      if (!await exited(30000)) { child.kill('SIGKILL'); await exited(5000); }
    }
    log.end();
    await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
    assert.equal(child.exitCode, 0, 'The task fixture and its own Node processes finish cleanup');
    await writeFile(resolve(artifacts, 'cleanup.json'), `${JSON.stringify({ fixture_stopped: true })}\n`, { mode: 0o600 });
  }
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
