#!/usr/bin/env node
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const root = path.resolve(new URL('..', import.meta.url).pathname);
const generator = path.join(root, 'scripts/generate-installer-contract.mjs');
const run = (...args) => spawnSync(process.execPath, [generator, ...args], { cwd: root, encoding: 'utf8' });
const bash = fs.readFileSync(path.join(root, 'install.sh'), 'utf8');
const ps = fs.readFileSync(path.join(root, 'scripts/install.ps1'), 'utf8');
for (const [name, text] of [['install.sh', bash], ['scripts/install.ps1', ps]]) {
  assert.equal((text.match(/^# BEGIN AUTO-16 GENERATED INSTALLER CONTRACTS$/gm) || []).length, 1, `${name} begin marker`);
  assert.equal((text.match(/^# END AUTO-16 GENERATED INSTALLER CONTRACTS$/gm) || []).length, 1, `${name} end marker`);
  assert(text.includes('Source projection SHA-256: '));
}
assert.equal(run('--check').status, 0, 'checked-in blocks are current');
const before = [bash, ps];
assert.equal(run('--check').status, 0);
assert.deepEqual(before, [fs.readFileSync(path.join(root, 'install.sh'), 'utf8'), fs.readFileSync(path.join(root, 'scripts/install.ps1'), 'utf8')], 'check is read-only');
assert.equal(run('--write').status, 0, 'write is idempotent');
assert.deepEqual(before, [fs.readFileSync(path.join(root, 'install.sh'), 'utf8'), fs.readFileSync(path.join(root, 'scripts/install.ps1'), 'utf8')], 'write is idempotent');
assert.match(bash, /AUTO16_BUILD_FIELD_COUNT='9'/);
assert.match(bash, /AUTO16_DATA_EPOCH='1'/);
assert.match(bash, /AUTO16_NODE_REQUIRED_PATHS=\([^)]*web\/vibeface_secure_bg\.wasm/);
assert.match(ps, /\$Auto16Target = 'x86_64-pc-windows-msvc'/);
assert.match(ps, /scripts\\install-node-service\.ps1/);
console.log('Installer contract generator self-test passed');
