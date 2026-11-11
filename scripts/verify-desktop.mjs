#!/usr/bin/env node
// Verify a built desktop asset set from the artifacts themselves: payload checksums, the provenance
// receipt, the packaged application layout, and the sealed runtime inside the shipped application.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { hash, relativeFiles, readSealedRuntime } from './sealed-runtime.mjs';
import { inspectAuthenticode, requireAuthenticode } from './windows-signing.mjs';

const repo = fileURLToPath(new URL('../', import.meta.url));
const NATIVE_UI = { ui: 'gpui', gpui_kit: '0.9.1', gpui: '0.3.12' };
const SCHEMA_VERSION = 2;
const SIGNING_STATES = ['developer-id-notarized', 'authenticode', 'unsigned-development-preview', 'platform-checksums'];
const PLATFORMS = {
  'aarch64-apple-darwin': { installer: 'dmg', exe: '', signing: 'developer-id-notarized' },
  'x86_64-pc-windows-msvc': { installer: 'exe', exe: '.exe', signing: 'authenticode' },
  'x86_64-unknown-linux-gnu': { installer: 'AppImage', exe: '', signing: 'platform-checksums' },
  'aarch64-unknown-linux-gnu': { installer: 'AppImage', exe: '', signing: 'platform-checksums' },
};

const run = (program, args, options = {}) =>
  execFileSync(program, args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, ...options });

function readReceipt(input, target, version) {
  const platform = PLATFORMS[target];
  const base = `vibeface-desktop-${version}-${target}`;
  const payloads = [`${base}.${platform.installer}`, `${base}.zip`];
  const expected = [`${base}.provenance.json`, ...payloads.flatMap(name => [name, `${name}.sha256`])].sort();
  assert.deepEqual(relativeFiles(input).filter(name => name !== 'release.json'), expected,
    'the desktop asset set is incomplete or carries extra files');
  const receipt = JSON.parse(fs.readFileSync(path.join(input, `${base}.provenance.json`), 'utf8'));
  assert.deepEqual(Object.keys(receipt).sort(), ['schema', 'app', 'target', 'version', 'tag', 'channel',
    'source_commit', 'ui', 'gpui_kit', 'gpui', 'signed', 'signing', 'runtime', 'artifacts'].sort(), 'unsupported desktop receipt fields');
  assert.equal(receipt.schema, SCHEMA_VERSION);
  assert.equal(receipt.app, 'desktop');
  assert.equal(receipt.target, target);
  assert.equal(receipt.version, version);
  assert.equal(receipt.tag, `v${version}`);
  for (const [key, value] of Object.entries(NATIVE_UI)) assert.equal(receipt[key], value, 'the desktop application must ship the pinned native GPUI version');
  assert(/^[a-f0-9]{40}$/.test(receipt.source_commit), 'invalid desktop source commit');
  assert(SIGNING_STATES.includes(receipt.signing) && typeof receipt.signed === 'boolean', 'invalid desktop signing status');
  assert.equal(receipt.signed, ['developer-id-notarized', 'authenticode'].includes(receipt.signing), 'desktop signing status and signed flag disagree');
  assert(!receipt.signed || receipt.signing === platform.signing, 'desktop signing kind does not match its platform');
  assert(target.endsWith('-unknown-linux-gnu') ? receipt.signing === 'platform-checksums' : receipt.signing !== 'platform-checksums',
    'desktop signing status does not match its platform');
  assert.equal(receipt.runtime.archive, `vibeface-${version}-${target}.tar.gz`, 'the runtime must be this platform CLI bundle');
  assert(/^[a-f0-9]{64}$/.test(receipt.runtime.sha256), 'invalid runtime digest');
  assert.deepEqual(Object.keys(receipt.artifacts).sort(), [...payloads].sort(), 'the receipt must cover exactly its two payloads');
  for (const name of payloads) {
    assert.equal(hash(path.join(input, name)), receipt.artifacts[name], `desktop payload digest mismatch: ${name}`);
    const checksum = fs.readFileSync(path.join(input, `${name}.sha256`), 'utf8');
    assert.equal(checksum, `${receipt.artifacts[name]}  ${name}\n`, `desktop checksum file mismatch: ${name}`);
  }
  return { receipt, payloads };
}

