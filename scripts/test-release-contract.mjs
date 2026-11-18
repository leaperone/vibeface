#!/usr/bin/env node
// Offline literal oracles and real CLI fixtures. All mutations stay in a disposable root.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import mutableFs from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { PLATFORMS, NATIVE_UI, parseReleaseVersion, parseReleaseTag, npmDistTag, compareWithinChannel,
  platformFor, programNames, requiredRuntimeFiles, desktopPayloadNames, validateBuildInfoShape } from './release-contract.mjs';
import { readContracts, readOwnVersion, readNativeUi, readNpmTemplate, parseSourceJson } from './release-metadata.mjs';

const repo = fileURLToPath(new URL('../', import.meta.url));
const corpus = JSON.parse(fs.readFileSync(new URL('./fixtures/release-contract.json', import.meta.url), 'utf8'));
const write = (root, name, content) => { const file = path.join(root, name); fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, content); };
const json = (root, name, value) => write(root, name, JSON.stringify(value));
const digest = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const contracts = { data_epoch: 1, update_protocol: 1 };
const commit = 'a'.repeat(40), version = '1.2.3-hourly.2026101909';
const info = target => ({ version, target, source_commit: commit, update_protocol: 1, data_epoch: 1,
  core_version: '2.3.4', core_source_commit: 'b'.repeat(40), app_version: '3.4.5-rc.2', app_source_commit: 'c'.repeat(40) });

