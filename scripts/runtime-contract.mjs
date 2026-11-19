// AUTO-16 Wave 0 runtime contract adapter. Production validators remain unchanged.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseReleaseVersion, platformFor, programNames, requiredRuntimeFiles, validateBuildInfoShape, PLATFORMS } from './release-contract.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const FIXTURE = path.join(ROOT, 'scripts/fixtures/release-contract.json');
export function loadRuntimeCorpus(file = FIXTURE) {
  const corpus = JSON.parse(fs.readFileSync(file, 'utf8'));
  assert(corpus.runtime && corpus.runtime.schema === 1, 'runtime corpus schema');
  return corpus.runtime;
}

function duplicateSafeParse(raw) {
  const seen = new Set();
  for (const match of raw.matchAll(/"((?:\\.|[^"\\])*)"\s*:/g)) {
    const key = JSON.parse(`"${match[1]}"`);
    if (seen.has(key)) throw new Error('duplicate JSON key');
    seen.add(key);
  }
  return JSON.parse(raw);
}
function fail(error) { const e = new Error(error); e.code = error; throw e; }

export function checkBuildInfo(value, contracts, runtime) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) fail('object');
  const expected = [...runtime.build_info.fields].sort();
  if (JSON.stringify(Object.keys(value).sort()) !== JSON.stringify(expected)) fail('fields');
  if (!runtime.targets.some(row => row.target === value.target)) fail('target');
  for (const field of ['source_commit', 'core_source_commit', 'app_source_commit']) {
    if (typeof value[field] !== 'string' || !/^[a-f0-9]{40}$/.test(value[field])) fail('commit');
  }
  for (const field of ['data_epoch', 'update_protocol']) {
    if (!Number.isInteger(value[field]) || value[field] < 0 || value[field] > 0xffffffff || value[field] !== contracts[field]) fail('contract');
  }
  for (const field of ['version', 'core_version', 'app_version']) {
    try { parseReleaseVersion(value[field]); } catch { fail('version'); }
  }
  try { validateBuildInfoShape(value, contracts); } catch { fail('invalid'); }
  return true;
}
export function checkBuildInfoRaw(raw, runtime) {
  for (const field of ['data_epoch', 'update_protocol']) {
    const token = new RegExp(`\"${field}\"\\s*:\\s*([^,}\\s]+)`).exec(raw)?.[1];
    if (!token || !/^(0|[1-9][0-9]*)$/.test(token)) fail('contract');
  }
  try { return checkBuildInfo(duplicateSafeParse(raw), runtime.contracts, runtime); }
  catch (error) { if (error.message === 'duplicate JSON key') fail('json-duplicate'); throw error; }
}

const HASH = /^[a-f0-9]{64}$/;
const SAFE_PATH = /^[A-Za-z0-9._/-]+$/;
export function checkFilesManifest(raw) {
  if (typeof raw !== 'string' || raw.charCodeAt(0) === 0xfeff || !raw.endsWith('\n')) fail('syntax');
  const lines = raw.split('\n'); lines.pop();
  if (!lines.length || lines.some(line => !line)) fail('syntax');
  const paths = [];
  for (const line of lines) {
    const match = /^([^ ]+)  (.+)$/.exec(line);
    if (!match) fail('syntax');
    if (!HASH.test(match[1])) fail('syntax');
    const name = match[2];
    if (name !== name.trim()) fail('syntax');
    if (name === 'FILES.sha256') fail('self-entry');
    if (name.startsWith('/') || name.includes('\\') || name.split('/').some(part => !part || part === '.' || part === '..') || !SAFE_PATH.test(name)) fail('path');
    paths.push(name);
  }
  if (new Set(paths).size !== paths.length) fail('duplicate');
  const sorted = [...paths].sort((a, b) => Buffer.from(a).compare(Buffer.from(b)));
  if (sorted.some((name, index) => name !== paths[index])) fail('order');
  return true;
}

export function checkRole(candidate, target, role, runtime) {
  const row = runtime.targets.find(item => item.target === target);
  if (!row || !Object.hasOwn(row.roles, role)) fail('role');
  const expected = row.roles[role];
  if (!Array.isArray(candidate) || !expected.every(name => candidate.includes(name))) fail('required');
  return true;
}