function verifyNativeBinary(executable, target) {
  const bytes = fs.readFileSync(executable);
  assert(bytes.length >= 64, 'native executable header is truncated');
  if (target.endsWith('-apple-darwin')) {
    assert.equal(bytes.readUInt32LE(0), 0xfeedfacf, 'desktop executable must be 64-bit Mach-O');
    assert.equal(bytes.readUInt32LE(4), 0x0100000c, 'macOS desktop executable must be ARM64');
  } else if (target.includes('-windows-')) {
    assert.equal(bytes.readUInt16LE(0), 0x5a4d, 'desktop executable must be native PE');
    const header = bytes.readUInt32LE(0x3c);
    assert(header + 26 <= bytes.length, 'native PE header is truncated');
    assert.equal(bytes.readUInt32LE(header), 0x00004550, 'desktop executable has no PE signature');
    assert.equal(bytes.readUInt16LE(header + 4), 0x8664, 'Windows desktop executable must be x64');
    assert.equal(bytes.readUInt16LE(header + 24), 0x20b, 'Windows desktop executable must be PE32+');
  } else {
    assert(bytes.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70])), 'desktop executable must be native ELF');
    assert.equal(bytes[4], 2, 'Linux desktop executable must be ELF64');
    assert.equal(bytes[5], 1, 'Linux desktop executable must use little endian');
    assert.equal(bytes.readUInt16LE(18), target.startsWith('aarch64-') ? 183 : 62, 'Linux desktop executable architecture mismatch');
  }
}

/** The packaged application layout and the sealed runtime it carries, after any signing. */
function verifyApplication(root, target, receipt) {
  const platform = PLATFORMS[target];
  const mac = target === 'aarch64-apple-darwin';
  const windows = target === 'x86_64-pc-windows-msvc';
  const application = mac ? path.join(root, 'Vibeface.app') : root;
  const resources = mac ? path.join(application, 'Contents/Resources') : path.join(application, 'resources');
  const executable = mac ? path.join(application, 'Contents/MacOS/Vibeface')
    : path.join(application, windows ? 'Vibeface.exe' : 'vibeface');
  assert(fs.statSync(executable).size > 0, 'the packaged application has no host executable');
  verifyNativeBinary(executable, target);
  assert(!relativeFiles(application).some(name => /(?:app\.asar|chrome-sandbox|Electron Framework|preload\.cjs|libcef)/i.test(name)), 'the native application contains a browser runtime');
  if (mac) {
    const info = fs.readFileSync(path.join(application, 'Contents/Info.plist'), 'utf8');
    assert(info.includes('<string>one.leaper.vibeface.desktop</string>'), 'unexpected macOS bundle identifier');
    assert(info.includes(`<string>${receipt.version}</string>`), 'the macOS bundle does not carry the product version');
    assert(/CFBundleURLSchemes[\s\S]*?<string>vibeface<\/string>/.test(info), 'the macOS bundle does not declare the native deep-link scheme');

  }
  if (target.endsWith('-unknown-linux-gnu')) {
    const entry = fs.readFileSync(path.join(application, 'vibeface.desktop'), 'utf8');
    assert(/^Exec=vibeface %u$/m.test(entry) && /^MimeType=x-scheme-handler\/vibeface;$/m.test(entry), 'Linux desktop entry must forward the native deep-link URL');
    const dependencies = JSON.parse(fs.readFileSync(path.join(application, 'native-dependencies.json'), 'utf8'));
    assert.equal(dependencies.target, target);
    assert(Array.isArray(dependencies.libraries) && dependencies.libraries.length > 0 && dependencies.minimum_glibc, 'Linux package has no actual native dependency report');
  }
  const runtime = path.join(resources, 'runtime');
  const sealed = readSealedRuntime(runtime);
  for (const name of [`vibeface${platform.exe}`, `vibeface-core${platform.exe}`, 'web/index.html',
    'web/vibeface_secure.js', 'web/vibeface_secure_bg.wasm']) {
    assert(sealed.has(name), `the bundled runtime is missing ${name}`);
  }
  const info = JSON.parse(fs.readFileSync(path.join(runtime, 'BUILD-INFO.json'), 'utf8'));
  assert.deepEqual([info.version, info.target, info.source_commit],
    [receipt.version, target, receipt.source_commit], 'the bundled runtime is from another cohort than the receipt');
  return { application, resources, runtime, sealed };
}