function pureTests() {
  for (const expected of corpus.versions) {
    const parsed = parseReleaseVersion(expected.value);
    assert.deepEqual({ channel: parsed.channel, npm: npmDistTag(parsed.channel), parts: parsed.parts.map(String) },
      { channel: expected.channel, npm: expected.npm, parts: expected.parts });
    for (const app of ['core', 'app', 'cli', 'desktop', 'mobile', 'www']) {
      assert.deepEqual(parseReleaseTag(`${app}-v${expected.value}`), { app, version: expected.value, channel: expected.channel });
    }
  }
  for (const invalid of [...corpus.invalid, null, 1, true, {}, []]) assert.throws(() => parseReleaseVersion(invalid));
  for (const invalid of ['v1.2.3', 'server-v1.2.3', 'CLI-v1.2.3', 'cli-v1.2.3\n', 'cli-v1.2.3+build', 'cli-v']) assert.throws(() => parseReleaseTag(invalid));
  for (const [left, right, sign] of corpus.comparisons) assert.equal(Number(compareWithinChannel(left, right) > 0n) - Number(compareWithinChannel(left, right) < 0n), sign);
  for (const [left, right] of corpus.crossChannel) assert.throws(() => compareWithinChannel(left, right), /own CLI channel/);
  assert.throws(() => npmDistTag('latest'));
  assert.deepEqual(Object.keys(PLATFORMS), corpus.platforms.map(platform => platform.target));
  for (const expected of corpus.platforms) {
    const actual = platformFor(expected.target);
    for (const key of ['host', 'exe', 'service', 'installer', 'signing']) assert.equal(actual[key], expected[key]);
    assert.deepEqual(programNames(expected.target), expected.programs);
    assert.deepEqual(desktopPayloadNames('9.8.7', expected.target), [`vibeface-desktop-9.8.7-${expected.target}.${expected.installer}`, `vibeface-desktop-9.8.7-${expected.target}.zip`]);
    assert(Object.isFrozen(actual));
    assert.throws(() => { actual.exe = 'bad'; });
    validateBuildInfoShape(info(expected.target), contracts);
    assert.deepEqual(requiredRuntimeFiles(expected.target, { web: ['index.html'], checksum: true }), [
      'FILES.sha256', ...expected.programs, 'BUILD-INFO.json', 'README.md', 'RUNTIME-REQUIREMENTS.md',
      `scripts/install-node-service.${expected.service}`, 'docs/node.md', 'docs/releasing.md', 'web/index.html']);
  }
  assert(Object.isFrozen(PLATFORMS) && Object.isFrozen(NATIVE_UI));
  for (const target of corpus.unsupportedTargets) { assert.throws(() => platformFor(target)); assert.throws(() => programNames(target)); }
  for (const [field, values] of [['data_epoch', [0, 2, 12, '1', true]], ['update_protocol', [0, 2, 12, '1', true]],
    ['core_version', corpus.invalid], ['app_version', corpus.invalid], ['source_commit', ['a', null]], ['target', corpus.unsupportedTargets]]) {
    for (const value of values) assert.throws(() => validateBuildInfoShape({ ...info('x86_64-unknown-linux-gnu'), [field]: value }, contracts));
  }
  assert.throws(() => validateBuildInfoShape({ ...info('x86_64-unknown-linux-gnu'), legacy: true }, contracts));
  assert.throws(() => validateBuildInfoShape(info('x86_64-unknown-linux-gnu')));
  // Evaluate only the self-contained launcher's literal host selection, without executing it.
  const launcher = fs.readFileSync(path.join(repo, 'apps/cli/npm/bin/vibeface.mjs'), 'utf8');
  assert.equal((launcher.match(/^import .*from /gm) ?? []).length, 3);
  const targetsLiteral = /const targets = (\{[\s\S]*?\n\});/.exec(launcher)?.[1];
  assert(targetsLiteral, 'launcher target facts must remain inspectable');
  assert.equal(JSON.stringify(vm.runInNewContext(`(${targetsLiteral})`)), JSON.stringify({
    'darwin-arm64': 'aarch64-apple-darwin', 'linux-x64': 'x86_64-unknown-linux-gnu',
    'linux-arm64': 'aarch64-unknown-linux-gnu', 'win32-x64': 'x86_64-pc-windows-msvc',
  }));
  for (const expected of corpus.platforms) assert.equal(vm.runInNewContext(`(${targetsLiteral})`)[expected.host], expected.target);
  assert(launcher.includes('glibcVersionRuntime') && launcher.includes("process.platform === 'win32' ? 'vibeface.exe' : 'vibeface'"));
  const selection = launcher.slice(0, launcher.indexOf('const child =')).replace(/^import .*;\n/gm, '').replaceAll('import.meta.url', '"file:///fixture/bin/vibeface.mjs"');
  const launcherHost = (platform, arch, glibc = '2.17') => {
    const errors = [];
    const context = vm.createContext({ process: { platform, arch, report: { getReport: () => ({ header: { glibcVersionRuntime: glibc } }) }, exit: () => { throw new Error('host rejected'); } },
      console: { error: value => errors.push(value) }, existsSync: () => true, fileURLToPath: value => String(value), URL });
    try { return { target: vm.runInContext(selection + '\ntarget;', context), errors }; }
    catch (error) { assert.equal(error.message, 'host rejected'); return { target: null, errors }; }
  };
  for (const expected of corpus.platforms) {
    const [platform, arch] = expected.host.split('-'); assert.equal(launcherHost(platform, arch).target, expected.target);
  }
  for (const [platform, arch, glibc] of [['darwin', 'x64', ''], ['win32', 'arm64', ''], ['linux', 'x64', ''], ['linux', 'arm64', ''], ['freebsd', 'x64', '']]) {
    const rejected = launcherHost(platform, arch, glibc); assert.equal(rejected.target, null); assert.match(rejected.errors[0], /supports macOS Apple Silicon/);
  }
  console.log(`release contract literal tests passed: ${corpus.versions.length} accepted, ${corpus.invalid.length} rejected, Gregorian/BigInt/channel/platform/launcher/build-info.`);
}

