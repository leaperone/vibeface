import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { copyFile, mkdir, readFile, readdir, readlink, unlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { join, relative, resolve } from 'node:path';
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
type NativeNetwork = { invitation: string; receiver: string; call(request: Record<string, unknown>): Promise<Record<string, unknown>> };
const test = base.extend<{ network: NativeNetwork }>({
  network: async ({}, use) => {
    const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
    assert(binary && crypto, 'Native acceptance needs the actual Node and its matching WASM');
    const artifacts = resolve(root, '.artifacts/native/network');
    await mkdir(artifacts, { recursive: true, mode: 0o700 });
    await unlink(resolve(artifacts, 'cleanup.json')).catch(error => { if (error.code !== 'ENOENT') throw error; });
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
      const setup = await call({ op: 'native_setup', invitation_file: invitation });
      await use({ invitation: (await readFile(invitation, 'utf8')).trim(), receiver: text(record(record(setup.nodes).receiver).peer), call });
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
  },
});

async function bundleManifest(directory: string): Promise<Record<string, string>> {
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

function providerInspection(value: Record<string, unknown>) {
  let manager: { provider: string; model: string; reasoning_effort: string | null } | null = null;
  if (value.manager !== null) {
    const saved = record(value.manager);
    assert(typeof saved.model === 'string');
    assert(saved.reasoning_effort === null || typeof saved.reasoning_effort === 'string');
    manager = { provider: text(saved.provider), model: saved.model, reasoning_effort: saved.reasoning_effort };
  }
  assert(typeof value.checked === 'boolean' && Array.isArray(value.jobs));
  assert(typeof value.running_invocations === 'number' && Number.isSafeInteger(value.running_invocations) && value.running_invocations >= 0);
  assert(typeof value.queued_invocations === 'number' && Number.isSafeInteger(value.queued_invocations) && value.queued_invocations >= 0);
  assert(Array.isArray(value.roles) && value.roles.every(role => role === 'agent' || role === 'execution'));
  assert(typeof value.execution_model === 'string');
  assert(typeof value.login_count === 'number' && Number.isSafeInteger(value.login_count) && value.login_count >= 0);
  const setup = record(value.setup);
  return { manager, roles: value.roles, execution_model: value.execution_model, login_count: value.login_count,
    setup: { directory: text(setup.directory), command: text(setup.command) },
    readiness: text(value.readiness), authentication: text(value.authentication), catalog: text(value.catalog), checked: value.checked,
    jobs: value.jobs.map((item: unknown) => { const job = record(item); return { trigger: text(job.trigger), state: text(job.state) }; }),
    running_invocations: value.running_invocations, queued_invocations: value.queued_invocations };
}

test('provider settings and queued cancellation work in the installed native app', { platforms: ['ios'], timeout: 600000 }, async ({ app, device, network, screen }) => {
  const simulator = process.env.VIBEFACE_E2E_SIMULATOR, appPath = process.env.VIBEFACE_E2E_IOS_APP;
  const source = process.env.VIBEFACE_E2E_SOURCE_COMMIT;
  const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
  assert(simulator && appPath && binary && crypto && source && /^[a-f0-9]{40}$/.test(source), 'Provider acceptance pins its source and actual artifacts');
  const publicPath = resolve(root, '.artifacts/native/public');
  await mkdir(publicPath, { recursive: true, mode: 0o700 });
  for (const name of ['provider.json', 'provider-selectable.png', 'provider-controlled.png', 'provider-typed.png']) {
    await unlink(join(publicPath, name)).catch(error => { if (error.code !== 'ENOENT') throw error; });
  }
  const wasm = crypto.replace(/\.js$/, '_bg.wasm');
  assert(wasm !== crypto);
  const inputs = { node: binary, crypto_js: crypto, crypto_wasm: wasm };
  const hashes = Object.fromEntries(await Promise.all(Object.entries(inputs).map(async ([name, path]) => [name, createHash('sha256').update(await readFile(path)).digest('hex')])));
  const web = await bundleManifest(resolve(root, 'crates/node/web/dist/client'));
  const built = await bundleManifest(appPath);
  const executable = (await execute('plutil', ['-extract', 'CFBundleExecutable', 'raw', '-o', '-', join(appPath, 'Info.plist')])).stdout.trim();
  assert(executable && !executable.includes('/') && /^[a-f0-9]{64}$/.test(built[executable]), 'The built app has its real native executable');
  assert((await execute('lipo', ['-archs', join(appPath, executable)])).stdout.trim().split(/\s+/).includes('arm64'));
  const javascript = Object.keys(built).filter(name => name.endsWith('.jsbundle'));
  assert(javascript.length > 0, 'The release app contains its packaged JavaScript');
  const native = Object.keys(built).filter(name => name === executable || name.endsWith('.dylib') || name.split('/').at(-2) === `${name.split('/').at(-1)}.framework`);
  await network.call({ op: 'native_provider_setup' });
  await assert.rejects(network.call({ op: 'native_provider_setup' }));
  await assert.rejects(network.call({ op: 'native_provider_stage', stage: 'invalid' }));
  await assert.rejects(network.call({ op: 'native_provider_stage', stage: 'ready', role: 'source' }));
  await device.installApp(undefined, { reinstall: true });
  const installedPath = (await execute('xcrun', ['simctl', 'get_app_container', simulator, 'one.leaper.vibeface', 'app'])).stdout.trim();
  assert(installedPath.startsWith('/') && installedPath.toLowerCase().includes(simulator.toLowerCase()), 'Inspect only the assigned simulator installed bundle');
  expect(await bundleManifest(installedPath)).toEqual(built);
  await device.clearKeychain();
  await app.open();
  await screen.getByLabel('加入链接').fill(secrets.get('nativeInvitation'), { timeout: 30000 });
  await expect(screen.getByText('已填入加入链接', { exact: true })).toBeVisible();
  await screen.getByRole('textbox', '这台设备的名字').fill('VDP native receiver');
  await device.dismissKeyboard();
  await screen.getByRole('button', '加入', { exact: true }).click();
  const pairing = await network.call({ op: 'native_join' }), code = text(pairing.code);
  assert(/^[0-9]{6}$/.test(code));
  await expect(screen.getByLabel(`安全码 ${code.split('').join(' ')}`)).toBeVisible();
  await network.call({ op: 'native_approve', peer: text(pairing.peer), code });
  await expect(screen.getByRole('link', '设置', { exact: true })).toBeVisible({ timeout: 30000 });
  const openSettings = async () => {
    await device.openLink(`vibeface://device/${network.receiver}`);
    const link = screen.getByRole('link', 'Bot 与任务', { exact: true });
    await screen.scrollUntilVisible(link, { direction: 'down', timeout: 30000 });
    await link.click();
  };
  const inspect = async () => providerInspection(await network.call({ op: 'native_provider_inspect' }));
  const probe = () => screen.getByRole('button', '检查可用性', { exact: true }).first().click();
  const codexRow = () => screen.getByRole('button', /^Codex，/);
  const botDetails = screen.getByRole('button', '运行 Bot', { exact: true });
  const executionDetails = screen.getByRole('button', '执行任务', { exact: true });
  const modelButton = () => screen.getByRole('button', /^设备默认模型，/);
  const select = async (name: string) => {
    await screen.scrollUntilVisible(modelButton(), { direction: 'down' });
    await modelButton().click();
    await screen.getByRole('radio', name, { exact: true }).click();
  };
  const save = async (model: string, effort: string | null) => {
    const button = screen.getByRole('button', '保存', { exact: true, visible: true });
    await screen.scrollUntilVisible(button, { direction: 'down' });
    await button.click();
    await expect.poll(async () => (await inspect()).manager, { timeout: 15000 }).toEqual({ provider: 'codex', model, reasoning_effort: effort });
  };
  const executionButton = () => screen.getByRole('button', /^执行任务默认模型，/);
  const selectExecution = async (name: string) => {
    await screen.scrollUntilVisible(executionButton(), { direction: 'down' });
    await executionButton().click();
    await screen.getByRole('radio', name, { exact: true }).click();
  };
  const saveExecution = async (model: string) => {
    const button = screen.getByRole('button', '保存', { exact: true, visible: true });
    await screen.scrollUntilVisible(button, { direction: 'down' });
    await button.click();
    await expect.poll(async () => (await inspect()).execution_model, { timeout: 15000 }).toBe(model);
  };
  const screenshot = async (name: string) => { await copyFile(await app.screenshot(name), join(publicPath, `${name}.png`)); };
  await openSettings();
  const initialProvider = screen.getByRole('button', 'Codex，尚未确认', { exact: true });
  await expect(initialProvider).toBeVisible();
  await expect(screen.getByText('尚未主动检查', { exact: true }).first()).toBeVisible();
  expect((await inspect()).checked).toBe(false);
  expect((await inspect()).manager).toBeNull();
  await assert.rejects(network.call({ op: 'native_provider_queue' }));
  await initialProvider.click();
  await screen.getByLabel('Codex 执行任务', { exact: true }).click();
  await expect.poll(async () => (await inspect()).roles, { timeout: 15000 }).toEqual(['execution']);
  expect((await inspect()).manager).toBeNull();
  await probe();
  await expect(codexRow()).toHaveAccessibleName('Codex，执行任务，需设置');
  expect((await inspect()).authentication).toBe('sign_in_required');
  const setup = (await inspect()).setup;
  await expect(screen.getByText(setup.command, { exact: true })).toBeVisible();
  await screen.getByRole('button', '复制Codex 私有目录登录命令', { exact: true }).click();
  await expect.poll(() => device.clipboard()).toBe(setup.command);
  const copied = await device.clipboard();
  assert.equal(copied, setup.command);
  await assert.rejects(network.call({ op: 'native_provider_login', command: copied + '; false' }));
  await assert.rejects(network.call({ op: 'native_provider_login', command: copied, role: 'source' }));
  expect((await inspect()).login_count).toBe(0);
  expect((await inspect()).authentication).toBe('sign_in_required');
  const login = await network.call({ op: 'native_provider_login', command: copied });
  expect(login).toEqual({ executed: true, private_root_verified: true, controlled_bin_verified: true, login_count: 1 });
  await probe();
  await expect(codexRow()).toHaveAccessibleName('Codex，执行任务，可用');
  await expect.poll(async () => (await inspect()).authentication).toBe('authenticated');
  expect((await inspect()).catalog).toBe('available');
  expect((await inspect()).manager).toBeNull();
  await expect(executionDetails).not.toBeExpanded();
  await executionDetails.click();
  await expect(executionButton()).toHaveAccessibleName('执行任务默认模型，由 AI 服务决定');
  await selectExecution('Native selectable');
  await saveExecution('native-selectable');
  expect((await inspect()).manager).toBeNull();
  await screen.scrollUntilVisible(executionButton(), { direction: 'up' });
  await executionButton().click();
  await screen.getByRole('button', '其他…', { exact: true }).click();
  await screen.getByRole('textbox', '执行任务默认模型', { exact: true }).fill('native-execution-custom');
  await device.dismissKeyboard();
  await saveExecution('native-execution-custom');
  expect((await inspect()).manager).toBeNull();
  await screen.scrollUntilVisible(screen.getByRole('button', '从列表选', { exact: true }), { direction: 'up' });
  await screen.getByRole('button', '从列表选', { exact: true }).click();
  await saveExecution('');
  await expect(executionButton()).toHaveAccessibleName('执行任务默认模型，由 AI 服务决定');
  expect((await inspect()).roles).toEqual(['execution']);
  expect((await inspect()).manager).toBeNull();
  await screen.scrollUntilVisible(executionDetails, { direction: 'up' });
  await executionDetails.click();
  await screen.scrollUntilVisible(screen.getByLabel('Codex 运行 Bot', { exact: true }), { direction: 'up' });
  await screen.getByLabel('Codex 运行 Bot', { exact: true }).click();
  await expect.poll(async () => (await inspect()).manager?.provider, { timeout: 15000 }).toBe('codex');
  await openSettings();
  await codexRow().click();
  await expect(botDetails).not.toBeExpanded();
  await expect(executionDetails).not.toBeExpanded();
  await botDetails.click();
  await select('Native selectable');
  await screen.getByRole('button', 'high', { exact: true }).click();
  await screen.scrollUntilVisible(botDetails, { direction: 'up' });
  await botDetails.click();
  await executionDetails.click();
  await selectExecution('Native controlled');
  await screen.scrollUntilVisible(executionDetails, { direction: 'up' });
  await executionDetails.click();
  await screen.scrollUntilVisible(botDetails, { direction: 'up' });
  await botDetails.click();
  await expect(modelButton()).toHaveAccessibleName('设备默认模型，Native selectable');
  await save('native-selectable', 'high');
  expect((await inspect()).execution_model).toBe('');
  await screen.scrollUntilVisible(modelButton(), { direction: 'up' });
  await select('Native controlled');
  await screen.scrollUntilVisible(botDetails, { direction: 'up' });
  await botDetails.click();
  await executionDetails.click();
  await expect(executionButton()).toHaveAccessibleName('执行任务默认模型，Native controlled');
  await saveExecution('native-controlled');
  expect((await inspect()).manager).toEqual({ provider: 'codex', model: 'native-selectable', reasoning_effort: 'high' });
  await screen.scrollUntilVisible(executionDetails, { direction: 'up' });
  await executionDetails.click();
  await screen.scrollUntilVisible(botDetails, { direction: 'up' });
  await botDetails.click();
  await expect(modelButton()).toHaveAccessibleName('设备默认模型，Native controlled');
  await select('Native selectable');
  await screen.scrollUntilVisible(botDetails, { direction: 'up' });
  await botDetails.click();
  await executionDetails.click();
  await selectExecution('由 AI 服务决定');
  await saveExecution('');
  await expect(executionButton()).toHaveAccessibleName('执行任务默认模型，由 AI 服务决定');
  expect((await inspect()).manager).toEqual({ provider: 'codex', model: 'native-selectable', reasoning_effort: 'high' });
  await openSettings();
  await codexRow().click();
  await botDetails.click();
  await expect(modelButton()).toHaveAccessibleName('设备默认模型，Native selectable');
  expect((await inspect()).manager?.reasoning_effort).toBe('high');
  await screenshot('provider-selectable');
  await select('Native controlled');
  await save('native-controlled', null);
  await expect(screen.getByRole('button', 'high', { exact: true })).toBeHidden();
  await expect(screen.getByText('由 AI 服务决定', { exact: true }).first()).toBeVisible();
  await screenshot('provider-controlled');
  await select('Native selectable');
  await screen.getByRole('button', 'high', { exact: true }).click();
  await save('native-selectable', 'high');
  await network.call({ op: 'native_provider_stage', stage: 'catalog_unknown' });
  await screen.scrollUntilVisible(screen.getByRole('button', '检查可用性', { exact: true }).first(), { direction: 'up' });
  await probe();
  await expect(screen.getByText('模型列表暂不可用，可继续手填。已保存的模型保持不变。', { exact: true })).toBeVisible();
  expect((await inspect()).manager).toEqual({ provider: 'codex', model: 'native-selectable', reasoning_effort: 'high' });
  expect((await inspect()).catalog).toBe('unavailable');
  const typed = screen.getByRole('textbox', '设备默认模型', { exact: true });
  await screen.scrollUntilVisible(typed, { direction: 'down' });
  await typed.fill('native-custom-model');
  await device.dismissKeyboard();
  await save('native-custom-model', 'high');
  await screenshot('provider-typed');
  await network.call({ op: 'native_provider_stage', stage: 'ready' });
  await screen.scrollUntilVisible(screen.getByRole('button', '检查可用性', { exact: true }).first(), { direction: 'up' });
  await probe();
  await screen.scrollUntilVisible(typed, { direction: 'down' });
  await typed.fill(' native-controlled ');
  await device.dismissKeyboard();
  await expect(screen.getByRole('button', 'high', { exact: true })).toBeHidden();
  await save('native-controlled', null);
  await select('Native selectable');
  await screen.getByRole('button', 'high', { exact: true }).click();
  await save('native-selectable', 'high');
  const queue = await network.call({ op: 'native_provider_queue' });
  await assert.rejects(network.call({ op: 'native_provider_queue' }));
  const botId = text(queue.bot_id), runningTrigger = text(queue.running_trigger), queuedTrigger = text(queue.queued_trigger);
  await device.openLink(`vibeface://b/${botId}`);
  const cancel = screen.getByRole('button', '取消排队', { exact: true });
  await screen.scrollUntilVisible(cancel, { direction: 'up', timeout: 30000 });
  await cancel.click();
  await expect.poll(async () => (await inspect()).jobs.find(job => job.trigger === queuedTrigger)?.state, { timeout: 15000 }).toBe('cancelled');
  expect((await inspect()).queued_invocations).toBe(0);
  await screen.getByRole('button', '停止', { exact: true }).click();
  await expect.poll(async () => (await inspect()).jobs.find(job => job.trigger === runningTrigger)?.state, { timeout: 30000 }).toBe('cancelled');
  await expect(screen.getByRole('button', '停止', { exact: true })).toBeHidden();
  const after = await inspect();
  expect(after.running_invocations).toBe(1);
  expect(after.queued_invocations).toBe(0);
  await device.openLink('vibeface://settings');
  await device.openLink(`vibeface://b/${botId}`);
  await expect(screen.getByRole('textbox', '给 Bot 发消息', { exact: true })).toBeVisible();
  const reopened = await inspect();
  expect(reopened.jobs).toEqual(after.jobs);
  expect(reopened.running_invocations).toBe(1);
  expect(reopened.queued_invocations).toBe(0);
  expect(reopened.execution_model).toBe('');
  expect(reopened.login_count).toBe(1);
  expect(await bundleManifest(installedPath)).toEqual(built);
  expect(await bundleManifest(resolve(root, 'crates/node/web/dist/client'))).toEqual(web);
  for (const [name, path] of Object.entries(inputs)) expect(createHash('sha256').update(await readFile(path)).digest('hex')).toBe(hashes[name]);
  const digest = (value: unknown) => createHash('sha256').update(JSON.stringify(value)).digest('hex');
  await writeFile(join(publicPath, 'provider.json'), `${JSON.stringify({ status: 'ui_passed', source_commit: source,
    run_id: process.env.GITHUB_RUN_ID ?? null, run_attempt: process.env.GITHUB_RUN_ATTEMPT ?? null, simulator,
    host_arch: process.arch, runtime: process.env.VIBEFACE_E2E_IOS_RUNTIME ?? null, device_type: process.env.VIBEFACE_E2E_IOS_DEVICE_TYPE ?? null,
    app_manifest_sha256: digest(built), executable_sha256: built[executable],
    javascript: Object.fromEntries(javascript.map(name => [name, built[name]])), native: Object.fromEntries(native.map(name => [name, built[name]])),
    node_sha256: hashes.node, crypto_js_sha256: hashes.crypto_js, crypto_wasm_sha256: hashes.crypto_wasm,
    web_manifest_sha256: digest(web),
    real_pairing: true, installed_bundle_matches: true, known_sign_in_required: true, saved_model_high: true,
    provider_controlled_cleared_effort: true, unavailable_catalog_preserved_model_high: true, typed_model_saved: true,
    execution_only_catalog_select: true, execution_only_custom_and_default: true,
    default_collapsed_role_groups: true, role_drafts_survived_collapse: true,
    bot_save_preserved_execution_draft: true, execution_save_preserved_bot_draft: true,
    independent_execution_model_saved: true, execution_clear_preserved_bot: true,
    private_login_command_copied_exactly: true, controlled_bin_and_private_codex_home_verified: true,
    synthetic_login_count: reopened.login_count, native_authenticated_recheck: true, trimmed_controlled_cleared_effort: true,
    real_provider_model_calls: 0, not_checked: ['real Provider authentication', 'Claude-specific native login'],
    running_trigger: runningTrigger, queued_trigger: queuedTrigger, jobs: reopened.jobs,
    running_invocations: reopened.running_invocations, queued_invocations: reopened.queued_invocations,
    native_cancelled_queue: true, native_stopped_running: true, reopened_without_execution: true }, null, 2)}\n`, { mode: 0o600 });
});

test('receives a charity file through a real native pairing and verifies the saved bytes', { platforms: ['ios'], timeout: 240000 }, async ({ app, device, network, screen }) => {
  const simulator = process.env.VIBEFACE_E2E_SIMULATOR;
  assert(simulator && /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/i.test(simulator), 'Native acceptance never selects an implicit booted device');
  await device.installApp(undefined, { reinstall: true });
  await device.clearKeychain();
  await app.open();
  await device.openLink(`vibeface://join?link=${encodeURIComponent(network.invitation)}`);
  await expect(screen.getByText('已填入加入链接', { exact: true })).toBeVisible({ timeout: 30000 });
  await screen.getByRole('textbox', '这台设备的名字').fill('VDP native receiver');
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