/** Real signature assessment; an unsigned preview is reported, never presented as signed. */
function verifySignature(application, target, receipt) {
  if (target === 'x86_64-pc-windows-msvc') {
    assert.equal(process.platform, 'win32', 'Windows signatures must be verified on Windows');
    const executable = path.join(application, 'Vibeface.exe');
    if (!receipt.signed) {
      assert.equal(inspectAuthenticode(executable).status, 'NotSigned', 'an unsigned preview must carry no product signature');
      return 'unsigned development preview';
    }
    requireAuthenticode(executable);
    for (const name of ['vibeface.exe', 'vibeface-core.exe']) requireAuthenticode(path.join(application, 'resources/runtime', name));
    return 'Authenticode and trusted timestamp verified on Windows';
  }
  if (target.endsWith('-unknown-linux-gnu')) {
    assert.equal(process.platform, 'linux', 'Linux desktop packages must be verified on Linux');
    assert.equal(receipt.signed, false, 'Linux must not claim a vendor signature');
    assert.equal(receipt.signing, 'platform-checksums', 'Linux integrity is recorded by checksums and provenance');
    return 'SHA-256 checksums and source/runtime provenance';
  }
  assert.equal(process.platform, 'darwin', 'macOS signatures must be verified on macOS');
  const verified = spawnSync('codesign', ['--verify', '--strict', '--deep', application], { encoding: 'utf8' });
  if (!receipt.signed) {
    const identity = spawnSync('codesign', ['-dvvv', application], { encoding: 'utf8' });
    assert(!(identity.stdout + identity.stderr).includes('Authority=Developer ID Application'), 'a preview must not claim a Developer ID signature');
    return 'unsigned development preview';
  }
  assert.equal(verified.status, 0, `codesign rejected the application: ${verified.stderr}`);
  const described = spawnSync('codesign', ['-dvvv', application], { encoding: 'utf8' });
  assert.equal(described.status, 0, 'cannot read the application signature');
  assert(/flags=0x[0-9a-f]+\([^)]*runtime/.test(described.stdout + described.stderr),
    'the signed application must use the hardened runtime');
  const assessment = spawnSync('spctl', ['-a', '-vvv', '-t', 'exec', application], { encoding: 'utf8' });
  assert.equal(assessment.status, 0, `Gatekeeper rejected the application: ${assessment.stderr}`);
  assert(assessment.stderr.includes('source=Notarized Developer ID'), 'the published application must be notarized');
  return 'Developer ID, hardened runtime, notarized';
}

function extract(archive, destination) {
  fs.mkdirSync(destination, { recursive: true });
  if (process.platform === 'linux') run('unzip', ['-q', archive, '-d', destination]);
  else {
    let tar = 'tar';
    if (process.platform === 'win32') {
      const systemRoot = process.env.SystemRoot;
      assert(systemRoot && path.isAbsolute(systemRoot), 'Windows SystemRoot is required to locate native tar');
      tar = path.join(systemRoot, 'System32', 'tar.exe');
      assert(fs.statSync(tar).isFile(), 'Windows native tar is installed');
    }
    run(tar, ['-xf', archive, '-C', destination]);
  }
  return destination;
}

