#!/usr/bin/env node
// Build the Vibeface desktop application around an already sealed runtime bundle.
// Build only: this script never publishes and never rebuilds or re-signs the bundled CLI and Core.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { hash, relativeFiles, readSealedRuntime, verifySealedRuntime } from './sealed-runtime.mjs';
import { requireAuthenticode, signNative } from './windows-signing.mjs';

const repo = fileURLToPath(new URL('../', import.meta.url));
const desktop = path.join(repo, 'apps/desktop');
const NATIVE_UI = { ui: 'gpui', gpui_kit: '0.9.1', gpui: '0.3.12' };
const SCHEMA_VERSION = 2;
const PLATFORMS = {
  'aarch64-apple-darwin': { platform: 'darwin', flag: '--mac', installer: 'dmg', exe: '', signing: 'developer-id-notarized' },
  'x86_64-pc-windows-msvc': { platform: 'win32', flag: '--win', installer: 'exe', exe: '.exe', signing: 'authenticode' },
  'x86_64-unknown-linux-gnu': { platform: 'linux', flag: '--linux', installer: 'AppImage', exe: '', signing: 'platform-checksums', arch: 'x64' },
  'aarch64-unknown-linux-gnu': { platform: 'linux', flag: '--linux', installer: 'AppImage', exe: '', signing: 'platform-checksums', arch: 'arm64' },
};
const BUILD_FIELDS = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit'];

const run = (program, args, options = {}) =>
  execFileSync(program, args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, ...options });
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
  const sealed = readSealedRuntime(directory);
  for (const name of [`vibeface${exe}`, `vibeface-core${exe}`, 'web/index.html', 'web/vibeface_secure.js',
    'web/vibeface_secure_bg.wasm', 'README.md', 'RUNTIME-REQUIREMENTS.md']) {
    assert(sealed.has(name), `the runtime is missing ${name}`);
  }
  return { info, sealed };
}

function readDesktopPackage(version) {
  const manifest = fs.readFileSync(path.join(desktop, 'Cargo.toml'), 'utf8');
  assert(manifest.includes(`version = "${version}"`), 'desktop must join the canonical product version');
  assert(manifest.includes('gpui-kit = "=0.9.1"'), 'desktop must pin the verified GPUI kit');
}

/** Signing identity resolution: real credentials or an explicitly recorded unsigned preview. */
const MAC_SIGNING_VARS = ['VIBEFACE_CODESIGN_IDENTITY', 'VIBEFACE_CODESIGN_TEAM', 'VIBEFACE_NOTARY_KEY',
  'VIBEFACE_NOTARY_KEY_ID', 'VIBEFACE_NOTARY_ISSUER'];

function signingState(target, requested) {
  if (target.endsWith('-unknown-linux-gnu')) {
    assert(!requested, 'Linux has no platform signing identity; use checksums and provenance');
    return { signed: false, signing: 'platform-checksums' };
  }
  const configured = target === 'aarch64-apple-darwin'
    ? MAC_SIGNING_VARS.every(name => process.env[name])
    : Boolean(process.env.WIN_CSC_LINK || process.env.CSC_LINK);
  if (!requested) return { signed: false, signing: 'unsigned-development-preview' };
  assert(configured, `--sign needs a configured signing certificate for ${target}; no signature is ever claimed without one`);
  return { signed: true, signing: PLATFORMS[target].signing };
}

/** The App Store Connect key used for native notarization, from the existing release secrets. */
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

function signMac(application) {
  const executable = path.join(application, 'Contents/MacOS/Vibeface');
  // The sealed runtime was already signed by its producer; never recurse or re-sign it.
  run('codesign', ['--force', '--timestamp', '--options', 'runtime', '--sign', process.env.VIBEFACE_CODESIGN_IDENTITY, executable]);
  run('codesign', ['--force', '--timestamp', '--options', 'runtime', '--entitlements', path.join(desktop, 'build/entitlements.mac.plist'), '--sign', process.env.VIBEFACE_CODESIGN_IDENTITY, application]);
}

