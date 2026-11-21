#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { createHash } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { loadRuntimeCorpus } from './runtime-contract.mjs';
const root = fileURLToPath(new URL('..', import.meta.url));
const generator = path.join(root, 'scripts/generate-installer-contract.mjs');
const fixture = path.join(root, 'scripts/fixtures/release-contract.json');
const run = (...args) => spawnSync(process.execPath, [generator, ...args], { cwd: root, encoding: 'utf8' });
const read = file => fs.readFileSync(file, 'utf8');
const bashFile = path.join(root, 'install.sh');
const psFile = path.join(root, 'scripts/install.ps1');
const bash = read(bashFile), ps = read(psFile);
for (const [name, text] of [['install.sh', bash], ['scripts/install.ps1', ps]]) {
  assert.equal((text.match(/^# BEGIN AUTO-16 GENERATED INSTALLER CONTRACTS$/gm) || []).length, 1, `${name} begin marker`);
  assert.equal((text.match(/^# END AUTO-16 GENERATED INSTALLER CONTRACTS$/gm) || []).length, 1, `${name} end marker`);
  assert(text.includes('Source: scripts/fixtures/release-contract.json + scripts/package-node-web.json'));
  assert(text.match(/Source projection SHA-256: [0-9a-f]{64}/));
}
// Independent facts from the normative corpus, rather than the renderer output, pin the contract.
assert.match(bash, /AUTO16_BUILD_FIELDS='version target source_commit update_protocol data_epoch core_version core_source_commit app_version app_source_commit'/);
assert.match(bash, /AUTO16_BUILD_FIELD_COUNT='9'/);
assert.match(bash, /AUTO16_BUILD_NUMBER_FIELDS='update_protocol data_epoch'/);
assert.match(bash, /AUTO16_DATA_EPOCH='1'/);
assert.match(bash, /AUTO16_UPDATE_PROTOCOL='1'/);
assert.match(bash, /AUTO16_TARGET_DARWIN_ARM64='aarch64-apple-darwin'/);
assert.match(bash, /AUTO16_TARGET_LINUX_X64='x86_64-unknown-linux-gnu'/);
assert.match(bash, /AUTO16_TARGET_LINUX_ARM64='aarch64-unknown-linux-gnu'/);
assert.match(bash, /AUTO16_NODE_REQUIRED_PATHS=\('vibeface' 'vibeface-core' 'BUILD-INFO.json'/);
assert.match(bash, /web\/vibeface_secure_bg\.wasm/);
assert.match(ps, /\$Auto16Target = 'x86_64-pc-windows-msvc'/);
assert.match(ps, /\$Auto16ProgramCli = 'vibeface\.exe'/);
assert.match(ps, /scripts\\install-node-service\.ps1/);
assert.equal(run('--check').status, 0, 'checked-in blocks are current');
const before = [read(bashFile), read(psFile)];
assert.equal(run('--check').status, 0);
assert.deepEqual(before, [read(bashFile), read(psFile)], 'check is read-only');
assert.equal(run('--write').status, 0, 'write is idempotent');
assert.deepEqual(before, [read(bashFile), read(psFile)], 'write is idempotent');
function mutate(file, edit, expected = 1) {
  const original = read(file);
  try { fs.writeFileSync(file, edit(original)); assert.equal(run('--check').status, expected); }
  finally { fs.writeFileSync(file, original); }
}
mutate(bashFile, text => text.replace('Source projection SHA-256:', 'Source projection SHA-256: stale-'), 1);
mutate(bashFile, text => text.replace(/^# BEGIN AUTO-16 GENERATED INSTALLER CONTRACTS$/m, '$&\n# duplicate'), 1);
const originalCorpus = read(fixture);
try {
  const corpus = JSON.parse(originalCorpus);
  corpus.runtime.targets[0].host = 'unsafe$host';
  fs.writeFileSync(fixture, `${JSON.stringify(corpus, null, 2)}\n`);
  assert.equal(run('--check').status, 1, 'unsafe literal rejected');
  corpus.runtime.targets[0].target = corpus.runtime.targets[1].target;
  fs.writeFileSync(fixture, `${JSON.stringify(corpus, null, 2)}\n`);
  assert.equal(run('--check').status, 1, 'duplicate target row rejected');
  corpus.runtime.targets[0].target = 'aarch64-apple-darwin';
  corpus.runtime.targets[0].host = 'darwin-arm64';
  corpus.runtime.targets[0].roles['node-full'].push('extra.txt');
  fs.writeFileSync(fixture, `${JSON.stringify(corpus, null, 2)}\n`);
  assert.equal(run('--check').status, 1, 'divergent POSIX role set rejected');
} finally { fs.writeFileSync(fixture, originalCorpus); }
assert.equal(run('--check').status, 0, 'fixtures restored after negative tests');
console.log('Installer contract generator self-test passed');

function checkInstallerResult(test, result) {
  assert.ifError(result.error);
  assert.equal(result.status, test.accepted ? 0 : 1, test.id + ': ' + result.stderr);
}

const runtime = loadRuntimeCorpus(fixture);
const posixTargets = new Set(runtime.targets.filter(row => row.service_suffix === 'sh').map(row => row.target));
const buildCases = runtime.cases.build_info.filter(test => !test.accepted || posixTargets.has(test.input.target));
const baseline = buildCases.find(test => test.id === 'build-valid-hourly-aarch64-apple-darwin').input;
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'vibeface-installer-contract-'));
const runInstaller = (...args) => spawnSync('bash', ['-c', 'source "$1"; shift; "$@"', 'installer-contract', bashFile, ...args], {
  cwd: root, encoding: 'utf8',
});
try {
  const buildFile = path.join(scratch, 'BUILD-INFO.json');
  const malformed = { id: 'build-missing-comma', accepted: false, kind: 'build-info-raw',
    raw: JSON.stringify(baseline).replace(',"target"', ' "target"') };
  for (const test of [...buildCases, malformed]) {
    const identity = test.accepted ? test.input : baseline;
    const pins = ['', '', '', '', '', identity.core_version, identity.core_source_commit, identity.app_version, identity.app_source_commit].join('\t');
    fs.writeFileSync(buildFile, test.kind === 'build-info-raw' ? test.raw : JSON.stringify(test.input));
    checkInstallerResult(test, runInstaller('verify_build_info', buildFile, identity.version, identity.target, identity.source_commit, scratch, pins));
  }

  const filesManifest = path.join(scratch, 'FILES.sha256');
  for (const test of runtime.cases.files) {
    fs.writeFileSync(filesManifest, test.raw);
    checkInstallerResult(test, runInstaller('parse_file_checksums', filesManifest));
  }

  const acceptedSeal = { id: 'seal-valid', accepted: true, listed: ['BUILD-INFO.json', 'vibeface'], actual: ['BUILD-INFO.json', 'vibeface'] };
  for (const test of [acceptedSeal, ...runtime.cases.seals]) {
    const bundle = path.join(scratch, test.id);
    fs.mkdirSync(bundle);
    for (const name of test.actual) {
      const destination = path.join(bundle, name);
      if (test.symlink?.includes(name)) { fs.symlinkSync('BUILD-INFO.json', destination); continue; }
      if (test.special?.includes(name)) {
        const fifo = spawnSync('mkfifo', [destination], { encoding: 'utf8' });
        assert.ifError(fifo.error);
        assert.equal(fifo.status, 0, fifo.stderr);
        continue;
      }
      fs.writeFileSync(destination, test.changed?.includes(name) ? 'changed\n' : name + '\n');
    }
    const manifest = [...test.listed].sort().map(name =>
      createHash('sha256').update(name + '\n').digest('hex') + '  ' + name + '\n',
    ).join('');
    fs.writeFileSync(path.join(bundle, 'FILES.sha256'), manifest);
    checkInstallerResult(test, runInstaller('verify_file_checksums', bundle, scratch));
  }
} finally {
  fs.rmSync(scratch, { recursive: true, force: true });
}
const streamedHelp = spawnSync('bash', ['-s', '--', '--help'], { input: bash, cwd: root, encoding: 'utf8' });
assert.ifError(streamedHelp.error);
assert.equal(streamedHelp.status, 0, streamedHelp.stderr);
assert.match(streamedHelp.stdout, /^Usage: install\.sh/);
console.log('POSIX installer accepted and rejected ' + buildCases.length + ' shared BUILD-INFO and ' + runtime.cases.files.length + ' FILES.sha256 vectors, plus ' + runtime.cases.seals.length + ' shared seal rejections.');