function verify({ target, input, distribution, scratch }) {
  assert(PLATFORMS[target], `--target must be one of ${Object.keys(PLATFORMS).join(', ')}`);
  const version = /^version = "([^"]+)"$/m.exec(fs.readFileSync(path.join(repo, 'apps/cli/Cargo.toml'), 'utf8'))?.[1];
  assert(version, 'the canonical product version comes from the CLI package');
  const { receipt } = readReceipt(input, target, version);
  const checksumProtectedLinux = target.endsWith('-unknown-linux-gnu') && receipt.signing === 'platform-checksums';
  assert(!distribution || receipt.signed || checksumProtectedLinux, 'a stable distribution requires a signed application or Linux checksums and provenance');
  assert(receipt.signed || checksumProtectedLinux || receipt.channel !== 'stable', 'a stable channel desktop build must be signed');
  const extracted = extract(path.join(input, `vibeface-desktop-${version}-${target}.zip`), path.join(scratch, 'app'));
  const packaged = verifyApplication(extracted, target, receipt);
  const { application } = packaged;
  const signature = verifySignature(application, target, receipt);
  if (target.endsWith('-unknown-linux-gnu')) {
    const imageRoot = path.join(scratch, 'image');
    fs.mkdirSync(imageRoot);
    const image = path.join(input, `vibeface-desktop-${version}-${target}.AppImage`);
    fs.chmodSync(image, 0o755);
    run(image, ['--appimage-extract'], { cwd: imageRoot });
    const installed = verifyApplication(path.join(imageRoot, 'squashfs-root'), target, receipt);
    assert.deepEqual([...installed.sealed], [...packaged.sealed], 'AppImage runtime differs from portable ZIP');
    assert.equal(hash(path.join(installed.application, 'vibeface')), hash(path.join(application, 'vibeface')), 'AppImage native executable differs from portable ZIP');
  } else if (target.endsWith('-apple-darwin')) {
    const mount = path.join(scratch, 'dmg');
    fs.mkdirSync(mount);
    run('hdiutil', ['attach', '-readonly', '-nobrowse', '-mountpoint', mount, path.join(input, `vibeface-desktop-${version}-${target}.dmg`)]);
    try {
      const installed = verifyApplication(mount, target, receipt);
      verifySignature(installed.application, target, receipt);
      assert.deepEqual([...installed.sealed], [...packaged.sealed], 'DMG runtime differs from portable ZIP');
      assert.equal(hash(path.join(installed.application, 'Contents/MacOS/Vibeface')), hash(path.join(application, 'Contents/MacOS/Vibeface')), 'DMG native executable differs from portable ZIP');
    } finally { run('hdiutil', ['detach', mount]); }
  }
  if (target === 'x86_64-pc-windows-msvc' && receipt.signed) {
    requireAuthenticode(path.join(input, `vibeface-desktop-${version}-${target}.exe`));
  }
  console.log(`Verified desktop ${target} ${version} from ${receipt.source_commit}; signature: ${signature}`);
  return receipt;
}

