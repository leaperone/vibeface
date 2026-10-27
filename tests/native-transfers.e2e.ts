import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, readdir, unlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { promisify } from 'node:util';
import { execFile } from 'node:child_process';
import { test as base } from '@e2e-dev/mobile';
import { expect, secrets } from 'e2e';

const root = resolve(import.meta.dirname, '..');
const execute = promisify(execFile);
function record(value: unknown): Record<string, unknown> {
  const valid = (input: unknown): input is Record<string, unknown> => input !== null && typeof input === 'object' && !Array.isArray(input);
  assert(valid(value), 'Expected bounded fixture JSON');
  return value;
}
function text(value: unknown): string { assert(typeof value === 'string' && value.length > 0); return value; }
type NativeNetwork = { call(request: Record<string, unknown>): Promise<Record<string, unknown>> };
const test = base.extend<{ network: NativeNetwork }>({
  network: async ({}, use) => {
    const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
    assert(binary && crypto, 'Native acceptance needs the actual Node and its matching WASM');
    const artifacts = resolve(root, '.artifacts/native/network');
    await mkdir(artifacts, { recursive: true, mode: 0o700 });
    const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(artifacts, 'invitation.txt');
    await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
    const listener = createServer();
    await new Promise<void>((done, fail) => { listener.once('error', fail); listener.listen(0, '127.0.0.1', done); });
    const address = listener.address();
    assert(address && typeof address === 'object');
    await new Promise<void>((done, fail) => listener.close(error => error ? fail(error) : done()));
    const args = [resolve(root, 'scripts/verify-node.py'), '--e2e-fixture', '--fixture-kind', 'delivery',
      '--fixture-port', String(address.port), '--binary', binary, '--crypto-module', crypto, '--artifacts', artifacts];
    if (process.env.VIBEFACE_E2E_PROFILE_FILE) args.push('--fixture-profile', process.env.VIBEFACE_E2E_PROFILE_FILE);
    const child = spawn('python3', args, { cwd: root, stdio: ['pipe', 'pipe', 'pipe'] });
    let pending: { resolve(value: Record<string, unknown>): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> } | null = null;
    const wait = () => new Promise<Record<string, unknown>>((resolveValue, reject) => {
      assert(!pending, 'Native fixture accepts one operation at a time');
      const timer = setTimeout(() => { pending = null; reject(new Error('Native fixture operation timed out')); }, 45000);
      pending = { resolve: resolveValue, reject, timer };
    });
    const log = createWriteStream(resolve(artifacts, `fixture-${randomUUID()}.log`), { flags: 'wx', mode: 0o600 });
    child.stderr.pipe(log);
    child.once('error', error => { if (pending) { clearTimeout(pending.timer); pending.reject(error); pending = null; } });
    child.once('exit', () => { if (pending) { clearTimeout(pending.timer); pending.reject(new Error('Native fixture stopped before completing its request')); pending = null; } });
    const ready = wait();
    createInterface({ input: child.stdout }).on('line', line => {
      const current = pending;
      if (!current) return;
      pending = null; clearTimeout(current.timer);
      try {
        assert(line.length <= 65536, 'Native fixture response is bounded');
        const value = record(JSON.parse(line));
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
      await ready;
      await call({ op: 'native_setup', invitation_file: invitation });
      await use({ call });
    } finally {
      cancelPending();
      child.stdin.end();
      if (child.exitCode === null) await new Promise<void>(done => {
        const timer = setTimeout(() => { child.kill('SIGKILL'); done(); }, 20000);
        child.once('exit', () => { clearTimeout(timer); done(); });
      });
      log.end();
      await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
    }
  },
});

test('receives a charity file through a real native pairing and verifies the saved bytes', { platforms: ['ios'], timeout: 240000 }, async ({ app, device, network, screen }) => {
  const simulator = process.env.VIBEFACE_E2E_SIMULATOR;
  assert(simulator && /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/i.test(simulator), 'Native acceptance never selects an implicit booted device');
  await device.installApp(undefined, { reinstall: true });
  await device.clearKeychain();
  await app.open();
  await screen.getByRole('textbox', /^加入链接/).fill(secrets.get('nativeInvitation'), { timeout: 30000 });
  await screen.getByRole('textbox', /^设备名称/).fill('VDP native receiver');
  await device.dismissKeyboard();
  await screen.getByRole('button', '加入', { exact: true }).click();
  const pairing = await network.call({ op: 'native_join' });
  const code = text(pairing.code);
  assert(/^[0-9]{6}$/.test(code), 'Real pairing has a six digit safety code');
  await expect(screen.getByLabel(`安全码 ${code.split('').join(' ')}`)).toBeVisible();
  await network.call({ op: 'native_approve', peer: text(pairing.peer), code });
  await expect(screen.getByRole('link', '设置', { exact: true })).toBeVisible({ timeout: 30000 });
  await screen.getByRole('link', '设置', { exact: true }).click();
  await screen.scrollUntilVisible(screen.getByRole('link', '发送和接收文件', { exact: true }), { direction: 'down' });
  await screen.getByRole('link', '发送和接收文件', { exact: true }).click();
  await expect(screen.getByText('传输设置', { exact: true }).first()).toBeVisible();
  await expect(screen.getByText('启用文件传输', { exact: true }).first()).toBeVisible();
  const bytes = randomBytes(96 * 1024), name = 'native-charity-original.bin';
  const digest = createHash('sha256').update(bytes).digest('hex');
  const offered = await network.call({ op: 'native_send', name, content_base64: bytes.toString('base64') });
  assert(/^[a-f0-9]{64}$/.test(text(offered.order_id)));
  const incoming = screen.getByRole('button', /^待接收的文件/);
  await screen.scrollUntilVisible(incoming, { direction: 'down', timeout: 30000 });
  await incoming.click();
  const accept = screen.getByRole('button', '接收文件');
  await screen.scrollUntilVisible(accept, { direction: 'down' });
  await accept.click();
  const download = screen.getByRole('button', '保存文件');
  await screen.scrollUntilVisible(download, { direction: 'down', timeout: 30000 });
  await download.click();
  const container = (await execute('xcrun', ['simctl', 'get_app_container', simulator, 'one.leaper.vibeface', 'data'])).stdout.trim();
  assert(container.startsWith('/') && container.toLowerCase().includes(simulator.toLowerCase()), 'Read only this task simulator app container');
  const directory = resolve(container, 'Documents/transfers');
  await expect.poll(async () => {
    const found = await readdir(directory).catch(error => { if (error.code === 'ENOENT') return []; throw error; });
    const matching = found.filter(file => file.endsWith(`-${name}`));
    if (matching.length !== 1) return null;
    return createHash('sha256').update(await readFile(resolve(directory, matching[0]))).digest('hex');
  }, { timeout: 30000, message: 'The real native app saves the original file bytes' }).toBe(digest);
  const files = (await readdir(directory)).filter(file => file.endsWith(`-${name}`));
  assert.equal(files.length, 1, 'The actual native app saves one recovered original');
  const saved = await readFile(resolve(directory, files[0]));
  expect(createHash('sha256').update(saved).digest('hex')).toBe(digest);
  expect(saved.equals(bytes)).toBe(true);
  const inspection = await network.call({ op: 'inspect' });
  expect(inspection.wallet_files).toBe(0);
  expect(inspection.chain_rpc_calls).toBe(0);
  const screenshot = await app.screenshot('native-charity-file-saved');
  const evidence = { status: 'passed', simulator, bytes: bytes.length, sha256: digest, wallet_files: 0,
    chain_rpc_calls: 0, real_pairing: true, real_native_saved_file: true, screenshot };
  await writeFile(resolve(root, '.artifacts/native/acceptance.json'), `${JSON.stringify(evidence, null, 2)}\n`, { mode: 0o600 });
});
