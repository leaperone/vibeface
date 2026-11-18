// Pure offline support unit entry. No browser, native driver, product process, or
// real fixture filesystem access. Run: node --experimental-vm-modules --test tests/support/artifact-paths.test.mjs
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { insideTaskArtifacts } from './artifact-paths.ts';
import { createFixtureContract } from './fixture-contract.ts';
import { readyDetails } from './fixture-samples.mjs';
import { constants } from 'node:fs';

const repo = fileURLToPath(new URL('../..', import.meta.url));
const artifacts = path.join(repo, '.artifacts/e2e');
const nativeRoot = path.join(repo, '.artifacts/native');
const workspaceRoot = path.join(nativeRoot, 'workspace');

for (const flavor of ['posix', 'win32']) {
  const root = flavor === 'posix' ? '/task/artifacts' : 'C:\\task\\artifacts';
  const p = path[flavor];
  const cases = [
    [root, false], [root + p.sep, false], [p.join(root, '.'), false],
    [p.join(root, 'child'), true], [p.join(root, '..cache'), true],
    [p.join(root, '..cache', 'evidence.json'), true], [root + p.sep + './child/', true],
    [root + p.sep + 'child/../evidence', true], [root + p.sep + '../sibling', false],
    [root + '-sibling/file', false], [p.dirname(root), false],
    ['relative/child', false], ['', false], ['.', false], ['../outside', false],
  ];
  for (const [candidate, expected] of cases) {
    test(`${flavor}: ${candidate}`, () => assert.equal(insideTaskArtifacts(root, candidate, { flavor }), expected));
  }
  test(`${flavor}: explicit root permission and absolute root requirement`, () => {
    assert(insideTaskArtifacts(root, root + p.sep + '.', { flavor, allowRoot: true }));
    assert(!insideTaskArtifacts('relative', root, { flavor, allowRoot: true }));
    assert(!insideTaskArtifacts('', root, { flavor, allowRoot: true }));
  });
}
test('POSIX backslashes are literal names, not traversal separators or Windows drives', () => {
  assert(insideTaskArtifacts('/task', '/task/..\\outside', { flavor: 'posix' }));
  for (const candidate of ['C:\\task\\file', 'C:/task/file', '\\\\server\\share\\file', '//server/share/file', '/else/file']) {
    assert(!insideTaskArtifacts('/task', candidate, { flavor: 'posix' }));
  }
});
test('Windows drives, mixed separators, case, UNC shares and process-relative paths', () => {
  for (const candidate of ['c:/TASK/artifacts/child', 'C:\\task/artifacts\\..cache\\file']) {
    assert(insideTaskArtifacts('C:\\task\\artifacts', candidate, { flavor: 'win32' }));
  }
  for (const candidate of ['D:\\task\\artifacts\\file', 'C:task\\artifacts\\file', '\\task\\artifacts\\file', '/task/artifacts/file', 'C:\\task\\artifacts\\..\\outside', '\\\\?\\C:\\task\\artifacts\\file']) {
    assert(!insideTaskArtifacts('C:\\task\\artifacts', candidate, { flavor: 'win32' }));
  }
  const root = '\\\\server.example\\share\\task';
  assert(insideTaskArtifacts(root, '//SERVER.example/share/task/..cache/file', { flavor: 'win32' }));
  for (const candidate of [root, '\\\\server.example\\other\\task\\file', '\\\\other\\share\\task\\file', root + '-sibling\\file', root + '\\..\\outside', 'C:\\task\\file']) {
    assert(!insideTaskArtifacts(root, candidate, { flavor: 'win32' }));
  }
  assert(insideTaskArtifacts(root, root, { flavor: 'win32', allowRoot: true }));
});
test('default flavor is the host path module', () => {
  assert(insideTaskArtifacts(artifacts, path.join(artifacts, '..cache')));
  assert(!insideTaskArtifacts(artifacts, artifacts));
});