// Exercise the real exported adapter with literal memory sources; no copied parser.
function innerAttributeTests() {
  const control = 'pub const UPDATE_PROTOCOL: u32 = 1;\npub const DATA_EPOCH: u32 = 1;\n';
  const originalRead = mutableFs.readFileSync;
  let source;
  mutableFs.readFileSync = (file, ...args) => String(file) === '/memory-contract/crates/control/src/lib.rs'
    ? source : originalRead(file, ...args);
  syncBuiltinESMExports();
  try {
    for (const prefix of ['#[cfg(any())]\n', '#![cfg(any())]\n', '#![cfg_attr(all(), cfg(any()))]\n',
      '#![cfg(any())]\nuse std::fmt;\n', '#![cfg_attr(all(), cfg(any()))]\nuse std::fmt;\n',
      '# /* comment */ ! [cfg(any())]\nuse std::{fmt};\n', '#![cfg(\nany()\n)]\nuse std::fmt;\n',
      '// #![cfg(any())]\n#![cfg(any())]\nuse std::fmt;\n',
      'const TEXT: &str = r#"#![cfg(any())]"#;\n#![cfg_attr(all(), cfg(any()))]\nuse std::fmt;\n']) {
      source = prefix + control;
      assert.throws(() => readContracts('/memory-contract'), /Rust .*unsupported/);
    }
    for (const prefix of ['// #![cfg(any())]\n', '/* #![cfg_attr(all(), cfg(any()))] /* nested */ */\n',
      'const TEXT: &str = "#![cfg(any())]";\n', 'const TEXT: &str = r###"#![cfg_attr(all(), cfg(any()))]"###;\n',
      'const TEXT: &[u8] = br#"#![cfg(any())]"#;\n']) {
      source = prefix + control;
      assert.deepEqual(readContracts('/memory-contract'), contracts);
    }
  } finally { mutableFs.readFileSync = originalRead; syncBuiltinESMExports(); }
  console.log('real exported contracts adapter: 9 conditional attribute negatives, 5 comment/string literals accepted.');
}

function desktopInitializationTests(root) {
  const isolated = path.join(root, 'desktop-no-source');
  for (const name of ['release-contract', 'release-metadata', 'sealed-runtime', 'windows-signing', 'private-control', 'build-desktop', 'verify-desktop']) {
    write(isolated, `scripts/${name}.mjs`, fs.readFileSync(path.join(repo, `scripts/${name}.mjs`)));
  }
  for (const name of ['installer.nsi', 'entitlements.mac.plist']) {
    write(isolated, `apps/desktop/build/${name}`, fs.readFileSync(path.join(repo, `apps/desktop/build/${name}`)));
  }
  for (const name of ['Cargo.lock', 'apps/desktop/Cargo.toml', 'crates/control/src/lib.rs']) assert(!fs.existsSync(path.join(isolated, name)));
  write(isolated, 'count-source-reads.mjs', `import fs from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
const original = fs.readFileSync; let reads = 0;
fs.readFileSync = (file, ...args) => {
  if (/(?:Cargo\\.lock|apps\\/desktop\\/Cargo\\.toml|crates\\/control\\/src\\/lib\\.rs)$/.test(String(file))) {
    reads++; throw new Error('unexpected source read during isolated desktop initialization/self-test');
  }
  return original(file, ...args);
};
syncBuiltinESMExports();
process.on('exit', () => console.log('desktop source reads: ' + reads));
`);
  const env = { PATH: process.env.PATH, LANG: 'C.UTF-8', TZ: 'UTC' };
  for (const name of ['build-desktop', 'verify-desktop']) {
    for (const [args, expected] of [[['--unsupported'], 1], [['--self-test'], 0]]) {
      const result = spawnSync(process.execPath, ['--import', path.join(isolated, 'count-source-reads.mjs'),
        path.join(isolated, `scripts/${name}.mjs`), ...args], { cwd: isolated, env, encoding: 'utf8', timeout: 30000 });
      assert.equal(result.status, expected, `${name} ${args}\n${result.stdout}\n${result.stderr}`);
      assert.match(result.stdout, /desktop source reads: 0(?:\n|$)/);
      if (expected) assert.match(result.stderr, /invalid argument: --unsupported/);
      else assert.match(result.stdout, /self-test passed/);
    }
  }
  console.log('desktop initialization and complete self-tests: 4 real subprocesses, zero source reads without Cargo manifests/lock/control source.');
}

