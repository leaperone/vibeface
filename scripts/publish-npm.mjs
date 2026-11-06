#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';

const repository = 'leaperone/vibeface';
const workflow = '.github/workflows/release.yml';
const registry = 'https://registry.npmjs.org/';
const packageName = '@leaperone/vibeface';

function nightly(version) {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-nightly\.(\d{8})\.([1-9]\d*)$/.exec(version);
  assert(match, 'invalid canonical Nightly version');
  const day = `${match[4].slice(0, 4)}-${match[4].slice(4, 6)}-${match[4].slice(6, 8)}`;
  assert(Number(match[4].slice(0, 4)) > 0 && new Date(`${day}T00:00:00Z`).toISOString().startsWith(day), 'invalid Nightly date');
  return match.slice(1).map(BigInt);
}

function compare(left, right) {
  const before = nightly(right);
  return nightly(left).map((part, index) => part - before[index]).find(part => part !== 0n) ?? 0n;
}

function github(endpoint, key) {
  const args = ['api', `repos/${repository}/${endpoint}`];
  if (key) args.push('--paginate', '--slurp');
  const value = JSON.parse(execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }));
  return key ? value.flatMap(page => page[key]) : value;
}

function resolveArtifact({ tag, sha, runId, artifactId, workflowRef }, api = github, now = Date.now()) {
  assert(/^v(0|[1-9][0-9]*)\./.test(tag), 'product v tag is required');
  nightly(tag.slice(1));
  assert(/^[a-f0-9]{40}$/.test(sha), 'full source SHA is required');
  assert(/^[1-9]\d*$/.test(runId), 'current workflow run id is required');
  assert.equal(workflowRef, `${repository}/.github/workflows/${artifactId ? 'release.yml' : 'npm-publish.yml'}@refs/tags/${tag}`,
    'calling workflow mismatch or missing producer artifact id');
  let run;
  if (artifactId) {
    assert(/^[1-9]\d*$/.test(artifactId), 'invalid immutable artifact id');
    run = api(`actions/runs/${runId}`);
    assert.equal(String(run.id), runId, 'artifact must belong to the calling run');
  } else {
    const runs = api(`actions/workflows/release.yml/runs?head_sha=${sha}&status=success&per_page=100`, 'workflow_runs')
      .filter(item => item.head_sha === sha && item.head_branch === tag && item.path === workflow
        && item.status === 'completed' && item.conclusion === 'success');
    assert.equal(runs.length, 1, 'manual publication needs one successful release run for the exact tag');
    [run] = runs;
  }
  assert(Number.isSafeInteger(run.id) && run.id > 0, 'invalid release run id');
  assert.equal(run.repository?.full_name, repository, 'release repository mismatch');
  assert.equal(run.head_repository?.full_name, repository, 'release head repository mismatch');
  assert.equal(run.path, workflow, 'artifact must come from release.yml');
  assert.equal(run.head_sha, sha, 'release source SHA mismatch');
  assert.equal(run.head_branch, tag, 'release tag mismatch');
  assert(['push', 'workflow_dispatch'].includes(run.event), 'untrusted release event');
  assert(Number.isSafeInteger(run.run_attempt) && run.run_attempt > 0, 'invalid release run attempt');
  let artifact;
  if (artifactId) {
    artifact = api(`actions/artifacts/${artifactId}`);
    assert.equal(String(artifact.id), artifactId, 'immutable artifact id mismatch');
  } else {
    const artifacts = api(`actions/runs/${run.id}/artifacts?per_page=100`, 'artifacts')
      .filter(item => item.name.startsWith(`npm-package-${sha}-`) && !item.expired && Date.parse(item.expires_at) > now);
    assert.equal(artifacts.length, 1, 'manual publication needs one unexpired npm artifact');
    [artifact] = artifacts;
  }
  assert(Number.isSafeInteger(artifact.id) && artifact.id > 0, 'invalid artifact id');
  assert.equal(artifact.workflow_run?.id, run.id, 'artifact workflow run mismatch');
  assert.equal(artifact.workflow_run?.head_sha, sha, 'artifact source SHA mismatch');
  assert.equal(artifact.expired, false, 'npm artifact expired');
  assert(Date.parse(artifact.expires_at) > now && artifact.size_in_bytes > 0, 'npm artifact expired or empty');
  const attempt = new RegExp(`^npm-package-${sha}-([1-9]\\d*)$`).exec(artifact.name)?.[1];
  assert(attempt && BigInt(attempt) <= BigInt(run.run_attempt), 'artifact producer attempt mismatch');
  return { artifact_id: String(artifact.id), run_id: String(run.id) };
}

async function readRegistry(request = fetch) {
  const response = await request(`${registry}@leaperone%2fvibeface`, {
    headers: { accept: 'application/json', 'cache-control': 'no-cache' },
    signal: AbortSignal.timeout(15000), redirect: 'error',
  });
  if (response.status === 404) return null;
  assert(response.ok, `npm registry read failed with HTTP ${response.status}`);
  const value = await response.json();
  assert(value !== null, 'invalid npm packument');
  return value;
}

