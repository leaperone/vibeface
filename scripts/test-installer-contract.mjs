#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const root = path.resolve(new URL('..', import.meta.url).pathname);
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
  corpus.runtime.targets[0].host = corpus.runtime.targets[1].host;
  fs.writeFileSync(fixture, `${JSON.stringify(corpus, null, 2)}\n`);
  assert.equal(run('--check').status, 1, 'duplicate host/target row rejected');
  corpus.runtime.targets[0].host = 'darwin-arm64';
  corpus.runtime.targets[0].roles['node-full'].push('extra.txt');
  fs.writeFileSync(fixture, `${JSON.stringify(corpus, null, 2)}\n`);
  assert.equal(run('--check').status, 1, 'divergent POSIX role set rejected');
} finally { fs.writeFileSync(fixture, originalCorpus); }
assert.equal(run('--check').status, 0, 'fixtures restored after negative tests');
console.log('Installer contract generator self-test passed');