function sourceTests(root) {
  const control = 'pub const UPDATE_PROTOCOL: u32 = 1;\npub const DATA_EPOCH: u32 = 1;\n';
  const manifest = '[package]\nname = "vibeface-desktop"\nversion = "9.9.9"\n[dependencies]\ngpui-kit = "=0.9.1"\n';
  const lock = 'version = 4\n[[package]]\nname = "gpui-kit"\nversion = "0.9.1"\n[[package]]\nname = "gpui-pre"\nversion = "0.3.12"\n';
  write(root, 'crates/control/src/lib.rs', control);
  write(root, 'apps/desktop/Cargo.toml', manifest);
  write(root, 'Cargo.lock', lock);
  assert.deepEqual(readContracts(root), contracts);
  assert.equal(readOwnVersion(root, 'desktop'), '9.9.9');
  assert.deepEqual(readNativeUi(root), { ui: 'gpui', gpui_kit: '0.9.1', gpui: '0.3.12' });
  for (const invalid of [control + control, control.replace('= 1;', '= 1 + 0;'), control.replace('u32', 'u64'),
    control.replace('= 1;', '= 01;'), control.replace('= 1;', '= 4294967296;'), '#[cfg(test)]\n' + control,
    '#[cfg(\n test\n)]\n' + control, '/*\n' + control + '*/', 'mod nested {\n' + control + '}']) {
    write(root, 'crates/control/src/lib.rs', invalid); assert.throws(() => readContracts(root));
  }
  write(root, 'crates/control/src/lib.rs', control);
  for (const invalid of [manifest.replace('version = "9.9.9"', 'version = "9.9.9"\nversion = "9.9.9"'), manifest + '[package]\nversion = "9.9.9"',
    manifest.replace('version = "9.9.9"', 'version.workspace = true'), manifest.replace('version = "9.9.9"', "version = '9.9.9'"), manifest + '["package"]\nversion = "9.9.9"']) {
    write(root, 'apps/desktop/Cargo.toml', invalid); assert.throws(() => readOwnVersion(root, 'desktop'));
  }
  for (const invalid of [manifest.replace('=0.9.1', '^0.9.1'), manifest.replace('gpui-kit =', 'gpui-kit = "=0.9.1"\ngpui-kit ='),
    manifest + '[dependencies.gpui-kit]\nversion = "=0.9.1"']) {
    write(root, 'apps/desktop/Cargo.toml', invalid); assert.throws(() => readNativeUi(root));
  }
  write(root, 'apps/desktop/Cargo.toml', manifest);
  for (const invalid of [lock + '[[package]]\nname = "gpui-pre"\nversion = "0.3.12"\n', lock.replace('"gpui-pre"', '"gpui"'),
    lock.replace('0.3.12', '0.1.14'), lock.replace('version = "0.3.12"', 'version = "0.3.12"\nversion = "0.3.12"'),
    lock.replace('name = "gpui-pre"', "name = 'gpui-pre'"), lock.replace('[[package]]', '[["package"]]'), lock.replace('version = 4', 'version = 4\nversion = 4'), lock + 'checksum = \"x\"\nchecksum = \"y\"\n']) {
    write(root, 'Cargo.lock', invalid); assert.throws(() => readNativeUi(root));
  }
  write(root, 'Cargo.lock', lock);
  json(root, 'apps/mobile/app.json', { expo: { version: '1.0.0', extra: { releaseVersion: '7.8.9-rc.2' } } });
  json(root, 'apps/mobile/package.json', { version: '2.0.0' });
  json(root, 'apps/www/package.json', { version: '0.0.1' });
  assert.equal(readOwnVersion(root, 'mobile'), '7.8.9-rc.2'); assert.equal(readOwnVersion(root, 'www'), '0.0.1');
  for (const invalid of ['{"version":"1.2.3","version":"1.2.4"}', '{"version":"1.2.3","versi\\u006fn":"1.2.4"}', '{"expo":{"extra":{"releaseVersion":"1.2.3","releaseVersion":"1.2.4"}}}']) assert.throws(() => parseSourceJson(invalid), /duplicate JSON key/);
  for (const [app, expected] of Object.entries(corpus.ownVersions)) {
    if (['core', 'cli', 'desktop'].includes(app)) {
      const packageName = app === 'cli' ? 'vibeface' : `vibeface-${app}`;
      write(root, `apps/${app}/Cargo.toml`, `[package]\nname = \"${packageName}\"\nversion = \"${expected}\"\n${app === 'desktop' ? '[dependencies]\ngpui-kit = \"=0.9.1\"\n' : ''}`);
    } else if (app === 'mobile') json(root, 'apps/mobile/app.json', { expo: { version: '8.8.8', extra: { releaseVersion: expected } } });
    else json(root, `apps/${app}/package.json`, { version: expected });
    assert.equal(readOwnVersion(root, app), expected, `own fixture version: ${app}`);
  }
  assert.deepEqual(readContracts(repo), contracts); assert.deepEqual(readNativeUi(repo), NATIVE_UI);
  const templateSource = fs.readFileSync(path.join(repo, 'apps/cli/npm/package.json'), 'utf8');
  write(root, 'apps/cli/npm/package.json', templateSource);
  assert.equal(readNpmTemplate(root).version, '0.0.0');
  assert.deepEqual(readNpmTemplate(root).os, ['darwin', 'linux', 'win32']);
  assert.deepEqual(readNpmTemplate(root).cpu, ['arm64', 'x64']);
  assert.deepEqual(readNpmTemplate(root).engines, { node: '>=22' });
  write(root, 'apps/cli/npm/package.json', templateSource.replace('\"version\": \"0.0.0\"', '\"version\": \"0.0.0\", \"version\": \"1.2.3\"'));
  assert.throws(() => readNpmTemplate(root), /duplicate JSON key/);
  console.log('release source adapter tests passed: explicit own versions, duplicate/unsupported Rust/TOML/JSON and exact gpui-pre/gpui-kit lock records.');
}

