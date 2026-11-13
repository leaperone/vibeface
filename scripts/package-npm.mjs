#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync, gzipSync } from 'node:zlib';

const repo = fileURLToPath(new URL('../', import.meta.url));
const targets = ['aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu',
  'x86_64-pc-windows-msvc'];
const windowsTargets = new Set(['x86_64-pc-windows-msvc']);
const programSuffix = target => (windowsTargets.has(target) ? '.exe' : '');
const programs = target => [`vibeface${programSuffix(target)}`, `vibeface-core${programSuffix(target)}`];
const requiredFiles = target => ['FILES.sha256', ...programs(target), 'BUILD-INFO.json', 'README.md',
  'RUNTIME-REQUIREMENTS.md',
  `scripts/install-node-service.${windowsTargets.has(target) ? 'ps1' : 'sh'}`, 'docs/node.md', 'docs/releasing.md',
  'web/_shell.html', 'web/index.html', 'web/_redirects', 'web/_headers', 'web/.vibeface-build.json',
  'web/vibeface_secure.js', 'web/vibeface_secure_bg.wasm'];
const run = (program, args, options = {}) => execFileSync(program, args, {
  encoding: 'utf8', maxBuffer: 8 * 1024 * 1024, ...options,
});
const hash = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const control = fs.readFileSync(path.join(repo, 'crates/control/src/lib.rs'), 'utf8');
const updateProtocol = Number(/pub const UPDATE_PROTOCOL: u32 = (\d+);/.exec(control)?.[1]);
const dataEpoch = Number(/pub const DATA_EPOCH: u32 = (\d+);/.exec(control)?.[1]);
assert(Number.isSafeInteger(updateProtocol) && Number.isSafeInteger(dataEpoch), 'missing release protocol constants');

// The npm package carries all four platform bundles, so it grows four times as fast as one of
// them. These bounds only stop a corrupt archive; collect fails on them and blocks the whole
// product release, so they stay far above the real package (175 MB packed and 492 MB unpacked in
// November 2026).
const MAX_PACKED = 1024 * 1024 * 1024;
const MAX_UNPACKED = 2 * 1024 * 1024 * 1024;

