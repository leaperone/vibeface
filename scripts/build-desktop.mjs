#!/usr/bin/env node
// Build the Vibeface desktop application around an already released runtime bundle.
// Build only: this script never publishes and never rebuilds or re-signs the bundled CLI and Core.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { requireAuthenticode } from './windows-signing.mjs';

const repo = fileURLToPath(new URL('../', import.meta.url));
const desktop = path.join(repo, 'apps/desktop');
const ELECTRON_VERSION = '45.2.3';
const SCHEMA_VERSION = 1;
const PLATFORMS = {
  'aarch64-apple-darwin': { platform: 'darwin', flag: '--mac', installer: 'dmg', exe: '', signing: 'developer-id-notarized' },
  'x86_64-pc-windows-msvc': { platform: 'win32', flag: '--win', installer: 'exe', exe: '.exe', signing: 'authenticode' },
};
const BUILD_FIELDS = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit'];
const HOST_OUTPUTS = ['dist/main/index.cjs', 'dist/preload/launcher.cjs', 'dist/launcher/index.html'];

const run = (program, args, options = {}) =>
  execFileSync(program, args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, ...options });
const hash = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const writeJson = (file, value) => fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);

function controlConstant(name) {
  const source = fs.readFileSync(path.join(repo, 'crates/control/src/lib.rs'), 'utf8');
  const value = Number(new RegExp(`pub const ${name}: u32 = (\\d+);`).exec(source)?.[1]);
  assert(Number.isSafeInteger(value), `missing ${name}`);
  return value;
}

function productVersion() {
  const version = /^version = "([^"]+)"$/m.exec(fs.readFileSync(path.join(repo, 'apps/cli/Cargo.toml'), 'utf8'))?.[1];
  assert(version && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-(nightly\.\d{8}\.[1-9]\d*|rc\.[1-9]\d*))?$/.test(version),
    'the canonical product version comes from the CLI package');
  return version;
}