// The loader executes existing fixture definitions with inert module doubles.
// UI test callbacks are registered but never invoked; filesystem calls below
// inspect synthetic values only, and any unexpected I/O fails immediately.
async function loadFixture(name, { fs = {}, env = {}, dependencies = {}, exports = '', fetch } = {}) {
  const definitions = [];
  const registrations = [];
  const inertTest = (...args) => registrations.push(args);
  inertTest.extend = value => { definitions.push(value); return inertTest; };
  const context = vm.createContext({ process: { env, execPath: '/synthetic/node' }, Buffer, URL, TextDecoder, structuredClone, AbortSignal, setTimeout, clearTimeout, fetch });
  const cache = new Map();
  const forbidden = () => { throw new Error('Unexpected offline I/O'); };
  const stub = (specifier, values) => new vm.SyntheticModule(Object.keys(values), function () {
    for (const [key, value] of Object.entries(values)) this.setExport(key, value);
  }, { context, identifier: specifier });
  async function module(file) {
    if (cache.has(file)) return cache.get(file);
    const source = await readFile(file, 'utf8');
    const append = file.endsWith('/' + name) ? exports : '';
    const loaded = new vm.SourceTextModule(stripTypeScriptTypes(source, { mode: 'strip' }) + append, {
      context, identifier: file, initializeImportMeta(meta) { meta.dirname = path.dirname(file); },
    });
    cache.set(file, loaded);
    await loaded.link(async specifier => {
      if (dependencies[specifier]) return stub(specifier, dependencies[specifier]);
      if (specifier === 'node:fs/promises') return stub(specifier, {
        lstat: forbidden, open: forbidden, readFile: forbidden, mkdir: async () => {}, unlink: async () => {},
        mkdtemp: forbidden, writeFile: forbidden, readdir: forbidden, readlink: forbidden, copyFile: forbidden, ...fs,
      });
      if (specifier === 'node:fs') return stub(specifier, { createWriteStream: forbidden, constants });
      if (specifier === 'node:child_process') return stub(specifier, { execFile: forbidden, spawn: forbidden });
      if (specifier === 'node:net') return stub(specifier, { createServer: forbidden });
      if (specifier === '@e2e-dev/web' || specifier === '@e2e-dev/mobile') return stub(specifier, { test: inertTest });
      if (specifier === 'e2e') return stub(specifier, { expect: forbidden, secrets: forbidden });
      if (specifier.startsWith('node:')) return stub(specifier, await import(specifier));
      if (specifier.startsWith('.')) return module(path.resolve(path.dirname(file), specifier.endsWith('.ts') ? specifier : specifier + '.ts'));
      throw new Error(`Unexpected offline import: ${specifier}`);
    });
    return loaded;
  }
  const loaded = await module(path.join(repo, 'tests/e2e', name));
  await loaded.evaluate();
  return { namespace: loaded.namespace, definitions, registrations };
}

test('web node and gate consumers keep the e2e root and admit ..cache', async () => {
  const origin = 'http://127.0.0.1:8000';
  const ready = readyDetails('workspace', { artifacts, origin });
  const { source } = ready.nodes;
  source.directory = path.join(ready.run, '..cache');
  ready.gates.main = path.join(ready.run, '..cache/main');
  const controlJson = JSON.stringify({ url: 'http://127.0.0.1:9000/', token: 'b'.repeat(64) });
  const web = await loadFixture('fixtures.ts', { fs: {
    open: async () => ({
      stat: async () => ({ isFile: () => true, mode: 0o600, size: controlJson.length }),
      readFile: async () => controlJson, close: async () => {},
    }),
  }, fetch: async (_url, options) => Response.json(JSON.parse(options.body).op === 'reset' ? ready : { stopped: true }) });
  const workspace = web.definitions.find(value => value.workspace).workspace;
  let used = 0;
  const use = async value => {
    used++;
    assert.equal(value.source.directory, source.directory);
    assert.deepEqual({ ...value.gates }, ready.gates);
  };
  const run = () => workspace({ app: { baseUrl: origin } }, use);
  await run();
  for (const rejected of [artifacts, artifacts + '-sibling/file', nativeRoot]) {
    const directory = source.directory;
    source.directory = rejected;
    await assert.rejects(run(), /outside the task directory/);
    source.directory = directory;
    ready.gates.main = rejected;
    await assert.rejects(run(), /outside the task directory/);
    ready.gates.main = path.join(ready.run, 'main');
  }
  assert.equal(used, 1);
});

test('Web fixture HTTP success and error bodies have the same byte bound and reject invalid UTF-8', async () => {
  const { namespace } = await loadFixture('fixtures.ts', { exports: '\nexport { responseJSON };' });
  const limit = 64 * 1024;
  const data = '"' + 'é'.repeat((limit - 2) / 2) + '"';
  assert.equal(Buffer.byteLength(data), limit);
  assert.equal(await namespace.responseJSON(new Response(data)), JSON.parse(data));
  await assert.rejects(namespace.responseJSON(new Response(data + ' ', { status: 500 })), /64 KiB/);
  await assert.rejects(namespace.responseJSON(new Response(Buffer.from([34, 255, 34]))));
});

test('native workspace gates retain the narrower workspace root', async () => {
  const origin = 'http://127.0.0.1:8000';
  const ready = readyDetails('workspace', { artifacts: workspaceRoot, origin });
  ready.gates.main = path.join(ready.run, '..cache/main');
  const { definitions, registrations } = await loadFixture('workspace.native.ts', { dependencies: {
    './native-fixture': { bundleManifest: () => assert.fail('No bundle reads'), record: value => value, text: value => value,
      nativeFixture: async (kind, artifacts, use) => {
        assert.equal(kind, 'workspace'); assert.equal(artifacts, workspaceRoot);
        const contract = createFixtureContract({ kind, artifacts, origin });
        const fixture = contract.ready(ready);
        await use({ fixture, invitation: path.join(artifacts, 'invitation'), call: async request => contract.result(request, ready) });
      } },
  } });
  assert.equal(registrations.length, 1);
  const network = definitions[0].network;
  let used = 0;
  const use = async value => { used++; assert.deepEqual({ ...value.gates }, ready.gates); };
  await network({}, use);
  for (const rejected of [workspaceRoot, workspaceRoot + '-sibling/gate', path.join(nativeRoot, 'other/gate')]) {
    ready.gates.main = rejected;
    await assert.rejects(network({}, use), /outside the task directory/);
  }
  assert.equal(used, 1);
});