export function checkSeal(seal) {
  if (seal.symlink?.length) fail('symlink');
  if (seal.special?.length) fail('special');
  if (seal.changed?.length) fail('changed');
  const listed = new Set(seal.listed), actual = new Set(seal.actual);
  for (const name of listed) if (!actual.has(name)) fail('missing');
  for (const name of actual) if (!listed.has(name)) fail('unlisted');
  return true;
}

export function runRuntimeCorpus(runtime = loadRuntimeCorpus()) {
  const outcomes = [];
  const run = (item, operation) => {
    let accepted = true, error = 'ok';
    try { operation(item); } catch (exception) { accepted = false; error = exception.code || exception.message || 'error'; }
    outcomes.push({ id: item.id, accepted, error });
    assert.equal(accepted, item.accepted, `${item.id}: acceptance drift`);
    assert.equal(error, item.error, `${item.id}: error drift`);
  };
  for (const item of runtime.cases.build_info) run(item, value => item.kind === 'build-info-raw' ? checkBuildInfoRaw(item.raw, runtime) : checkBuildInfo(item.input, runtime.contracts, runtime));
  for (const item of runtime.cases.files) run(item, value => checkFilesManifest(item.raw));
  for (const item of runtime.cases.roles) run(item, value => checkRole(item.files, item.target, item.role, runtime));
  for (const item of runtime.cases.seals) run(item, value => checkSeal(item));
  for (const item of runtime.cases.compatibility) run(item, value => { if (JSON.stringify(item.wire) !== JSON.stringify(runtime.wire)) fail('wire'); });
  for (const item of runtime.cases.source) run(item, value => checkRustContracts(item.source, runtime));
  return outcomes;
}

function stripRustComments(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\r\n]*/g, '');
}
export function checkRustContracts(source, runtime) {
  const active = stripRustComments(source);
  for (const [field, key] of [['UPDATE_PROTOCOL', 'update_protocol'], ['DATA_EPOCH', 'data_epoch']]) {
    const matches = [...active.matchAll(new RegExp(`^\\s*pub\\s+const\\s+${field}\\s*:\\s*u32\\s*=\\s*([0-9]+)\\s*;`, 'gm'))]
      .filter(match => !/^\s*#(?:!\s*)?\[cfg(?:_attr)?\b/.test(active.slice(0, match.index).split('\n').at(-2) ?? ''));
    if (matches.length !== 1 || Number(matches[0][1]) !== runtime.contracts[key]) fail('source');
  }
  return true;
}

export function validateRuntimeCorpus(runtime = loadRuntimeCorpus()) {
  assert.deepEqual(runtime.build_info.fields, ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch', 'core_version', 'core_source_commit', 'app_version', 'app_source_commit']);
  assert.deepEqual(runtime.files.web_required, JSON.parse(fs.readFileSync(path.join(ROOT, 'scripts/package-node-web.json'), 'utf8')));
  assert.deepEqual(Object.keys(PLATFORMS), runtime.targets.map(row => row.target));
  for (const row of runtime.targets) {
    const platform = platformFor(row.target);
    assert.equal(row.host, platform.host); assert.equal(row.platform, platform.platform); assert.equal(row.flag, platform.flag);
    assert.equal(row.exe_suffix, platform.exe); assert.equal(row.service_suffix, platform.service); assert.equal(row.installer, platform.installer); assert.equal(row.signing, platform.signing); assert.equal(row.arch ?? null, platform.arch ?? null);
    assert.deepEqual(row.programs, programNames(row.target));
    assert.deepEqual(row.roles['node-full'], requiredRuntimeFiles(row.target, { web: runtime.files.web_required, checksum: true }));
    assert.deepEqual(row.roles['npm-vendor'], row.roles['node-full']);
    assert.deepEqual(row.roles['runtime-core'], [...row.programs, 'BUILD-INFO.json', 'FILES.sha256']);
  }
  const source = fs.readFileSync(path.join(ROOT, 'crates/control/src/lib.rs'), 'utf8');
  checkRustContracts(source, runtime);
  return true;
}
