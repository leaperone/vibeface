import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, readdir, readlink, unlink, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { join, relative, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { promisify } from 'node:util';
import { test as base } from '@e2e-dev/mobile';
import { expect, secrets } from 'e2e';

const root = resolve(import.meta.dirname, '../..');
const artifacts = resolve(root, '.artifacts/native/workspace');
const execute = promisify(execFile);
function record(value: unknown): Record<string, unknown> {
  assert(value !== null && typeof value === 'object' && !Array.isArray(value), 'Expected fixture JSON');
  return Object.fromEntries(Object.entries(value));
}
function text(value: unknown): string { assert(typeof value === 'string' && value.length > 0); return value; }
function rows(value: unknown) { assert(Array.isArray(value)); return value.map(record); }
type NativeWorkspace = {
  gates: { taskA: string; channel: string };
  call(request: Record<string, unknown>): Promise<Record<string, unknown>>;
};
const test = base.extend<{ network: NativeWorkspace }>({
  network: async ({}, use) => {
    const binary = process.env.VIBEFACE_E2E_BINARY, crypto = process.env.VIBEFACE_E2E_CRYPTO_MODULE;
    assert(binary && crypto, 'Native workspace acceptance needs the current Node and matching WASM');
    await mkdir(artifacts, { recursive: true, mode: 0o700 });
    for (const name of ['acceptance.json', 'cleanup.json']) {
      await unlink(resolve(artifacts, name)).catch(error => { if (error.code !== 'ENOENT') throw error; });
    }
    const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(artifacts, 'invitation.txt');
    const inside = relative(artifacts, resolve(invitation));
    assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Only this task can receive an invitation');
    await unlink(invitation).catch(error => { if (error.code !== 'ENOENT') throw error; });
    const listener = createServer();
    await new Promise<void>((done, fail) => { listener.once('error', fail); listener.listen(0, '127.0.0.1', done); });
    const address = listener.address();
    assert(address && typeof address === 'object');
    await new Promise<void>((done, fail) => listener.close(error => error ? fail(error) : done()));
    const args = [resolve(root, 'scripts/verify-node.py'), '--e2e-fixture', '--fixture-kind', 'workspace',
      '--fixture-port', String(address.port), '--binary', binary, '--crypto-module', crypto, '--artifacts', artifacts];
    if (process.env.VIBEFACE_E2E_APP_DIR) args.push('--app-dir', process.env.VIBEFACE_E2E_APP_DIR);
    const child = spawn('python3', args, { cwd: root, stdio: ['pipe', 'pipe', 'pipe'] });
    const log = createWriteStream(resolve(artifacts, `fixture-${randomUUID()}.log`), { flags: 'wx', mode: 0o600 });
    child.stderr.pipe(log);
    let pending: { resolve(value: Record<string, unknown>): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> } | null = null;
    const wait = () => new Promise<Record<string, unknown>>((resolveValue, reject) => {
      assert(!pending, 'The fixture accepts one operation at a time');
      const timer = setTimeout(() => { pending = null; reject(new Error('Workspace fixture operation timed out')); }, 45000);
      pending = { resolve: resolveValue, reject, timer };
    });
    const fail = (error: Error) => { if (pending) { clearTimeout(pending.timer); pending.reject(error); pending = null; } };
    child.once('error', fail);
    child.once('exit', () => fail(new Error('Workspace fixture stopped before its request completed')));
    const ready = wait();
    createInterface({ input: child.stdout }).on('line', line => {
      const current = pending;
      if (!current) return;
      pending = null; clearTimeout(current.timer);
      try {
        assert(line.length <= 65536, 'Fixture JSON is bounded');
        const value = record(JSON.parse(line));
        assert('ready' in value || value.ok === true, 'The controlled fixture operation succeeds');
        current.resolve(record(value.ready ?? value.result));
      } catch { current.reject(new Error('Invalid workspace fixture response')); }
    });
    const call = async (request: Record<string, unknown>) => {
      const result = wait();
      child.stdin.write(`${JSON.stringify(request)}\n`);
      return result;
    };
    const cancelPending = () => { if (pending) { clearTimeout(pending.timer); pending = null; } };
    try {
      const fixture = await ready;
      const source = record(record(fixture.nodes).source), url = new URL(text(source.url));
      assert(url.protocol === 'http:' && url.hostname === '127.0.0.1', 'The native app connects only to this isolated Node');
      const gate = (name: string) => {
        const value = text(record(fixture.gates)[name]);
        const inside = relative(artifacts, resolve(value));
        assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'Native gates remain in this fixture');
        return value;
      };
      await call({ op: 'workspace_native_setup', invitation_file: invitation });
      await use({ gates: { taskA: gate('taskA'), channel: gate('channel') }, call });
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
      assert.equal(child.exitCode, 0, 'The fixture stops its own Node and controlled CLI');
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
  assert(Object.keys(files).some(name => name.endsWith('.jsbundle')), 'Acceptance uses packaged native JavaScript');
  return files;
}

test('native workspace routes preserve Task input, queue controls and directed Channel results', { platforms: ['ios'], tags: ['workspace-native'], timeout: 600000 }, async ({ app, device, network, screen }) => {
  const simulator = process.env.VIBEFACE_E2E_SIMULATOR, appPath = process.env.VIBEFACE_E2E_IOS_APP;
  const source = process.env.VIBEFACE_E2E_SOURCE_COMMIT;
  assert(simulator && appPath && source && /^[a-f0-9]{40}$/.test(source), 'Acceptance names the current native build and its source');
  assert.equal((await execute('git', ['rev-parse', 'HEAD'], { cwd: root })).stdout.trim(), source);
  assert.equal((await readFile(resolve(artifacts, 'simulator.txt'), 'utf8')).trim(), simulator);
  const devices = record(record(JSON.parse((await execute('xcrun', ['simctl', 'list', 'devices', '--json'])).stdout)).devices);
  const assigned = Object.values(devices).flatMap(rows).filter(row => row.udid === simulator);
  assert.equal(assigned.length, 1);
  assert.equal(assigned[0].state, 'Booted');
  assert(text(assigned[0].name).startsWith('vibeface-workspace-'), 'Only a simulator created for this workspace acceptance can be reset');
  const built = await bundleManifest(appPath);
  await device.installApp(undefined, { reinstall: true });
  const installed = (await execute('xcrun', ['simctl', 'get_app_container', simulator, 'one.leaper.vibeface', 'app'])).stdout.trim();
  assert(installed.startsWith('/') && installed.toLowerCase().includes(simulator.toLowerCase()));
  expect(await bundleManifest(installed)).toEqual(built);
  await device.clearKeychain();
  await app.open();
  const textbox = (name: string) => screen.getByRole('textbox', new RegExp(`^${name}(?:$|[\\s,，])`));
  const keyboard = device.locator('role=keyboard');
  const invitationField = textbox('加入链接');
  const openConfirmation = screen.getByRole('alert', 'Open in “Vibeface”?', { exact: true });
  await expect.poll(async () => await openConfirmation.isVisible() || await invitationField.isVisible(), { timeout: 30000 }).toBe(true);
  if (await openConfirmation.isVisible()) {
    await device.alert('dismiss');
    await expect(openConfirmation).toHaveCount(0);
    await app.open();
  }
  await invitationField.fill(secrets.get('nativeInvitation'), { timeout: 30000 });
  await expect(screen.getByText('已填入加入链接', { exact: true })).toBeVisible();
  await textbox('这台设备的名字').fill('VDP native receiver');
  await device.dismissKeyboard();
  await screen.getByRole('button', '加入', { exact: true }).click();
  const pairing = await network.call({ op: 'native_join' }), code = text(pairing.code);
  assert(/^[0-9]{6}$/.test(code));
  await expect(screen.getByLabel(`安全码 ${code.split('').join(' ')}`)).toBeVisible();
  await network.call({ op: 'native_approve', peer: text(pairing.peer), code });
  await expect(screen.getByRole('link', '设置', { exact: true })).toBeVisible({ timeout: 30000 });
  const inspect = async () => {
    const value = await network.call({ op: 'workspace_inspect' });
    return {
      invocations: rows(value.invocations).map(row => ({ label: text(row.label), conversation: text(row.conversation_id), thread: text(row.thread_id) })),
      steers: rows(value.steers).map(row => ({ id: text(row.message_id), thread: text(row.thread_id), turn: text(row.turn_id), text: text(row.text) })),
      turns: rows(value.turns).map(row => ({ thread: text(row.thread_id), turn: text(row.turn_id), state: text(row.state) })),
      interrupts: rows(value.interrupts).map(row => ({ thread: text(row.thread_id), turn: text(row.turn_id) })),
    };
  };
  const invocation = async (label: string) => {
    await expect.poll(async () => (await inspect()).invocations.filter(row => row.label === label).length, { timeout: 30000 }).toBe(1);
    const found = (await inspect()).invocations.find(row => row.label === label);
    assert(found);
    return found;
  };
  const backButton = screen.getByTestId('BackButton', { visible: true });
  const builderButton = screen.getByRole('button', 'Native Builder', { exact: true, visible: true });
  for (const name of ['Native Builder', 'Native Reviewer']) {
    await screen.getByRole('link', '新 Bot', { exact: true, visible: true }).first().click();
    await textbox('名称').fill(name);
    await screen.getByRole('button', '创建', { exact: true }).click();
    await expect(textbox('给 Bot 发消息')).toBeVisible();
    await expect(backButton).toHaveAccessibleName('消息');
    await backButton.click();
    await expect(screen.getByRole('link', '设置', { exact: true })).toBeVisible();
  }
  await screen.getByRole('link', '任务', { exact: true, visible: true }).first().click();
  const newTask = screen.getByRole('link', '新任务', { exact: true });
  await screen.scrollUntilVisible(newTask, { direction: 'down' });
  await newTask.click();
  await expect(textbox('任务名称')).toBeVisible();
  await expect(builderButton).toHaveCount(1);
  await builderButton.click();
  await expect(builderButton).toBeSelected();
  await textbox('任务名称').fill('原生导航检查');
  await textbox('交代内容').fill(JSON.stringify({ phase1: true, label: 'native-workspace-task', gate: network.gates.taskA }), { timeout: 30000 });
  await device.dismissKeyboard();
  await expect(keyboard).toHaveCount(0);
  const createTask = screen.getByRole('button', '创建并发送', { exact: true, visible: true });
  await screen.scrollUntilVisible(createTask, { direction: 'down' });
  await createTask.click();
  const composer = textbox('发给 Native Builder · 原生导航检查');
  await expect(composer).toBeVisible();
  const task = await invocation('native-workspace-task');
  const supplements = ['原生补充一：保留入口', '原生补充二：检查键盘', '原生补充三：支持手机'];
  for (const supplement of supplements) {
    await composer.fill(supplement);
    await screen.getByRole('button', '补充当前工作', { exact: true, visible: true }).click();
    await expect(composer).toHaveValue('');
  }
  await expect.poll(async () => (await inspect()).steers.filter(row => row.thread === task.thread).length, { timeout: 30000 }).toBe(3);
  const accepted = await inspect(), inputs = accepted.steers.filter(row => row.thread === task.thread);
  expect(inputs.map(row => supplements.find(value => row.text.includes(value)))).toEqual(supplements);
  expect(new Set(inputs.map(row => row.id)).size).toBe(3);
  expect(new Set(inputs.map(row => row.turn)).size).toBe(1);
  expect(accepted.turns.find(row => row.turn === inputs[0].turn)?.state).toBe('inProgress');
  await screen.getByLabel('任务上下文和历史', { exact: true, visible: true }).click();
  const sources = screen.getByText('本轮上下文引用', { exact: true });
  await screen.scrollUntilVisible(sources, { direction: 'down' });
  await expect(sources).toBeVisible();
  await expect(screen.getByText('本轮交代 1 项', { exact: true })).toBeVisible();
  const receipt = screen.getByText(/^第 3 条补充 · (执行服务已接受输入|已确认回合归属)$/);
  await screen.scrollUntilVisible(receipt, { direction: 'down' });
  await expect(receipt).toBeVisible();
  await app.screenshot('workspace-native-task-context');
  await expect(backButton).toHaveAccessibleName('原生导航检查');
  await backButton.click();
  const target = screen.getByRole('button', /^输入目标 Native Builder · 原生导航检查，/, { visible: true });
  const targetSheet = screen.getByTestId('input-options-sheet', { visible: true });
  await target.click();
  await expect(targetSheet).toHaveCount(1);
  await targetSheet.getByRole('button', '稍后处理', { exact: true, visible: true }).click();
  await expect(targetSheet).toHaveCount(0);
  await composer.fill('原生稍后条目');
  await screen.getByRole('button', '保存为稍后处理', { exact: true, visible: true }).click();
  await expect(composer).toHaveValue('');
  await target.click();
  await expect(targetSheet).toHaveCount(1);
  const queued = targetSheet.getByText('稍后处理 1 条', { exact: true });
  await screen.scrollUntilVisible(queued, { direction: 'down' });
  await expect(queued).toBeVisible();
  await targetSheet.getByRole('button', '编辑输入', { exact: true, visible: true }).click();
  await targetSheet.getByRole('textbox', /^编辑稍后输入(?:$|[\s,，])/).fill('原生稍后条目已编辑');
  await device.dismissKeyboard();
  await expect(keyboard).toHaveCount(0);
  const saveInput = targetSheet.getByRole('button', '保存输入修改', { exact: true, visible: true });
  await screen.scrollUntilVisible(saveInput, { direction: 'down' });
  await saveInput.click();
  await expect(targetSheet.getByText('原生稍后条目已编辑', { exact: true })).toBeVisible();
  await targetSheet.getByRole('button', '取消稍后输入', { exact: true, visible: true }).click();
  await expect(queued).toHaveCount(0);
  await targetSheet.getByRole('button', '关闭目标和队列', { exact: true }).click();
  await expect(targetSheet).toHaveCount(0);
  await composer.fill('停止后保留的原生草稿');
  await expect(screen.getByRole('button', '补充当前工作', { exact: true, visible: true })).toBeEnabled();
  const stop = screen.getByRole('button', '停止', { exact: true, visible: true });
  await expect(stop).toBeEnabled();
  await app.screenshot('workspace-native-task-draft-stop');
  await stop.click();
  await expect(composer).toHaveValue('停止后保留的原生草稿');
  await expect.poll(async () => (await inspect()).turns.find(row => row.turn === inputs[0].turn)?.state, { timeout: 30000 }).toBe('interrupted');
  const stopped = await inspect();
  expect(stopped.interrupts).toEqual([{ thread: task.thread, turn: inputs[0].turn }]);
  expect(stopped.steers.filter(row => row.thread === task.thread)).toHaveLength(3);
  expect(stopped.steers.some(row => row.text.includes('原生稍后条目'))).toBe(false);
  await expect(backButton).toHaveAccessibleName('任务');
  await backButton.click();
  const persistedTask = screen.getByRole('link', /^原生导航检查，/);
  await screen.scrollUntilVisible(persistedTask, { direction: 'down' });
  await persistedTask.click();
  await expect(composer).toHaveValue('停止后保留的原生草稿');

  await expect(backButton).toHaveAccessibleName('任务');
  await backButton.click();
  await screen.getByRole('link', '消息', { exact: true }).click();
  await screen.getByRole('link', '管理频道', { exact: true }).click();
  await screen.getByRole('link', '新频道', { exact: true }).click();
  await textbox('频道名称').fill('原生协作频道');
  await textbox('频道目标').fill('手机定向交给本人 Reviewer', { timeout: 30000 });
  await device.dismissKeyboard();
  await expect(keyboard).toHaveCount(0);
  await screen.getByRole('checkbox', 'Native Builder', { exact: true, visible: true }).check();
  await screen.getByRole('checkbox', 'Native Reviewer', { exact: true, visible: true }).check();
  await screen.scrollUntilVisible(builderButton, { direction: 'down' });
  await expect(builderButton).toHaveCount(1);
  await builderButton.click();
  await expect(builderButton).toBeSelected();
  const createChannel = screen.getByRole('button', '创建频道', { exact: true, visible: true });
  await screen.scrollUntilVisible(createChannel, { direction: 'down' });
  await createChannel.click();
  const channelTarget = screen.getByRole('button', /^输入目标 Native Builder · 新频道任务，/, { visible: true });
  await expect(channelTarget).toBeVisible();
  await channelTarget.click();
  await expect(targetSheet).toHaveCount(1);
  const reviewerButton = targetSheet.getByRole('button', 'Native Reviewer', { exact: true, visible: true });
  await expect(reviewerButton).toHaveCount(1);
  await reviewerButton.click();
  await expect(reviewerButton).toBeSelected();
  await targetSheet.getByRole('button', '关闭目标和队列', { exact: true }).click();
  await expect(targetSheet).toHaveCount(0);
  const channelComposer = textbox('发给 Native Reviewer · 新频道任务');
  await channelComposer.fill(JSON.stringify({ phase1: true, label: 'native-workspace-channel', gate: network.gates.channel }), { timeout: 30000 });
  await screen.getByRole('button', '发送', { exact: true, visible: true }).click();
  const directed = await invocation('native-workspace-channel');
  expect(directed.conversation).not.toBe(task.conversation);
  expect(directed.thread).not.toBe(task.thread);
  expect(await network.call({ op: 'workspace_release', gate: 'channel' })).toEqual({ released: 'channel' });
  await expect.poll(async () => (await inspect()).turns.find(row => row.thread === directed.thread)?.state, { timeout: 30000 }).toBe('completed');
  const result = screen.getByText('native-workspace-channel:complete', { exact: true }).first();
  await screen.scrollUntilVisible(result, { direction: 'down' });
  await expect(result).toBeVisible();
  await app.screenshot('workspace-native-channel-directed');
  await writeFile(resolve(artifacts, 'acceptance.json'), `${JSON.stringify({ ui_passed: true, source, simulator, installed_matches_build: true, task_inputs: 3, cancelled_input_invoked: false, directed_channel_result: true })}\n`, { mode: 0o600 });
});