test('private web control reader refuses symlinks, nonregular files and non-private modes before reading', async () => {
  for (const [isFile, symlink, mode] of [[true, true, 0o600], [false, false, 0o600], [true, false, 0o644]]) {
    let reads = 0;
    const { namespace } = await loadFixture('fixtures.ts', { exports: '\nexport { control };', fs: {
      open: async (_path, flags) => {
        assert(flags & constants.O_NOFOLLOW);
        assert(flags & constants.O_NONBLOCK);
        if (symlink) throw new Error('ELOOP');
        return { stat: async () => ({ isFile: () => isFile, mode, size: 100 }), close: async () => {},
          readFile: async () => { reads++; throw new Error('Must not read rejected control material'); } };
      },
    } });
    await assert.rejects(namespace.control('http://127.0.0.1:8000', 'single'), /ELOOP|Fixture control material is private/);
    assert.equal(reads, 0);
  }
});

test('iOS state and invitation retain explicit caller roots and input sources', async () => {
  const { namespace } = await loadFixture('native-ios-config.ts');
  const env = { VIBEFACE_E2E_IOS_APP: '/synthetic/app', VIBEFACE_E2E_SIMULATOR: '11111111-2222-3333-4444-555555555555' };
  for (const taskRoot of [nativeRoot, workspaceRoot]) {
    const invitation = path.join(taskRoot, '..cache/invitation');
    const inputs = namespace.nativeIosInputs({ taskRoot, invitation, env });
    assert.equal(inputs.invitation, invitation);
    assert.equal(inputs.agentDeviceHome, path.join(taskRoot, 'agent-device'));
    for (const key of ['AGENT_DEVICE_HOME', 'AGENT_DEVICE_STATE_DIR', 'VIBEFACE_E2E_NATIVE_INVITATION_FILE']) {
      const accepted = { ...env, [key]: path.join(taskRoot, '..cache/file') };
      namespace.nativeIosInputs({ taskRoot, invitation, env: accepted });
      for (const rejected of [taskRoot, taskRoot + '-sibling/file', path.dirname(taskRoot)]) {
        assert.throws(() => namespace.nativeIosInputs({ taskRoot, invitation, env: { ...env, [key]: rejected } }), /remains in this task/);
      }
    }
  }
});

test('native invitation consumers retain delivery/workspace roots before process creation', async () => {
  for (const [kind, output, allowedRoot] of [['delivery', path.join(nativeRoot, 'network'), nativeRoot], ['workspace', workspaceRoot, workspaceRoot]]) {
    for (const invitation of [path.join(allowedRoot, '..cache/invitation'), allowedRoot, allowedRoot + '-sibling/invitation']) {
      const { namespace } = await loadFixture('native-fixture.ts', { env: {
        VIBEFACE_E2E_BINARY: '/synthetic/binary', VIBEFACE_E2E_CRYPTO_MODULE: '/synthetic/crypto', VIBEFACE_E2E_NATIVE_INVITATION_FILE: invitation,
      } });
      const accepted = invitation === path.join(allowedRoot, '..cache/invitation');
      await assert.rejects(namespace.nativeFixture(kind, output, async () => assert.fail('No native callback runs')), accepted ? /Unexpected offline I\/O/ : /Only this task can receive an invitation/);
    }
  }
});

test('desktop output stays below workspace artifacts before executing its driver', async () => {
  const root = path.join(repo, '.artifacts');
  for (const output of [path.join(root, '..cache'), root, root + '-sibling/file']) {
    const { namespace } = await loadFixture('native-desktop-fixture.ts', { env: {
      VIBEFACE_E2E_DESKTOP_APP: '/synthetic/app', VIBEFACE_E2E_DESKTOP_RUNTIME: '/synthetic/runtime', VIBEFACE_E2E_DESKTOP_OUTPUT: output,
    } });
    await assert.rejects(namespace.nativeDesktopAcceptance(), output === path.join(root, '..cache') ? /Unexpected offline I\/O/ : /Native evidence stays under this task workspace artifacts/);
  }
});

test('native failure pointers retain optional log admission and bounded state root', async () => {
  const { namespace, registrations } = await loadFixture('native-transfers.e2e.ts', { exports: '\nexport { driverFailurePointer };' });
  assert.equal(registrations.length, 2);
  const state = path.join(nativeRoot, 'agent-device');
  for (const logPath of [path.join(state, '..cache/log'), state, state + '-sibling/log', undefined, 'bad\nlog']) {
    const pointer = namespace.driverFailurePointer({ code: 'TIMEOUT', reason: 'daemon_request_timeout', logPath });
    assert.equal(pointer.code, 'TIMEOUT');
    assert.equal(pointer.reason, 'daemon_request_timeout');
    assert.equal(pointer.log_path, logPath === path.join(state, '..cache/log') ? logPath : undefined);
  }
});