function verifyTarMetadata(compressed) {
  const bytes = gunzipSync(compressed, { maxOutputLength: MAX_UNPACKED });
  const text = (header, start, length) => header.subarray(start, start + length).toString('utf8').replace(/\0.*$/s, '').trim();
  for (let offset = 0; offset + 512 <= bytes.length;) {
    const header = bytes.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    for (const start of [108, 116]) assert.equal(parseInt(text(header, start, 8) || '0', 8), 0, 'archive must not contain a builder uid or gid');
    assert.equal(text(header, 265, 32), '', 'archive must not contain a builder user name');
    assert.equal(text(header, 297, 32), '', 'archive must not contain a builder group name');
    const size = parseInt(text(header, 124, 12) || '0', 8);
    assert(Number.isSafeInteger(size) && size >= 0 && offset + 512 + size <= bytes.length, 'invalid archive size');
    if (header[156] === 120 || header[156] === 103) {
      let pax = bytes.subarray(offset + 512, offset + 512 + size).toString('utf8');
      while (pax) {
        const length = parseInt(pax, 10), separator = pax.indexOf(' ');
        assert(Number.isSafeInteger(length) && length > separator + 1 && length <= Buffer.byteLength(pax), 'invalid PAX metadata');
        const record = Buffer.from(pax).subarray(separator + 1, length).toString('utf8');
        assert(['mtime', 'atime', 'ctime'].includes(record.split('=')[0]), 'archive contains private PAX metadata');
        pax = Buffer.from(pax).subarray(length).toString('utf8');
      }
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
}

function nightly(version) {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-nightly\.(\d{8})\.([1-9]\d*)$/.exec(version);
  assert(match, 'npm packaging only accepts canonical Nightly versions');
  const day = `${match[4].slice(0, 4)}-${match[4].slice(4, 6)}-${match[4].slice(6, 8)}`;
  assert(Number(match[4].slice(0, 4)) > 0 && new Date(`${day}T00:00:00Z`).toISOString().startsWith(day), 'invalid Nightly date');
}

function unpack(archive, destination, root) {
  assert(fs.statSync(archive).size <= MAX_PACKED, 'compressed package is too large');
  gunzipSync(fs.readFileSync(archive), { maxOutputLength: MAX_UNPACKED });
  const entries = run('tar', ['-tzf', archive]).trimEnd().split('\n').map(entry => entry.replace(/\/$/, ''));
  assert(entries.length > 0 && entries.length <= 4096, 'invalid archive entry count');
  assert.equal(new Set(entries).size, entries.length, 'duplicate archive paths');
  for (const entry of entries) {
    assert(/^[a-zA-Z0-9._/-]+$/.test(entry) && (entry === root || entry.startsWith(`${root}/`)), 'unsafe archive path');
    assert(entry.split('/').every(part => part && part !== '.' && part !== '..'), 'unsafe archive component');
  }
  assert(run('tar', ['-tvzf', archive]).trimEnd().split('\n').every(entry => /^[-d]/.test(entry)), 'archive links or special files are not allowed');
  fs.mkdirSync(destination, { recursive: true });
  run('tar', ['-xzf', archive, '--no-same-owner', '--no-same-permissions', '-C', destination], {
    env: { ...process.env, COPYFILE_DISABLE: '1' },
  });
  return path.join(destination, root);
}

function nativePackage(directory, version, target, sourceCommit) {
  for (const entry of requiredFiles(target)) {
    const file = fs.lstatSync(path.join(directory, entry), { throwIfNoEntry: false });
    assert(file?.isFile() && file.size > 0, `missing package file: ${entry}`);
  }
  const info = JSON.parse(fs.readFileSync(path.join(directory, 'BUILD-INFO.json'), 'utf8'));
  assert.deepEqual(Object.keys(info).sort(), ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
    'core_version', 'core_source_commit', 'app_version', 'app_source_commit'].sort(), 'unsupported build info fields');
  assert.deepEqual([info.version, info.target, info.source_commit, info.update_protocol, info.data_epoch],
    [version, target, sourceCommit, updateProtocol, dataEpoch], 'native package identity mismatch');
  for (const field of ['core_version', 'app_version']) {
    nightly(info[field]);
    assert.equal(info[field], version, 'component versions must equal the bundled product version');
  }
  for (const field of ['core_source_commit', 'app_source_commit']) {
    assert(/^[a-f0-9]{40}$/.test(info[field]), 'invalid component source');
    assert.equal(info[field], sourceCommit, 'component source commits must equal the bundled product source');
  }
  const checked = new Set();
  for (const line of fs.readFileSync(path.join(directory, 'FILES.sha256'), 'utf8').trim().split('\n')) {
    const match = /^([a-f0-9]{64})  ([a-zA-Z0-9._/-]+)$/.exec(line);
    assert(match && match[2].split('/').every(part => part && part !== '.' && part !== '..'), 'invalid file checksum entry');
    assert(!checked.has(match[2]) && match[2] !== 'FILES.sha256', 'duplicate or recursive file checksum');
    checked.add(match[2]);
    assert.equal(hash(path.join(directory, match[2])), match[1], 'component content checksum mismatch');
  }
  const actual = fs.readdirSync(directory, { recursive: true }).filter(name => name !== 'FILES.sha256' && fs.lstatSync(path.join(directory, name)).isFile()).sort();
  assert.deepEqual([...checked].sort(), actual, 'file checksum coverage mismatch');
  for (const binary of programs(target)) {
    const executable = path.join(directory, binary);
    assert(fs.statSync(executable).mode & 0o111, `${binary} must be executable`);
  }
  assert(fs.readFileSync(path.join(directory, 'web/vibeface_secure_bg.wasm')).subarray(0, 4).equals(Buffer.from([0, 97, 115, 109])), 'invalid WebAssembly module');
}

function verifyReceipt(archive, version, sourceCommit) {
  const receipt = JSON.parse(fs.readFileSync(`${archive}.json`, 'utf8'));
  assert.equal(receipt.sha256, hash(archive), 'npm artifact SHA256 mismatch');
  assert.equal(receipt.integrity, `sha512-${createHash('sha512').update(fs.readFileSync(archive)).digest('base64')}`, 'npm artifact integrity mismatch');
  assert.deepEqual([receipt.version, receipt.source_commit, receipt.filename, receipt.size],
    [version, sourceCommit, path.basename(archive), fs.statSync(archive).size], 'npm artifact receipt identity mismatch');
}

function verify(archive, version, sourceCommit, scratch) {
  verifyTarMetadata(fs.readFileSync(archive));
  const directory = unpack(archive, scratch, 'package');
  const metadata = JSON.parse(fs.readFileSync(path.join(directory, 'package.json'), 'utf8'));
  assert.equal(metadata.name, '@leaperone/vibeface');
  assert.equal(metadata.version, version);
  assert.equal(metadata.private, false);
  assert.deepEqual(metadata.bin, { vibeface: 'bin/vibeface.mjs' });
  assert(!metadata.scripts && !metadata.dependencies && !metadata.optionalDependencies, 'npm package must not run lifecycle scripts or download dependencies');
  assert.equal(fs.readFileSync(path.join(directory, 'bin/vibeface.mjs'), 'utf8'),
    fs.readFileSync(path.join(repo, 'apps/cli/npm/bin/vibeface.mjs'), 'utf8'), 'launcher does not match the source');
  assert.deepEqual(fs.readdirSync(path.join(directory, 'bin')), ['vibeface.mjs'], 'unexpected npm launcher file');
  assert.deepEqual(fs.readdirSync(path.join(directory, 'vendor')).sort(), [...targets].sort());
  for (const target of targets) nativePackage(path.join(directory, 'vendor', target), version, target, sourceCommit);
  return directory;
}

function pack(input, output, version, sourceCommit) {
  nightly(version);
  assert(/^[0-9a-f]{40}$/.test(sourceCommit), 'a full source commit is required');
  const names = targets.map(target => `vibeface-${version}-${target}.tar.gz`);
  assert.deepEqual(fs.readdirSync(input).sort(), names.flatMap(name => [name, `${name}.sha256`]).sort(),
    `expected exactly ${targets.length} native archives and ${targets.length} checksums`);
  fs.mkdirSync(output, { recursive: true });
  assert(!fs.existsSync(path.join(output, `leaperone-vibeface-${version}.tgz`)), 'npm artifact already exists; use a fresh output directory');
  const scratch = fs.mkdtempSync(path.join(output, '.npm-stage-'));
  try {
    const directory = path.join(scratch, 'npm');
    fs.mkdirSync(path.join(directory, 'vendor'), { recursive: true });
    fs.mkdirSync(path.join(directory, 'bin'));
    fs.copyFileSync(path.join(repo, 'apps/cli/npm/bin/vibeface.mjs'), path.join(directory, 'bin/vibeface.mjs'));
    fs.copyFileSync(path.join(repo, 'apps/cli/npm/README.md'), path.join(directory, 'README.md'));
    const metadata = JSON.parse(fs.readFileSync(path.join(repo, 'apps/cli/npm/package.json'), 'utf8'));
    fs.writeFileSync(path.join(directory, 'package.json'), `${JSON.stringify({ ...metadata, version, private: false }, null, 2)}\n`);
    for (const [index, target] of targets.entries()) {
      const archive = path.join(input, names[index]);
      const checksum = fs.readFileSync(`${archive}.sha256`, 'utf8').trim().split(/\s+/);
      assert(checksum.length === 2 && checksum[1] === names[index] && /^[a-fA-F0-9]{64}$/.test(checksum[0]), 'checksum must name the exact native archive');
      assert.equal(hash(archive), checksum[0].toLowerCase(), 'native archive SHA256 mismatch');
      const native = unpack(archive, path.join(scratch, target), names[index].slice(0, -7));
      nativePackage(native, version, target, sourceCommit);
      fs.renameSync(native, path.join(directory, 'vendor', target));
    }
    const [result] = JSON.parse(run('npm', ['pack', '--json', '--ignore-scripts', '--pack-destination', scratch], { cwd: directory }));
    const staged = path.join(scratch, result.filename);
    verify(staged, version, sourceCommit, path.join(scratch, 'verify'));
    const artifact = path.join(output, result.filename);
    fs.renameSync(staged, artifact);
    fs.writeFileSync(`${artifact}.json`, `${JSON.stringify({ version, source_commit: sourceCommit,
      filename: result.filename, size: result.size, integrity: result.integrity, sha256: hash(artifact) }, null, 2)}\n`);
    verifyReceipt(artifact, version, sourceCommit);
    return artifact;
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

async function selfTest() {
  const artifacts = path.join(repo, '.artifacts');
  fs.mkdirSync(artifacts, { recursive: true });
  const scratch = fs.mkdtempSync(path.join(artifacts, 'npm-self-test-'));
  const version = '1.2.3-nightly.20261019.1';
  const commit = 'a'.repeat(40);
  try {
    const input = path.join(scratch, 'input');
    fs.mkdirSync(input);
    const sources = new Map();
    const repack = target => {
      const name = `vibeface-${version}-${target}`;
      const archive = path.join(input, `${name}.tar.gz`);
      const directory = path.join(scratch, 'source', name);
      const entries = fs.readdirSync(directory, { recursive: true })
        .filter(entry => entry !== 'FILES.sha256' && fs.lstatSync(path.join(directory, entry)).isFile()).sort();
      fs.writeFileSync(path.join(directory, 'FILES.sha256'), entries.map(entry => `${hash(path.join(directory, entry))}  ${entry}\n`).join(''));
      run('tar', ['-czf', archive, '-C', path.join(scratch, 'source'), name], { env: { ...process.env, COPYFILE_DISABLE: '1' } });
      fs.writeFileSync(`${archive}.sha256`, `${hash(archive)}  ${name}.tar.gz\n`);
    };
    for (const target of targets) {
      const directory = path.join(scratch, 'source', `vibeface-${version}-${target}`);
      sources.set(target, directory);
      for (const entry of requiredFiles(target)) {
        fs.mkdirSync(path.dirname(path.join(directory, entry)), { recursive: true });
        fs.writeFileSync(path.join(directory, entry), 'fixture\n');
      }
      fs.writeFileSync(path.join(directory, 'BUILD-INFO.json'), JSON.stringify({ version, target, source_commit: commit, update_protocol: 1, core_version: version, core_source_commit: commit, app_version: version, app_source_commit: commit, data_epoch: 1 }));
      fs.writeFileSync(path.join(directory, 'web/vibeface_secure_bg.wasm'), Buffer.from([0, 97, 115, 109]));
      const [cli, core] = programs(target);
      fs.writeFileSync(path.join(directory, cli), '#!/usr/bin/env node\nif (process.env.VIBEFACE_PACK_ASSERT_NO_EXEC) process.exit(99);\nif (process.argv[2] === "--version") console.log(`vibeface ${process.env.VIBEFACE_FIXTURE_VERSION}`);\nelse if (process.argv[2] === "exit") process.exit(23);\nelse if (process.argv[2] === "wait") { process.on("SIGTERM", () => process.exit(42)); console.log("ready"); setInterval(() => {}, 1000); }\nelse console.log(JSON.stringify(process.argv.slice(2)));\n');
      fs.writeFileSync(path.join(directory, core), '#!/usr/bin/env node\nif (process.env.VIBEFACE_PACK_ASSERT_NO_EXEC) process.exit(99);\nif (process.argv[2] === "--version") console.log(`vibeface-core ${process.env.VIBEFACE_FIXTURE_VERSION}`);\nelse console.log(JSON.stringify(process.argv.slice(2)));\n');
      for (const binary of programs(target)) fs.chmodSync(path.join(directory, binary), 0o755);
      repack(target);
    }
    process.env.VIBEFACE_FIXTURE_VERSION = version;
    process.env.VIBEFACE_PACK_ASSERT_NO_EXEC = '1';
    const artifact = pack(input, path.join(scratch, 'output'), version, commit);
    delete process.env.VIBEFACE_PACK_ASSERT_NO_EXEC;
    assert.equal(path.basename(artifact), `leaperone-vibeface-${version}.tgz`);
    const artifactBytes = fs.readFileSync(artifact);
    const privateHeader = gunzipSync(artifactBytes);
    privateHeader.write('0000001\0', 108, 'ascii');
    assert.throws(() => verifyTarMetadata(gzipSync(privateHeader)), /builder uid/);
    fs.appendFileSync(artifact, 'changed');
    assert.throws(() => verifyReceipt(artifact, version, commit), /SHA256 mismatch/);
    fs.writeFileSync(artifact, artifactBytes);
    const prefix = path.join(scratch, 'prefix');
    run('npm', ['install', '--global', '--prefix', prefix, '--ignore-scripts', '--no-audit', '--no-fund', artifact]);
    assert.equal(JSON.parse(fs.readFileSync(path.join(prefix, 'lib/node_modules/@leaperone/vibeface/package.json'), 'utf8')).name, '@leaperone/vibeface');
    const binary = path.join(prefix, 'bin/vibeface');
    assert.equal(run(binary, ['one two', '--flag']), '["one two","--flag"]\n');
    const core = path.join(prefix, 'lib/node_modules/@leaperone/vibeface/vendor', targets[0], 'vibeface-core');
    assert.equal(run(core, ['--version']), `vibeface-core ${version}\n`);
    assert.equal(spawnSync(binary, ['exit']).status, 23);
    const waiting = spawn(binary, ['wait'], { stdio: ['ignore', 'pipe', 'inherit'] });
    try {
      await once(waiting.stdout, 'data', { signal: AbortSignal.timeout(5000) });
      waiting.kill('SIGTERM');
      assert.deepEqual(await once(waiting, 'exit', { signal: AbortSignal.timeout(5000) }), [42, null]);
    } finally { waiting.kill('SIGKILL'); }
    const target = targets[0];
    const archive = path.join(input, `vibeface-${version}-${target}.tar.gz`);
    fs.writeFileSync(`${archive}.sha256`, `${'0'.repeat(64)}  ${path.basename(archive)}\n`);
    assert.throws(() => pack(input, path.join(scratch, 'bad-checksum'), version, commit), /SHA256 mismatch/);
    repack(target);
    assert.throws(() => pack(input, path.join(scratch, 'wrong-source'), version, 'b'.repeat(40)), /identity mismatch/);
    const manifest = path.join(sources.get(target), 'BUILD-INFO.json');
    const original = fs.readFileSync(manifest);
    const build = JSON.parse(original);
    for (const [field, value] of [['update_protocol', 0], ['update_protocol', 2], ['data_epoch', 0], ['data_epoch', 2], ['data_epoch', 12]]) {
      fs.writeFileSync(manifest, JSON.stringify({ ...build, [field]: value }));
      repack(target);
      assert.throws(() => pack(input, path.join(scratch, `${field}-${value}`), version, commit), /identity mismatch/);
    }
    fs.writeFileSync(manifest, JSON.stringify({ ...build, legacy: true }));
    repack(target);
    assert.throws(() => pack(input, path.join(scratch, 'unknown-field'), version, commit), /unsupported build info fields/);
    for (const field of ['core_version', 'app_version']) {
      fs.writeFileSync(manifest, JSON.stringify({ ...build, [field]: '0.0.1' }));
      repack(target);
      assert.throws(() => pack(input, path.join(scratch, `mixed-${field}`), version, commit), /Nightly versions/);
      // A same-channel Nightly from another cohort is still a foreign component.
      fs.writeFileSync(manifest, JSON.stringify({ ...build, [field]: '1.2.3-nightly.20261019.2' }));
      repack(target);
      assert.throws(() => pack(input, path.join(scratch, `cohort-${field}`), version, commit), /must equal the bundled product version/);
    }
    for (const field of ['core_source_commit', 'app_source_commit']) {
      fs.writeFileSync(manifest, JSON.stringify({ ...build, [field]: 'c'.repeat(40) }));
      repack(target);
      assert.throws(() => pack(input, path.join(scratch, `foreign-${field}`), version, commit), /must equal the bundled product source/);
    }
    for (const [platform, directory] of sources) {
      const info = JSON.parse(fs.readFileSync(path.join(directory, 'BUILD-INFO.json')));
      fs.writeFileSync(path.join(directory, 'BUILD-INFO.json'), JSON.stringify({ ...info, data_epoch: 2 }));
      repack(platform);
    }
    assert.throws(() => pack(input, path.join(scratch, 'all-old-epoch'), version, commit), /identity mismatch/);
    for (const [platform, directory] of sources) {
      const info = JSON.parse(fs.readFileSync(path.join(directory, 'BUILD-INFO.json')));
      fs.writeFileSync(path.join(directory, 'BUILD-INFO.json'), JSON.stringify({ ...info, data_epoch: 1 }));
      repack(platform);
    }
    fs.writeFileSync(manifest, original);
    repack(target);
    const other = targets[2];
    const otherManifest = path.join(sources.get(other), 'BUILD-INFO.json');
    const otherOriginal = fs.readFileSync(otherManifest);
    fs.writeFileSync(otherManifest, JSON.stringify({ ...JSON.parse(otherOriginal), app_source_commit: 'd'.repeat(40) }));
    repack(other);
    assert.throws(() => pack(input, path.join(scratch, 'foreign-last-target'), version, commit), /must equal the bundled product source/);
    fs.writeFileSync(otherManifest, otherOriginal);
    repack(other);
    assert.equal(path.basename(pack(input, path.join(scratch, 'cohort-restored'), version, commit)), `leaperone-vibeface-${version}.tgz`);
    // The Windows vendor carries .exe basenames and the PowerShell service installer, nothing else.
    const windows = 'x86_64-pc-windows-msvc';
    const windowsSource = sources.get(windows);
    for (const [from, to, expected] of [
      ['vibeface.exe', 'vibeface', /missing package file: vibeface.exe/],
      ['scripts/install-node-service.ps1', 'scripts/install-node-service.sh', /missing package file: scripts\/install-node-service.ps1/],
    ]) {
      fs.renameSync(path.join(windowsSource, from), path.join(windowsSource, to));
      repack(windows);
      assert.throws(() => pack(input, path.join(scratch, `windows-${path.basename(to)}`), version, commit), expected);
      fs.renameSync(path.join(windowsSource, to), path.join(windowsSource, from));
      repack(windows);
    }
    fs.symlinkSync('/tmp/vibeface-outside-package', path.join(sources.get(target), 'link'));
    repack(target);
    assert.throws(() => pack(input, path.join(scratch, 'unsafe-archive'), version, commit), /links or special files/);
    assert.throws(() => nightly('1.2.3'), /Nightly versions/);
    assert.throws(() => nightly('1.2.3-nightly.20260229.1'), /invalid Nightly date/);
    console.log(`npm self-test passed: complete epoch-1 packages, one cohort version and source across all ${targets.length} targets including Windows .exe vendors, old-epoch rejection, global install without scripts, arguments, exit status, signals, checksums, source identity, protocol and unsafe archive rejection.`);
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

try {
  const args = process.argv.slice(2);
  if (args.length === 1 && args[0] === '--self-test') {
    await selfTest();
  } else {
    const options = new Map();
    while (args.length) {
      const flag = args.shift();
      assert(['--input', '--output', '--source-commit', '--verify'].includes(flag) && args.length && !options.has(flag), `invalid argument: ${flag}`);
      options.set(flag, args.shift());
    }
    const version = /^version = "([^"]+)"$/m.exec(fs.readFileSync(path.join(repo, 'apps/cli/Cargo.toml'), 'utf8'))?.[1];
    nightly(version);
    const commit = options.get('--source-commit') ?? run('git', ['rev-parse', 'HEAD'], { cwd: repo }).trim();
    assert(/^[0-9a-f]{40}$/.test(commit), 'a full source commit is required');
    const output = path.resolve(options.get('--output') ?? path.join(repo, '.artifacts/npm'));
    if (options.has('--verify')) {
      fs.mkdirSync(output, { recursive: true });
      const scratch = fs.mkdtempSync(path.join(output, '.verify-'));
      try {
        const archive = path.resolve(options.get('--verify'));
        verifyReceipt(archive, version, commit);
        verify(archive, version, commit, scratch);
        console.log(`Verified npm package ${version} from ${commit}`);
      } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
    } else {
      console.log(pack(path.resolve(options.get('--input') ?? path.join(repo, '.artifacts/packages')), output, version, commit));
    }
  }
} catch (error) {
  console.error(`vibeface npm packaging: ${error.message}`);
  process.exitCode = 1;
}