function registryState(packument, version, integrity) {
  if (packument === null) return 'absent';
  const object = value => value && typeof value === 'object' && !Array.isArray(value);
  assert(object(packument) && object(packument.versions) && object(packument['dist-tags']), 'invalid npm packument');
  const current = packument.versions[version];
  const latest = packument['dist-tags'].nightly;
  if (latest !== undefined) {
    nightly(latest);
    assert(typeof packument.versions[latest]?.dist?.integrity === 'string', 'nightly points to a missing registry version');
  }
  if (current !== undefined) {
    assert.equal(current?.dist?.integrity, integrity, 'published version integrity mismatch');
    return latest !== undefined && compare(latest, version) >= 0n ? 'complete' : 'unreconciled';
  }
  assert(latest !== undefined && compare(version, latest) > 0n, 'Nightly must advance; reconcile the registry before publishing');
  return 'absent';
}

async function publishOnce(version, integrity, { read = readRegistry, publish, pause = sleep, attempts = 40 }) {
  const state = registryState(await read(), version, integrity);
  if (state === 'complete') return;
  assert.equal(state, 'absent', 'existing version has a missing or older nightly tag; reconcile without republishing');
  let result;
  try { result = await publish(); } catch (error) { result = error.message; }
  let lastError = 'version or nightly tag is not visible yet';
  for (let attempt = 0; attempt < attempts; attempt++) {
    if (attempt) await pause(15000);
    try {
      if (registryState(await read(), version, integrity) === 'complete') return;
    } catch (error) {
      if (error.message.includes('integrity mismatch')) throw error;
      lastError = error.message;
    }
  }
  throw new Error(`npm publication is unverified (${result}); ${lastError}. Reconcile the registry before retrying.`);
}

async function selfTest() {
  const sha = 'a'.repeat(40), version = '0.0.1-nightly.20261106.1', newer = '0.0.1-nightly.20261106.2';
  const tag = `v${version}`, integrity = 'sha512-fixture';
  const run = { id: 7, head_sha: sha, head_branch: tag, path: workflow, run_attempt: 2, event: 'push',
    repository: { full_name: repository }, head_repository: { full_name: repository }, status: 'in_progress', conclusion: null };
  const artifact = { id: 11, name: `npm-package-${sha}-1`, expired: false, expires_at: '2099-01-01T00:00:00Z',
    size_in_bytes: 100, workflow_run: { id: 7, head_sha: sha } };
  const context = { tag, sha, runId: '7', artifactId: '11', workflowRef: `${repository}/${workflow}@refs/tags/${tag}` };
  const manual = { ...context, artifactId: '', workflowRef: `${repository}/.github/workflows/npm-publish.yml@refs/tags/${tag}` };
  const api = (endpoint, key) => key === 'workflow_runs' ? [{ ...run, status: 'completed', conclusion: 'success' }]
    : key === 'artifacts' ? [artifact] : endpoint.includes('/artifacts/') ? artifact : run;
  assert.deepEqual(resolveArtifact(context, api), { artifact_id: '11', run_id: '7' });
  assert.deepEqual(resolveArtifact(manual, api), { artifact_id: '11', run_id: '7' });
  assert.throws(() => resolveArtifact({ ...context, artifactId: '' }, api), /calling workflow mismatch/);
  for (const change of [{ workflow_run: { id: 8, head_sha: sha } }, { workflow_run: { id: 7, head_sha: 'b'.repeat(40) } },
    { name: `npm-package-${sha}-3` }, { name: `npm-package-${sha}-01` }, { expired: true }, { expires_at: '2000-01-01' }]) {
    assert.throws(() => resolveArtifact(context, endpoint => endpoint.includes('/artifacts/') ? { ...artifact, ...change } : run));
  }
  for (const change of [{ head_sha: 'b'.repeat(40) }, { head_branch: 'main' }, { path: '.github/workflows/ci.yml' }, { id: 8 }]) {
    assert.throws(() => resolveArtifact(context, endpoint => endpoint.includes('/artifacts/') ? artifact : { ...run, ...change }));
  }
  assert.throws(() => resolveArtifact(manual, () => []), /one successful release run/);
  assert.throws(() => resolveArtifact(manual, (endpoint, key) => key === 'workflow_runs'
    ? [api(endpoint, key)[0], api(endpoint, key)[0]] : api(endpoint, key)), /one successful release run/);
  const packument = (latest = version, published = true) => ({
    versions: { ...(published ? { [version]: { dist: { integrity } } } : {}),
      ...(latest && latest !== version ? { [latest]: { dist: { integrity: 'sha512-other' } } } : {}) },
    'dist-tags': latest ? { nightly: latest } : {},
  });
  assert.equal(registryState(packument(), version, integrity), 'complete');
  assert.equal(registryState(packument(newer), version, integrity), 'complete');
  assert.equal(registryState(null, version, integrity), 'absent');
  assert.equal(registryState(packument('0.0.1-nightly.20261105.1', false), version, integrity), 'absent');
  assert.throws(() => registryState(packument(), version, 'sha512-different'), /integrity mismatch/);
  assert.throws(() => registryState(packument(newer, false), version, integrity), /must advance/);
  for (const value of [{}, [], { versions: [], 'dist-tags': {} }, { versions: {}, 'dist-tags': { nightly: 'latest' } }]) {
    assert.throws(() => registryState(value, version, integrity));
  }
  assert.equal(await readRegistry(async () => ({ status: 404 })), null);
  await assert.rejects(readRegistry(async () => ({ status: 503, ok: false })), /HTTP 503/);
  await assert.rejects(readRegistry(async () => ({ status: 200, ok: true, json: async () => null })), /invalid npm packument/);
  await assert.rejects(readRegistry(async () => { throw new Error('transport failed'); }), /transport failed/);
  await assert.rejects(readRegistry(async () => ({ status: 200, ok: true, json: async () => { throw new Error('invalid JSON'); } })), /invalid JSON/);
  let writes = 0;
  const publish = () => { writes++; throw new Error('publish response lost'); };
  for (const value of [packument(), packument(newer)]) {
    await publishOnce(version, integrity, { read: async () => value, publish });
    assert.equal(writes, 0);
  }
  for (const latest of [null, '0.0.1-nightly.20261105.1']) {
    await assert.rejects(publishOnce(version, integrity, { read: async () => packument(latest), publish }), /reconcile without republishing/);
    assert.equal(writes, 0);
  }
  await assert.rejects(publishOnce(version, 'sha512-different', { read: async () => packument(), publish }), /integrity mismatch/);
  assert.equal(writes, 0);
  const reads = [packument('0.0.1-nightly.20261105.1', false), null, new Error('transient read failed'), packument(null), packument(newer)];
  await publishOnce(version, integrity, { read: async () => {
    const value = reads.shift();
    if (value instanceof Error) throw value;
    return value;
  }, publish, pause: async () => {}, attempts: 4 });
  assert.equal(writes, 1);
  await assert.rejects(publishOnce(version, integrity, { read: async () => null, publish, pause: async () => {}, attempts: 2 }), /unverified/);
  assert.equal(writes, 2);
  for (const read of [async () => ({}), async () => { throw new Error('offline'); }]) {
    await assert.rejects(publishOnce(version, integrity, { read, publish }));
    assert.equal(writes, 2);
  }
  console.log('npm publication self-test passed: exact source and artifact, original producer attempt reuse, registry integrity, monotonic Nightly, and publish-once reconciliation.');
}