function notarize(file) {
  const output = run('xcrun', ['notarytool', 'submit', file, '--key', process.env.VIBEFACE_NOTARY_KEY,
    '--key-id', process.env.VIBEFACE_NOTARY_KEY_ID, '--issuer', process.env.VIBEFACE_NOTARY_ISSUER,
    '--wait', '--timeout', '30m', '--output-format', 'json']);
  assert.equal(JSON.parse(output).status, 'Accepted', 'native desktop notarization was not accepted');
}

function zip(source, destination, mac) {
  if (mac) run('ditto', ['-c', '-k', '--keepParent', source, destination]);
  else run(process.env.PY || (process.platform === 'win32' ? 'python' : 'python3'),
    [path.join(repo, 'scripts/zip-directory.py'), source, destination]);
}

async function build({ target, runtimeDir, output, runtimeArchive, sign }) {
  const platform = PLATFORMS[target];
  assert(platform, `--target must be one of ${Object.keys(PLATFORMS).join(', ')}`);
  assert.equal(process.platform, platform.platform, `${target} desktop applications are built on their own platform`);
  if (platform.platform === 'linux') assert.equal(process.arch, platform.arch, `${target} desktop applications are built on their matching native architecture`);
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
  assert(signed || channel !== 'stable' || target.endsWith('-unknown-linux-gnu'),
    'a stable desktop distribution requires a configured signer except on Linux, which uses checksums and provenance');
  if (signed && target === 'x86_64-pc-windows-msvc') {
    for (const program of ['vibeface.exe', 'vibeface-core.exe']) requireAuthenticode(path.join(runtimeDir, program));
  }

  const stage = path.join(repo, '.artifacts', 'native-desktop-build', target);
  fs.rmSync(stage, { recursive: true, force: true });
  const mac = platform.platform === 'darwin';
  const windows = platform.platform === 'win32';
  const appRoot = path.join(stage, mac ? 'Vibeface.app' : 'application');
  const resources = path.join(appRoot, mac ? 'Contents/Resources' : 'resources');
  const executable = path.join(appRoot, mac ? 'Contents/MacOS/Vibeface' : windows ? 'Vibeface.exe' : 'vibeface');
  let rustflags = run(process.env.PY || (windows ? 'python' : 'python3'), [path.join(repo, 'scripts/release-rustflags.py')]).trim();
  if (windows) rustflags += '\x1f-C\x1ftarget-feature=+crt-static';
  run('cargo', ['build', '--release', '--locked', '--target', target, '-p', 'vibeface-desktop'],
    { cwd: repo, stdio: 'inherit', env: { ...process.env, CARGO_ENCODED_RUSTFLAGS: rustflags } });
  fs.mkdirSync(path.dirname(executable), { recursive: true });
  const targetDirectory = process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(repo, 'target');
  fs.copyFileSync(path.join(targetDirectory, target, 'release', `vibeface-desktop${platform.exe}`), executable);
  fs.chmodSync(executable, 0o755);
  copyRuntime(runtimeDir, path.join(resources, 'runtime'));
  const base = `vibeface-desktop-${version}-${target}`;
  const produced = path.join(stage, 'out');
  fs.mkdirSync(produced, { recursive: true });
  if (mac) {
    const plist = fs.readFileSync(path.join(desktop, 'build/Info.plist'), 'utf8')
      .replaceAll('@VERSION@', version).replaceAll('@MARKETING_VERSION@', version.split('-')[0])
      .replaceAll('@BUILD_VERSION@', version.includes('-') ? `1.0.${version.split('.').at(-1)}` : version);
    fs.writeFileSync(path.join(appRoot, 'Contents/Info.plist'), plist);
    fs.copyFileSync(path.join(desktop, 'build/icon.icns'), path.join(resources, 'icon.icns'));
    if (signed) {
      signMac(appRoot);
      const submission = path.join(stage, 'notarize.zip');
      zip(appRoot, submission, true);
      notarize(submission);
      run('xcrun', ['stapler', 'staple', appRoot]);
    }
    const dmgRoot = path.join(stage, 'dmg-root');
    fs.mkdirSync(dmgRoot);
    run('ditto', [appRoot, path.join(dmgRoot, 'Vibeface.app')]);
    fs.symlinkSync('/Applications', path.join(dmgRoot, 'Applications'));
    run('hdiutil', ['create', '-volname', 'Vibeface', '-srcfolder', dmgRoot, '-format', 'UDZO', path.join(produced, `${base}.dmg`)]);
    if (signed) {
      run('codesign', ['--timestamp', '--sign', process.env.VIBEFACE_CODESIGN_IDENTITY, path.join(produced, `${base}.dmg`)]);
      notarize(path.join(produced, `${base}.dmg`));
      run('xcrun', ['stapler', 'staple', path.join(produced, `${base}.dmg`)]);
    }
  } else if (windows) {
    if (signed) await signNative([executable]);
    run(process.env.MAKENSIS || path.join(process.env['ProgramFiles(x86)'] || 'C:/Program Files (x86)', 'NSIS', 'makensis.exe'),
      [`/DAPP_DIR=${appRoot}`, `/DOUTPUT=${path.join(produced, `${base}.exe`)}`, `/DVERSION=${version}`, ...(signed ? [`/DSIGNING_SCRIPT=${path.join(repo, 'scripts/windows-signing.mjs')}`] : []), path.join(desktop, 'build/installer.nsi')], { stdio: 'inherit' });
    if (signed) await signNative([path.join(produced, `${base}.exe`)]);
  } else {
    fs.copyFileSync(path.join(desktop, 'build/icon.png'), path.join(appRoot, 'vibeface.png'));
    fs.copyFileSync(path.join(desktop, 'build/vibeface.desktop'), path.join(appRoot, 'vibeface.desktop'));
    fs.writeFileSync(path.join(appRoot, 'AppRun'), '#!/bin/sh\ncd "$(dirname "$0")"\nexec ./vibeface "$@"\n', { mode: 0o755 });
    assert(process.env.APPIMAGETOOL, 'APPIMAGETOOL must name the verified native appimagetool');
    run(process.env.APPIMAGETOOL, [appRoot, path.join(produced, `${base}.AppImage`)],
      { env: { ...process.env, ARCH: target.startsWith('aarch64-') ? 'aarch64' : 'x86_64', APPIMAGE_EXTRACT_AND_RUN: '1' }, stdio: 'inherit' });
  }
  zip(appRoot, path.join(produced, `${base}.zip`), mac);
  verifySealedRuntime(path.join(resources, 'runtime'), sealed);
  const payloads = [`${base}.${platform.installer}`, `${base}.zip`];
  fs.mkdirSync(output, { recursive: true });
  const artifacts = {};
  for (const name of payloads) {
    const destination = path.join(output, name);
    assert(!fs.existsSync(destination), `${name} already exists; use a fresh --output directory`);
    fs.copyFileSync(path.join(produced, name), destination);
    assert(fs.statSync(destination).size > 0, `empty native desktop artifact ${name}`);
    artifacts[name] = hash(destination);
    fs.writeFileSync(`${destination}.sha256`, `${artifacts[name]}  ${name}\n`);
  }
  const receipt = {
    schema: SCHEMA_VERSION, app: 'desktop', target, version, tag: `v${version}`, channel,
    source_commit: sourceCommit, ...NATIVE_UI, signed, signing,
    runtime: { archive: path.basename(archive), sha256: hash(archive) }, artifacts,
  };
  writeJson(path.join(output, `${base}.provenance.json`), receipt);
  run('python3', ['-B', path.join(repo, 'scripts/release.py'), 'verify-desktop', '--target', target,
    '--runtime-archive', archive, output], { cwd: repo, stdio: 'inherit' });
  console.log(`${signed ? `Signed (${signing})` : signing === 'platform-checksums' ? 'Checksum/provenance protected' : 'Unsigned development preview'} desktop build for ${target}:`);
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
    assert.throws(() => readRuntime(bundle, target, version, commit), /changed the sealed runtime file web\/index.html/);

    assert.deepEqual(signingState('x86_64-pc-windows-msvc', false), { signed: false, signing: 'unsigned-development-preview' });
    assert.deepEqual(signingState('aarch64-apple-darwin', false), { signed: false, signing: 'unsigned-development-preview' });
    for (const target of ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu']) {
      assert.deepEqual(signingState(target, false), { signed: false, signing: 'platform-checksums' });
      assert.throws(() => signingState(target, true), /Linux has no platform signing identity/);
    }
    for (const platform of Object.keys(PLATFORMS).filter(target => !target.endsWith('-unknown-linux-gnu'))) {
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

(async () => {
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
      await build({
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
})();