function selfTest() {
  const artifacts = path.join(repo, '.artifacts');
  fs.mkdirSync(artifacts, { recursive: true });
  const scratch = fs.mkdtempSync(path.join(artifacts, 'desktop-verify-'));
  try {
    const version = '9.9.9-nightly.20261107.1';
    const commit = 'a'.repeat(40);
    const write = (file, content) => {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      fs.writeFileSync(file, content);
    };
    const buildFixture = (target, changes = {}) => {
      const platform = PLATFORMS[target];
      const mac = target === 'aarch64-apple-darwin';
      const windows = target === 'x86_64-pc-windows-msvc';
      const root = fs.mkdtempSync(path.join(scratch, 'fixture-'));
      const application = mac ? path.join(root, 'Vibeface.app') : root;
      const resources = mac ? path.join(application, 'Contents/Resources') : path.join(application, 'resources');
      const header = Buffer.alloc(512);
      if (mac) { header.writeUInt32LE(0xfeedfacf); header.writeUInt32LE(0x0100000c, 4); }
      else if (windows) {
        header.writeUInt16LE(0x5a4d); header.writeUInt32LE(0x80, 0x3c);
        header.writeUInt32LE(0x00004550, 0x80); header.writeUInt16LE(0x8664, 0x84); header.writeUInt16LE(0x20b, 0x98);
      } else {
        Buffer.from([127, 69, 76, 70, 2, 1]).copy(header);
        header.writeUInt16LE(target.startsWith('aarch64-') ? 183 : 62, 18);
      }
      write(mac ? path.join(application, 'Contents/MacOS/Vibeface')
        : path.join(application, windows ? 'Vibeface.exe' : 'vibeface'), header);

      if (mac) {
        write(path.join(application, 'Contents/Info.plist'),
          `<plist><dict><key>CFBundleIdentifier</key><string>one.leaper.vibeface.desktop</string>`
          + `<key>CFBundleShortVersionString</key><string>${version}</string><key>CFBundleURLSchemes</key><array><string>vibeface</string></array></dict></plist>`);

      }
      if (target.endsWith('-unknown-linux-gnu')) {
        write(path.join(application, 'vibeface.desktop'), 'Exec=vibeface %u\nMimeType=x-scheme-handler/vibeface;\n');
        write(path.join(application, 'native-dependencies.json'), JSON.stringify({ target, libraries: ['libc.so.6'], minimum_glibc: '2.17' }));
      }
      const runtime = path.join(resources, 'runtime');
      const entries = {
        [`vibeface${platform.exe}`]: 'cli\n',
        [`vibeface-core${platform.exe}`]: 'core\n',
        'web/index.html': '<!doctype html>fixture',
        'web/vibeface_secure.js': 'export const fixture = true;\n',
        'web/vibeface_secure_bg.wasm': 'wasm',
      };
      for (const [name, content] of Object.entries(entries)) write(path.join(runtime, name), content);
      write(path.join(runtime, 'BUILD-INFO.json'),
        `${JSON.stringify({ version, target, source_commit: commit }, null, 2)}\n`);
      fs.writeFileSync(path.join(runtime, 'FILES.sha256'), relativeFiles(runtime)
        .filter(name => name !== 'FILES.sha256').map(name => `${hash(path.join(runtime, name))}  ${name}\n`).join(''));
      const receipt = {
        schema: SCHEMA_VERSION, app: 'desktop', target, version, tag: `v${version}`, channel: 'nightly',
        source_commit: commit, ...NATIVE_UI, signed: false,
        signing: target.endsWith('-unknown-linux-gnu') ? 'platform-checksums' : 'unsigned-development-preview',
        runtime: { archive: `vibeface-${version}-${target}.tar.gz`, sha256: 'b'.repeat(64) },
        artifacts: {}, ...changes,
      };
      return { root, receipt, runtime };
    };

    for (const target of Object.keys(PLATFORMS)) {
      const { root, receipt, runtime } = buildFixture(target);
      const verified = verifyApplication(root, target, receipt);
      assert(verified.sealed.size >= 6, `${target} fixture seals its runtime`);
      const host = target.endsWith('-apple-darwin') ? path.join(verified.application, 'Contents/MacOS/Vibeface') : path.join(root, target.includes('-windows-') ? 'Vibeface.exe' : 'vibeface');
      const native = fs.readFileSync(host);
      fs.writeFileSync(host, '#!/bin/sh\nexit 0\n');
      assert.throws(() => verifyApplication(root, target, receipt), /native executable header/);
      fs.writeFileSync(host, native);
      fs.appendFileSync(path.join(runtime, 'web/index.html'), 'resigned');
      assert.throws(() => verifyApplication(root, target, receipt), /changed the sealed runtime file web\/index.html/);
    }

    const { root, receipt, runtime } = buildFixture('aarch64-apple-darwin');
    fs.writeFileSync(path.join(runtime, 'extra.txt'), 'x');
    assert.throws(() => verifyApplication(root, 'aarch64-apple-darwin', receipt), /changed the set of runtime files/);
    fs.rmSync(path.join(runtime, 'extra.txt'));
    fs.writeFileSync(path.join(runtime, 'BUILD-INFO.json'),
      `${JSON.stringify({ version, target: 'aarch64-apple-darwin', source_commit: 'c'.repeat(40) }, null, 2)}\n`);
    fs.writeFileSync(path.join(runtime, 'FILES.sha256'), relativeFiles(runtime)
      .filter(name => name !== 'FILES.sha256').map(name => `${hash(path.join(runtime, name))}  ${name}\n`).join(''));
    assert.throws(() => verifyApplication(root, 'aarch64-apple-darwin', receipt), /another cohort than the receipt/);

    const missing = buildFixture('aarch64-apple-darwin');
    fs.rmSync(path.join(missing.runtime, 'web/vibeface_secure.js'));
    fs.writeFileSync(path.join(missing.runtime, 'FILES.sha256'), relativeFiles(missing.runtime)
      .filter(name => name !== 'FILES.sha256').map(name => `${hash(path.join(missing.runtime, name))}  ${name}\n`).join(''));
    assert.throws(() => verifyApplication(missing.root, 'aarch64-apple-darwin', missing.receipt),
      /bundled runtime is missing web\/vibeface_secure.js/);

    // Asset sets: exact names, honest checksums and a signing status nobody can overstate.
    const assetSet = (target, changes = {}, mutate = () => {}) => {
      const input = fs.mkdtempSync(path.join(scratch, 'assets-'));
      const platform = PLATFORMS[target];
      const base = `vibeface-desktop-${version}-${target}`;
      const artifactDigests = {};
      for (const name of [`${base}.${platform.installer}`, `${base}.zip`]) {
        fs.writeFileSync(path.join(input, name), `${name} fixture payload`);
        artifactDigests[name] = hash(path.join(input, name));
        fs.writeFileSync(path.join(input, `${name}.sha256`), `${artifactDigests[name]}  ${name}\n`);
      }
      const { receipt: fixtureReceipt } = buildFixture(target, changes);
      fs.writeFileSync(path.join(input, `${base}.provenance.json`),
        `${JSON.stringify({ ...fixtureReceipt, artifacts: artifactDigests, ...changes }, null, 2)}\n`);
      mutate(input, base);
      return input;
    };
    for (const target of Object.keys(PLATFORMS)) readReceipt(assetSet(target), target, version);
    assert.throws(() => readReceipt(assetSet('aarch64-apple-darwin', {}, input =>
      fs.writeFileSync(path.join(input, 'extra.txt'), 'x')), 'aarch64-apple-darwin', version), /incomplete or carries extra files/);
    assert.throws(() => readReceipt(assetSet('aarch64-apple-darwin', {}, (input, base) =>
      fs.rmSync(path.join(input, `${base}.zip.sha256`))), 'aarch64-apple-darwin', version), /incomplete or carries extra files/);
    assert.throws(() => readReceipt(assetSet('aarch64-apple-darwin', {}, (input, base) =>
      fs.writeFileSync(path.join(input, `${base}.dmg`), 'changed')), 'aarch64-apple-darwin', version), /payload digest mismatch/);
    for (const [label, changes, expected] of [
      ['a faked signature claim', { signed: true }, /signing status and signed flag disagree/],
      ['Authenticode claimed on macOS', { signed: true, signing: 'authenticode' }, /signing kind does not match/],
      ['another native GPUI version', { gpui: '0.2.2' }, /pinned native GPUI version/],
      ['an unknown signing state', { signed: true, signing: 'self-signed' }, /invalid desktop signing status/],
      ['a foreign runtime archive', { runtime: { archive: 'vibeface-1.2.3-aarch64-apple-darwin.tar.gz', sha256: 'b'.repeat(64) } },
        /this platform CLI bundle/],
      ['an extra receipt field', { legacy: true }, /unsupported desktop receipt fields/],
    ]) {
      assert.throws(() => readReceipt(assetSet('aarch64-apple-darwin', changes), 'aarch64-apple-darwin', version), expected, label);
    }
    console.log('desktop verification self-test passed: exact asset sets, payload checksums, packaged layout, sealed runtime immutability and honest signing status.');
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
    let distribution = false;
    while (args.length) {
      const flag = args.shift();
      if (flag === '--distribution') { distribution = true; continue; }
      assert(['--target', '--input'].includes(flag) && args.length && !options.has(flag), `invalid argument: ${flag}`);
      options.set(flag, args.shift());
    }
    for (const flag of ['--target', '--input']) assert(options.has(flag), `${flag} is required`);
    const input = path.resolve(options.get('--input'));
    const scratch = fs.mkdtempSync(path.join(path.dirname(input), '.desktop-verify-'));
    try {
      verify({ target: options.get('--target'), input, distribution, scratch });
    } finally {
      fs.rmSync(scratch, { recursive: true, force: true });
    }
  }
} catch (error) {
  console.error(`vibeface desktop verification: ${error.message}`);
  process.exitCode = 1;
}