try {
  const [command, ...extra] = process.argv.slice(2);
  assert.equal(extra.length, 0, 'unexpected arguments');
  if (command === '--self-test') {
    await selfTest();
  } else {
    assert.equal(process.env.GITHUB_REPOSITORY, repository, 'publication only accepts the upstream repository');
    const tag = process.env.RELEASE_TAG ?? '';
    assert.equal(process.env.GITHUB_REF, `refs/tags/${tag}`, 'workflow ref must match the release tag');
    assert(/^v(0|[1-9][0-9]*)\./.test(tag), 'product v tag is required');
    const version = tag.slice(1);
    nightly(version);
    if (command === 'artifact') {
      const result = resolveArtifact({ tag, sha: process.env.GITHUB_SHA, runId: process.env.GITHUB_RUN_ID,
        artifactId: process.env.NPM_ARTIFACT_ID, workflowRef: process.env.GITHUB_WORKFLOW_REF });
      appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(result).map(([key, value]) => `${key}=${value}\n`).join(''));
      console.log(`Verified artifact ${result.artifact_id} from release run ${result.run_id}.`);
    } else {
      assert.equal(command, 'publish', 'expected artifact, publish, or --self-test');
      const archive = `.artifacts/npm/leaperone-vibeface-${version}.tgz`;
      const receipt = JSON.parse(readFileSync(`${archive}.json`, 'utf8'));
      assert.equal(receipt.version, version);
      assert.equal(receipt.source_commit, process.env.GITHUB_SHA);
      assert(/^sha512-[A-Za-z0-9+/]+={0,2}$/.test(receipt.integrity), 'invalid package integrity');
      await publishOnce(version, receipt.integrity, { publish: () => {
        const result = spawnSync('npm', ['publish', archive, '--tag', 'nightly', '--access', 'public', '--provenance',
          '--ignore-scripts', `--registry=${registry}`], { stdio: 'inherit' });
        return result.error?.message ?? `npm exit ${result.status}`;
      } });
      console.log(`Verified ${packageName}@${version} integrity with nightly at this version or newer.`);
    }
  }
} catch (error) {
  console.error(`vibeface npm publication: ${error.message}`);
  process.exitCode = 1;
}