function cliFixtures(root) {
  const copy = ['scripts/release-contract.mjs', 'scripts/release-metadata.mjs', 'scripts/sealed-runtime.mjs', 'scripts/windows-signing.mjs', 'scripts/private-control.mjs',
    'scripts/build-desktop.mjs', 'scripts/verify-desktop.mjs', 'scripts/package-npm.mjs', 'scripts/package-node-web.json', 'scripts/publish-npm.mjs',
    'apps/cli/npm/bin/vibeface.mjs', 'apps/cli/npm/package.json', 'apps/cli/npm/README.md'];
  for (const name of copy) write(root, name, fs.readFileSync(path.join(repo, name)));
  const cliManifest = value => `[package]\nname = "vibeface"\nversion = "${value}"\n`;
  const desktopManifest = value => `[package]\nname = "vibeface-desktop"\nversion = "${value}"\n[dependencies]\ngpui-kit = "=0.9.1"\n`;
  write(root, 'apps/cli/Cargo.toml', cliManifest(version));
  write(root, 'apps/desktop/Cargo.toml', desktopManifest('9.9.9-hourly.2026110712'));
  // No inherited credentials or signing configuration. Fake tools reject unexpected operations.
  const tools = path.join(root, 'tools'); fs.mkdirSync(tools);
  write(root, 'tools/git', `#!${process.execPath}\nconst fs = require('node:fs'); const args = process.argv.slice(2); fs.appendFileSync(${JSON.stringify(path.join(root, 'external.log'))}, args.join(' ')+'\\n'); if (JSON.stringify(args) === '["rev-parse","HEAD"]') console.log('${commit}'); else { console.error('unexpected git operation'); process.exit(97); }\n`);
  fs.chmodSync(path.join(tools, 'git'), 0o755);
  for (const name of ['cargo', 'rustc', 'gh', 'codesign', 'xcrun', 'hdiutil', 'spctl', 'makensis']) {
    write(root, `tools/${name}`, `#!${process.execPath}\nrequire('node:fs').appendFileSync(${JSON.stringify(path.join(root, 'forbidden.log'))}, '${name}\\n'); process.exit(98);\n`);
    fs.chmodSync(path.join(tools, name), 0o755);
  }
  write(root, 'deny-network.mjs', "import net from 'node:net'; import tls from 'node:tls'; import http from 'node:http'; import https from 'node:https'; const deny = () => { throw new Error('network forbidden in release fixture'); }; globalThis.fetch = deny; net.connect = net.createConnection = tls.connect = http.request = http.get = https.request = https.get = deny;\n");
  const env = { PATH: `${tools}${path.delimiter}${process.env.PATH}`, HOME: path.join(root, 'home'), LANG: 'C.UTF-8',
    npm_config_offline: 'true', npm_config_audit: 'false', npm_config_fund: 'false', npm_config_cache: path.join(root, 'npm-cache'),
    NODE_OPTIONS: `--import=${path.join(root, 'deny-network.mjs')}` };
  const cli = (name, args, expected = 0, extra = {}) => {
    const result = spawnSync(process.execPath, [path.join(root, `scripts/${name}.mjs`), ...args], { cwd: root, env: { ...env, ...extra }, encoding: 'utf8', timeout: 30000 });
    assert.equal(result.status, expected, `${name} ${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
    return result;
  };
  const target = 'x86_64-unknown-linux-gnu';
  const invalidDates = corpus.invalid.filter(value => value.startsWith('1.2.3-hourly.') && !value.endsWith('\n'));
  for (const value of invalidDates) {
    write(root, 'apps/cli/Cargo.toml', cliManifest(value)); write(root, 'apps/desktop/Cargo.toml', desktopManifest(value));
    assert.match(cli('package-npm', ['--input', root, '--source-commit', commit], 1).stderr, /invalid (hourly date|component release version)/);
    if (process.platform === 'linux' && process.arch === 'x64') {
      assert.match(cli('build-desktop', ['--target', target, '--runtime-dir', root, '--output', root], 1).stderr, /invalid (hourly date|component release version)/);
    }
    assert.match(cli('verify-desktop', ['--target', target, '--input', root], 1).stderr, /invalid (hourly date|component release version)/);
    assert.match(cli('publish-npm', ['artifact'], 1, { GITHUB_REPOSITORY: 'leaperone/vibeface', RELEASE_TAG: `cli-v${value}`, GITHUB_REF: `refs/tags/cli-v${value}` }).stderr, /invalid (hourly date|component release version)/);
  }
  assert(!fs.existsSync(path.join(root, 'external.log')), 'invalid dates fail before external tools');
  assert(!fs.existsSync(path.join(root, 'forbidden.log')), 'invalid dates never start builds/signatures/network tools');
  write(root, 'apps/cli/Cargo.toml', cliManifest(version)); write(root, 'apps/desktop/Cargo.toml', desktopManifest('9.9.9-hourly.2026110712'));
  const input = path.join(root, 'native-input'); fs.mkdirSync(input);
  const seal = directory => {
    const entries = fs.readdirSync(directory, { recursive: true }).filter(name => name !== 'FILES.sha256' && fs.statSync(path.join(directory, name)).isFile()).sort();
    fs.writeFileSync(path.join(directory, 'FILES.sha256'), entries.map(name => `${digest(path.join(directory, name))}  ${name}\n`).join(''));
  };
  for (const expected of corpus.platforms) {
    const base = `vibeface-${version}-${expected.target}`;
    const directory = path.join(root, 'native-source', base);
    for (const name of [...expected.programs, 'README.md', 'RUNTIME-REQUIREMENTS.md', `scripts/install-node-service.${expected.service}`, 'docs/node.md', 'docs/releasing.md',
      'web/_shell.html', 'web/index.html', 'web/_redirects', 'web/_headers', 'web/.vibeface-build.json', 'web/vibeface_secure.js']) write(directory, name, 'fixture');
    for (const name of expected.programs) fs.chmodSync(path.join(directory, name), 0o755);
    write(directory, 'web/vibeface_secure_bg.wasm', Buffer.from([0, 97, 115, 109])); json(directory, 'BUILD-INFO.json', info(expected.target)); seal(directory);
    const archive = path.join(input, `${base}.tar.gz`);
    execFileSync('tar', ['-czf', archive, '-C', path.dirname(directory), base], { env });
    write(input, `${base}.tar.gz.sha256`, `${digest(archive)}  ${base}.tar.gz\n`);
  }
  const output = path.join(root, 'npm-output');
  cli('package-npm', ['--input', input, '--output', output, '--source-commit', commit]);
  const archive = path.join(output, `leaperone-vibeface-${version}.tgz`);
  cli('package-npm', ['--verify', archive, '--output', output, '--source-commit', commit]);
  const original = fs.readFileSync(archive); fs.appendFileSync(archive, 'tampered');
  assert.match(cli('package-npm', ['--verify', archive, '--output', output, '--source-commit', commit], 1).stderr, /SHA256 mismatch/);
  fs.writeFileSync(archive, original);
  // Resealed malicious package: matching outer receipt cannot excuse an invalid native payload.
  const altered = path.join(root, 'altered'); fs.mkdirSync(altered); execFileSync('tar', ['-xzf', archive, '-C', altered], { env });
  const native = path.join(altered, 'package/vendor/x86_64-unknown-linux-gnu');
  write(native, 'web/vibeface_secure_bg.wasm', 'not-wasm'); seal(native);
  execFileSync('tar', ['--owner=0', '--group=0', '--numeric-owner', '-czf', archive, '-C', altered, 'package'], { env });
  json(root, path.relative(root, `${archive}.json`), { version, source_commit: commit, filename: path.basename(archive), size: fs.statSync(archive).size,
    sha256: digest(archive), integrity: `sha512-${createHash('sha512').update(fs.readFileSync(archive)).digest('base64')}` });
  assert.match(cli('package-npm', ['--verify', archive, '--output', output, '--source-commit', commit], 1).stderr, /invalid WebAssembly/);
  if (process.platform === 'linux' && process.arch === 'x64') desktopFixture(root, cli, seal, env);
  cli('publish-npm', ['--self-test']);
  assert(!fs.existsSync(path.join(root, 'forbidden.log')));
  console.log('real CLI fixtures passed: invalid dates before external operations in all four entrypoints; real npm tgz/receipt/resealed payload; Linux desktop ZIP/AppImage checks; fake publisher IO.');
}

function desktopFixture(root, cli, seal, env) {
  const target = 'x86_64-unknown-linux-gnu', desktopVersion = '9.9.9-hourly.2026110712';
  const base = `vibeface-desktop-${desktopVersion}-${target}`;
  const source = path.join(root, 'desktop-source');
  const native = Buffer.alloc(512); Buffer.from([127, 69, 76, 70, 2, 1]).copy(native); native.writeUInt16LE(62, 18);
  write(source, 'vibeface', native);
  write(source, 'vibeface.desktop', 'Exec=vibeface %u\nMimeType=x-scheme-handler/vibeface;\n');
  json(source, 'native-dependencies.json', { target, libraries: ['libc.so.6'], minimum_glibc: '2.17' });
  json(source, 'resources/desktop-build.json', { version: desktopVersion, source_commit: commit });
  const runtime = path.join(source, 'resources/runtime');
  for (const name of ['vibeface', 'vibeface-core', 'web/index.html', 'web/vibeface_secure.js', 'web/vibeface_secure_bg.wasm']) write(runtime, name, 'runtime');
  json(runtime, 'BUILD-INFO.json', info(target)); seal(runtime);
  const input = path.join(root, 'desktop-input'); fs.mkdirSync(input);
  const portable = path.join(input, `${base}.zip`);
  execFileSync('zip', ['-q', '-r', portable, '.'], { cwd: source, env });
  write(root, 'image-fixture.cjs', `const fs = require('node:fs'); if (process.argv[2] !== '--appimage-extract') process.exit(99); fs.cpSync(${JSON.stringify(source)}, 'squashfs-root', { recursive: true });\n`);
  // Paths are generated below os.tmpdir; quote them as shell words for the executable shim.
  const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
  write(input, `${base}.AppImage`, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root, 'image-fixture.cjs'))} "$@"\n`);
  const receipt = { schema: 2, app: 'desktop', target, version: desktopVersion, tag: `desktop-v${desktopVersion}`, channel: 'hourly', source_commit: commit,
    ui: 'gpui', gpui_kit: '0.9.1', gpui: '0.3.12', signed: false, signing: 'platform-checksums',
    runtime: { version, source_commit: commit, archive: `vibeface-${version}-${target}.tar.gz`, sha256: 'b'.repeat(64) }, artifacts: {} };
  for (const name of [`${base}.zip`, `${base}.AppImage`]) {
    receipt.artifacts[name] = digest(path.join(input, name)); write(input, `${name}.sha256`, `${receipt.artifacts[name]}  ${name}\n`);
  }
  json(input, `${base}.provenance.json`, receipt);
  cli('verify-desktop', ['--target', target, '--input', input]);
  fs.appendFileSync(portable, 'tampered');
  assert.match(cli('verify-desktop', ['--target', target, '--input', input], 1).stderr, /payload digest mismatch/);
}

assert(process.argv.length === 2 || (process.argv.length === 3 && process.argv[2] === '--cli-fixtures'), 'expected no arguments or --cli-fixtures');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'release-contract-'));
try { pureTests(); innerAttributeTests(); desktopInitializationTests(root); sourceTests(root); if (process.argv[2]) cliFixtures(root); }
finally { fs.rmSync(root, { recursive: true, force: true }); }