function relativeFiles(directory) {
  return fs.readdirSync(directory, { recursive: true, withFileTypes: true })
    .filter(entry => entry.isFile())
    .map(entry => path.relative(directory, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'))
    .sort();
}

/** Read a sealed runtime bundle and prove every byte still matches its own FILES.sha256. */
function readRuntime(directory, target, version, sourceCommit) {
  const { exe } = PLATFORMS[target];
  assert.equal(path.basename(directory), `vibeface-${version}-${target}`, 'the runtime directory must be the released CLI bundle for this platform');
  const info = JSON.parse(fs.readFileSync(path.join(directory, 'BUILD-INFO.json'), 'utf8'));
  assert.deepEqual(Object.keys(info).sort(), [...BUILD_FIELDS].sort(), 'unsupported build info fields');
  assert.deepEqual([info.version, info.target, info.source_commit, info.update_protocol, info.data_epoch],
    [version, target, sourceCommit, controlConstant('UPDATE_PROTOCOL'), controlConstant('DATA_EPOCH')],
    'the bundled runtime is not this platform, version and source');
  for (const field of ['core_version', 'app_version']) assert.equal(info[field], version, 'mixed component versions in the runtime');
  for (const field of ['core_source_commit', 'app_source_commit']) assert.equal(info[field], sourceCommit, 'mixed component sources in the runtime');
  const sealed = new Map();
  for (const line of fs.readFileSync(path.join(directory, 'FILES.sha256'), 'utf8').trimEnd().split('\n')) {
    const match = /^([a-f0-9]{64})  ([a-zA-Z0-9._/-]+)$/.exec(line);
    assert(match && match[2] !== 'FILES.sha256' && !sealed.has(match[2]), 'invalid or duplicate runtime checksum entry');
    assert(match[2].split('/').every(part => part && part !== '.' && part !== '..'), 'unsafe runtime checksum path');
    sealed.set(match[2], match[1]);
  }
  assert.deepEqual(relativeFiles(directory).filter(name => name !== 'FILES.sha256'), [...sealed.keys()].sort(),
    'the runtime contains files its checksums do not cover');
  for (const [name, expected] of sealed) {
    const file = path.join(directory, name);
    assert(!fs.lstatSync(file).isSymbolicLink(), 'the runtime must not contain links');
    assert.equal(hash(file), expected, `runtime content changed: ${name}`);
  }
  for (const name of [`vibeface${exe}`, `vibeface-core${exe}`, 'web/index.html', 'web/vibeface_secure.js',
    'web/vibeface_secure_bg.wasm', 'README.md', 'RUNTIME-REQUIREMENTS.md']) {
    assert(sealed.has(name), `the runtime is missing ${name}`);
  }
  return { info, sealed };
}

function readDesktopPackage(version) {
  const metadata = JSON.parse(fs.readFileSync(path.join(desktop, 'package.json'), 'utf8'));
  assert.equal(metadata.version, version, 'the desktop package joins the canonical product version');
  assert.equal(metadata.main, 'dist/main/index.cjs', 'the desktop package must point at the compiled main entry');
  assert.equal((metadata.devDependencies ?? {}).electron, ELECTRON_VERSION, `the desktop package must pin electron ${ELECTRON_VERSION}`);
  for (const script of ['preinstall', 'install', 'postinstall', 'prepare']) {
    assert(!(metadata.scripts ?? {})[script], 'the desktop package must not run install lifecycle scripts');
  }
  for (const output of HOST_OUTPUTS) {
    assert(fs.statSync(path.join(desktop, output)).size > 0, `missing desktop build output: ${output}; run pnpm --dir apps/desktop build`);
  }
  return metadata;
}

/** Signing identity resolution: real credentials or an explicitly recorded unsigned preview. */
const MAC_SIGNING_VARS = ['VIBEFACE_CODESIGN_IDENTITY', 'VIBEFACE_CODESIGN_TEAM', 'VIBEFACE_NOTARY_KEY',
  'VIBEFACE_NOTARY_KEY_ID', 'VIBEFACE_NOTARY_ISSUER'];

function signingState(target, requested) {
  const configured = target === 'aarch64-apple-darwin'
    ? MAC_SIGNING_VARS.every(name => process.env[name])
    : Boolean(process.env.WIN_CSC_LINK || process.env.CSC_LINK);
  if (!requested) return { signed: false, signing: 'unsigned-development-preview' };
  assert(configured, `--sign needs a configured signing certificate for ${target}; no signature is ever claimed without one`);
  return { signed: true, signing: PLATFORMS[target].signing };
}

/** The App Store Connect key electron-builder notarizes with, from the existing release secrets. */
function notaryEnvironment(target, signed) {
  if (!signed || target !== 'aarch64-apple-darwin') return {};
  return {
    APPLE_API_KEY: process.env.VIBEFACE_NOTARY_KEY,
    APPLE_API_KEY_ID: process.env.VIBEFACE_NOTARY_KEY_ID,
    APPLE_API_ISSUER: process.env.VIBEFACE_NOTARY_ISSUER,
  };
}

function copyRuntime(source, destination) {
  fs.rmSync(destination, { recursive: true, force: true });
  fs.mkdirSync(destination, { recursive: true });
  fs.cpSync(source, destination, { recursive: true, preserveTimestamps: true, dereference: false, verbatimSymlinks: true });
}

/** The sealed runtime inside the packaged application, read back after signing. */
function packagedRuntime(outputRoot, target) {
  const candidates = target === 'aarch64-apple-darwin'
    ? [path.join(outputRoot, 'mac-arm64/Vibeface.app/Contents/Resources/runtime')]
    : [path.join(outputRoot, 'win-unpacked/resources/runtime')];
  const found = candidates.find(candidate => fs.existsSync(candidate));
  assert(found, `cannot find the packaged runtime under ${outputRoot}`);
  return found;
}

function verifySealedRuntime(directory, sealed) {
  assert.deepEqual(relativeFiles(directory).filter(name => name !== 'FILES.sha256'), [...sealed.keys()].sort(),
    'packaging changed the set of runtime files');
  for (const [name, expected] of sealed) {
    assert.equal(hash(path.join(directory, name)), expected, `packaging or signing changed the sealed runtime file ${name}`);
  }
}

function build({ target, runtimeDir, output, runtimeArchive, sign }) {
  const platform = PLATFORMS[target];
  assert(platform, `--target must be one of ${Object.keys(PLATFORMS).join(', ')}`);
  assert.equal(process.platform, platform.platform, `${target} desktop applications are built on their own platform`);
  const version = productVersion();
  assert.equal(run('git', ['status', '--porcelain'], { cwd: repo }).trim(), '', 'desktop packaging source must be clean');
  const sourceCommit = run('git', ['rev-parse', 'HEAD'], { cwd: repo }).trim();
  assert(/^[0-9a-f]{40}$/.test(sourceCommit), 'a full source commit is required');

  const { sealed } = readRuntime(runtimeDir, target, version, sourceCommit);
  readDesktopPackage(version);
  const archive = runtimeArchive ?? path.join(path.dirname(runtimeDir), `vibeface-${version}-${target}.tar.gz`);
  assert(fs.statSync(archive).isFile(), `the released runtime archive is required beside --runtime-dir or named with --runtime-archive`);
  const { signed, signing } = signingState(target, sign);
  const channel = version.includes('-nightly.') ? 'nightly' : version.includes('-rc.') ? 'rc' : 'stable';
  assert(signed || channel !== 'stable', 'a stable desktop distribution requires a configured signer');
  if (signed && target === 'x86_64-pc-windows-msvc') {
    for (const program of ['vibeface.exe', 'vibeface-core.exe']) requireAuthenticode(path.join(runtimeDir, program));
  }


  const stage = path.join(desktop, '.build');
  fs.rmSync(stage, { recursive: true, force: true });
  const runtimeStage = path.join(stage, 'runtime');
  const electronOut = path.join(stage, 'out');
  copyRuntime(runtimeDir, runtimeStage);
  verifySealedRuntime(runtimeStage, sealed);

  const builderCli = createRequire(path.join(desktop, 'package.json')).resolve('electron-builder/cli.js');
  run(process.execPath, [builderCli, '--config', path.join(desktop, 'electron-builder.cjs'),
    platform.flag, '--publish', 'never'], {
    cwd: desktop,
    stdio: 'inherit',
    env: {
      ...process.env,
      VIBEFACE_DESKTOP_TARGET: target,
      VIBEFACE_DESKTOP_RUNTIME_STAGE: runtimeStage,
      VIBEFACE_DESKTOP_OUT: electronOut,
      VIBEFACE_DESKTOP_SIGN: signed ? '1' : '',
      CSC_IDENTITY_AUTO_DISCOVERY: signed ? 'true' : 'false',
      ...notaryEnvironment(target, signed),
    },
  });
  verifySealedRuntime(packagedRuntime(electronOut, target), sealed);

  const base = `vibeface-desktop-${version}-${target}`;
  const payloads = [`${base}.${platform.installer}`, `${base}.zip`];
  fs.mkdirSync(output, { recursive: true });
  const artifacts = {};
  for (const name of payloads) {
    const produced = path.join(electronOut, name);
    assert(fs.statSync(produced).size > 0, `electron-builder did not produce ${name}`);
    const destination = path.join(output, name);
    assert(!fs.existsSync(destination), `${name} already exists; use a fresh --output directory`);
    fs.copyFileSync(produced, destination);
    artifacts[name] = hash(destination);
    fs.writeFileSync(`${destination}.sha256`, `${artifacts[name]}  ${name}\n`);
  }
  if (signed && target === 'x86_64-pc-windows-msvc') {
    requireAuthenticode(path.join(electronOut, 'win-unpacked/Vibeface.exe'));
    requireAuthenticode(path.join(output, `${base}.exe`));
  }
  const receipt = {
    schema: SCHEMA_VERSION, app: 'desktop', target, version, tag: `v${version}`, channel,
    source_commit: sourceCommit, electron: ELECTRON_VERSION, signed, signing,
    runtime: { archive: path.basename(archive), sha256: hash(archive) }, artifacts,
  };
  writeJson(path.join(output, `${base}.provenance.json`), receipt);
  run('python3', ['-B', path.join(repo, 'scripts/release.py'), 'verify-desktop', '--target', target,
    '--runtime-archive', archive, output], { cwd: repo, stdio: 'inherit' });
  console.log(`${signed ? `Signed (${signing})` : 'Unsigned development preview'} desktop build for ${target}:`);
  for (const name of payloads) console.log(`  ${path.join(output, name)}`);
  return receipt;
}

function selfTest() {
  const artifacts = path.join(repo, '.artifacts');
  fs.mkdirSync(artifacts, { recursive: true });
  const scratch = fs.mkdtempSync(path.join(artifacts, 'desktop-build-'));
  try {
    const version = '9.9.9-nightly.20261107.1';
    const commit = 'a'.repeat(40);
    const target = 'aarch64-apple-darwin';
    const bundle = path.join(scratch, `vibeface-${version}-${target}`);
    const entries = {
      vibeface: '#!/bin/sh\necho "vibeface fixture"\n',
      'vibeface-core': '#!/bin/sh\necho "vibeface-core fixture"\n',
      'README.md': 'fixture\n',
      'RUNTIME-REQUIREMENTS.md': 'fixture\n',
      'web/index.html': '<!doctype html>fixture',
      'web/vibeface_secure.js': 'export const fixture = true;\n',
      'web/vibeface_secure_bg.wasm': '\u0000asm',
    };
    for (const [name, content] of Object.entries(entries)) {
      fs.mkdirSync(path.dirname(path.join(bundle, name)), { recursive: true });
      fs.writeFileSync(path.join(bundle, name), content);
    }
    writeJson(path.join(bundle, 'BUILD-INFO.json'), {
      version, target, source_commit: commit, update_protocol: controlConstant('UPDATE_PROTOCOL'),
      data_epoch: controlConstant('DATA_EPOCH'), core_version: version, core_source_commit: commit,
      app_version: version, app_source_commit: commit,
    });
    const seal = () => fs.writeFileSync(path.join(bundle, 'FILES.sha256'),
      relativeFiles(bundle).filter(name => name !== 'FILES.sha256')
        .map(name => `${hash(path.join(bundle, name))}  ${name}\n`).join(''));
    seal();
    const sealed = readRuntime(bundle, target, version, commit).sealed;
    assert.equal(sealed.size, Object.keys(entries).length + 1, 'the sealed set covers every runtime file');

    const packaged = path.join(scratch, 'packaged');
    copyRuntime(bundle, packaged);
    verifySealedRuntime(packaged, sealed);
    fs.writeFileSync(path.join(packaged, 'vibeface'), '#!/bin/sh\necho resigned\n');
    assert.throws(() => verifySealedRuntime(packaged, sealed), /changed the sealed runtime file vibeface/);
    fs.copyFileSync(path.join(bundle, 'vibeface'), path.join(packaged, 'vibeface'));
    fs.writeFileSync(path.join(packaged, 'extra.txt'), 'x');
    assert.throws(() => verifySealedRuntime(packaged, sealed), /changed the set of runtime files/);

    for (const [label, mutate, expected] of [
      ['a foreign source commit', () => readRuntime(bundle, target, version, 'b'.repeat(40)), /platform, version and source/],
      ['another platform', () => readRuntime(bundle, 'x86_64-pc-windows-msvc', version, commit), /released CLI bundle for this platform/],
      ['another version', () => readRuntime(bundle, target, '1.2.3', commit), /released CLI bundle for this platform/],
    ]) {
      assert.throws(mutate, expected, label);
    }
    const info = JSON.parse(fs.readFileSync(path.join(bundle, 'BUILD-INFO.json'), 'utf8'));
    for (const [label, change, expected] of [
      ['an extra build info field', { legacy: true }, /unsupported build info fields/],
      ['a mixed component version', { core_version: '1.2.3' }, /mixed component versions/],
      ['a mixed component source', { app_source_commit: 'c'.repeat(40) }, /mixed component sources/],
      ['another data epoch', { data_epoch: 2 }, /platform, version and source/],
    ]) {
      writeJson(path.join(bundle, 'BUILD-INFO.json'), { ...info, ...change });
      seal();
      assert.throws(() => readRuntime(bundle, target, version, commit), expected, label);
    }
    writeJson(path.join(bundle, 'BUILD-INFO.json'), info);
    seal();
    fs.appendFileSync(path.join(bundle, 'web/index.html'), 'tampered');
    assert.throws(() => readRuntime(bundle, target, version, commit), /runtime content changed: web\/index.html/);

    assert.deepEqual(signingState('x86_64-pc-windows-msvc', false), { signed: false, signing: 'unsigned-development-preview' });
    assert.deepEqual(signingState('aarch64-apple-darwin', false), { signed: false, signing: 'unsigned-development-preview' });
    for (const platform of Object.keys(PLATFORMS)) {
      assert.throws(() => signingState(platform, true), /configured signing certificate/, platform);
    }
    // A partial macOS credential set must never pass as signed, and never reaches notarization.
    for (const absent of MAC_SIGNING_VARS) {
      const environment = Object.fromEntries(MAC_SIGNING_VARS.filter(name => name !== absent).map(name => [name, 'set']));
      const restore = { ...process.env };
      Object.assign(process.env, environment);
      try {
        assert.throws(() => signingState('aarch64-apple-darwin', true), /configured signing certificate/, absent);
        assert.deepEqual(notaryEnvironment('aarch64-apple-darwin', false), {}, 'an unsigned build never reads notary keys');
      } finally {
        process.env = restore;
      }
    }
    console.log('desktop build self-test passed: sealed runtime verification, cohort identity, packaging immutability and honest signing status.');
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

try {
  const args = process.argv.slice(2);
  if (args.length === 1 && args[0] === '--self-test') {
    selfTest();
  } else {
    const options = new Map();
    let sign = false;
    while (args.length) {
      const flag = args.shift();
      if (flag === '--sign') { sign = true; continue; }
      assert(['--target', '--runtime-dir', '--output', '--runtime-archive'].includes(flag) && args.length && !options.has(flag),
        `invalid argument: ${flag}`);
      options.set(flag, args.shift());
    }
    for (const flag of ['--target', '--runtime-dir', '--output']) assert(options.has(flag), `${flag} is required`);
    build({
      target: options.get('--target'),
      runtimeDir: path.resolve(options.get('--runtime-dir')),
      output: path.resolve(options.get('--output')),
      runtimeArchive: options.has('--runtime-archive') ? path.resolve(options.get('--runtime-archive')) : undefined,
      sign,
    });
  }
} catch (error) {
  console.error(`vibeface desktop build: ${error.message}`);
  process.exitCode = 1;
}
