#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';

import { parseReleaseVersion, parseReleaseTag, npmDistTag, compareWithinChannel } from './release-contract.mjs';

const repository = 'leaperone/vibeface';
const workflow = '.github/workflows/release.yml';
const producerJob = 'collect';
const publisherJob = 'publish-cli';
const registry = 'https://registry.npmjs.org/';
const packageName = '@leaperone/vibeface';

function github(endpoint, key) {
  const args = ['api', `repos/${repository}/${endpoint}`];
  if (key) args.push('--paginate', '--slurp');
  const value = JSON.parse(execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }));
  return key ? value.flatMap(page => page[key]) : value;
}

function completedJob(api, runId, leaf, message) {
  const jobs = api(`actions/runs/${runId}/jobs?filter=latest&per_page=100`, 'jobs')
    .filter(job => typeof job.name === 'string' && job.name.split(' / ').pop() === leaf);
  assert.equal(jobs.length, 1, `the release run must contain exactly one ${leaf} job`);
  assert.equal(jobs[0].status, 'completed', message);
  assert.equal(jobs[0].conclusion, 'success', message);
}

function resolveArtifact({ tag, sha, runId, artifactId, workflowRef }, api = github, now = Date.now()) {
  assert.equal(parseReleaseTag(tag).app, 'cli', 'CLI cli-v tag is required');
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
    // A manual recovery may follow a published CLI whose npm job failed.
    const runs = api(`actions/workflows/release.yml/runs?head_sha=${sha}&per_page=100`, 'workflow_runs')
      .filter(item => item.head_sha === sha && item.head_branch === tag && item.path === workflow
        && item.status === 'completed');
    assert.equal(runs.length, 1, 'manual publication needs one completed release run for the exact tag');
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
  completedJob(api, run.id, producerJob, 'the complete release artifact producer did not succeed');
  completedJob(api, run.id, publisherJob, 'the CLI release must be published before npm');
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
  const channel = npmDistTag(parseReleaseVersion(version).channel);
  if (packument === null) return 'absent';
  const object = value => value && typeof value === 'object' && !Array.isArray(value);
  assert(object(packument) && object(packument.versions) && object(packument['dist-tags']), 'invalid npm packument');
  const current = packument.versions[version];
  const latest = packument['dist-tags'][channel];
  if (latest !== undefined) {
    assert.equal(npmDistTag(parseReleaseVersion(latest).channel), channel, 'npm dist-tag must point to its own CLI channel');
    assert(typeof packument.versions[latest]?.dist?.integrity === 'string', `${channel} points to a missing registry version`);
  }
  if (current !== undefined) {
    assert.equal(current?.dist?.integrity, integrity, 'published version integrity mismatch');
    return latest !== undefined && compareWithinChannel(latest, version) >= 0n ? 'complete' : 'unreconciled';
  }
  // Releases can finish out of order; an older version must not move its dist-tag back.
  return latest === undefined || compareWithinChannel(version, latest) > 0n ? 'absent' : 'superseded';
}

async function publishOnce(version, integrity, { read = readRegistry, publish, pause = sleep, attempts = 60 }) {
  const state = registryState(await read(), version, integrity);
  if (state === 'complete' || state === 'superseded') return state;
  assert.equal(state, 'absent', 'existing version has a missing or older channel tag; reconcile without republishing');
  let result;
  try { result = await publish(); } catch (error) { result = error.message; }
  let lastError = 'version or channel tag is not visible yet';
  for (let attempt = 0; attempt < attempts; attempt++) {
    if (attempt) await pause(15000);
    try {
      if (registryState(await read(), version, integrity) === 'complete') return 'complete';
    } catch (error) {
      if (error.message.includes('integrity mismatch')) throw error;
      lastError = error.message;
    }
  }
  throw new Error(`npm publication is unverified (${result}); ${lastError}. Reconcile the registry before retrying.`);
}

async function selfTest() {
  const sha = 'a'.repeat(40), version = '0.0.1-hourly.2026110610', newer = '0.0.1-hourly.2026110611';
  const tag = `cli-v${version}`, integrity = 'sha512-fixture';
  const run = { id: 7, head_sha: sha, head_branch: tag, path: workflow, run_attempt: 2, event: 'push',
    repository: { full_name: repository }, head_repository: { full_name: repository }, status: 'in_progress', conclusion: null };
  const artifact = { id: 11, name: `npm-package-${sha}-1`, expired: false, expires_at: '2099-01-01T00:00:00Z',
    size_in_bytes: 100, workflow_run: { id: 7, head_sha: sha } };
  const context = { tag, sha, runId: '7', artifactId: '11', workflowRef: `${repository}/${workflow}@refs/tags/${tag}` };
  const manual = { ...context, artifactId: '', workflowRef: `${repository}/.github/workflows/npm-publish.yml@refs/tags/${tag}` };
  const jobs = [{ name: 'collect', status: 'completed', conclusion: 'success' },
    { name: 'publish-cli', status: 'completed', conclusion: 'success' },
    { name: 'publish', status: 'completed', conclusion: 'skipped' },
    { name: 'npm / npm', status: 'completed', conclusion: 'failure' }];
  const api = (endpoint, key) => key === 'workflow_runs' ? [{ ...run, status: 'completed', conclusion: 'failure' }]
    : key === 'jobs' ? jobs : key === 'artifacts' ? [artifact] : endpoint.includes('/artifacts/') ? artifact : run;
  assert.deepEqual(resolveArtifact(context, api), { artifact_id: '11', run_id: '7' });
  for (const candidate of ['1.2.3', '1.2.3-rc.2']) {
    const cliTag = `cli-v${candidate}`;
    const candidateContext = { ...context, tag: cliTag, workflowRef: `${repository}/${workflow}@refs/tags/${cliTag}` };
    assert.deepEqual(resolveArtifact(candidateContext, (endpoint, key) => {
      const value = api(endpoint, key);
      return key || endpoint.includes('/artifacts/') ? value : { ...value, head_branch: cliTag };
    }), { artifact_id: '11', run_id: '7' });
  }
  assert.throws(() => resolveArtifact({ ...context, tag: `v${version}` }, api), /invalid release tag/);
  // A published CLI whose npm job failed is still recoverable by hand.
  assert.deepEqual(resolveArtifact(manual, api), { artifact_id: '11', run_id: '7' });
  assert.throws(() => resolveArtifact({ ...context, artifactId: '' }, api), /calling workflow mismatch/);
  for (const change of [{ workflow_run: { id: 8, head_sha: sha } }, { workflow_run: { id: 7, head_sha: 'b'.repeat(40) } },
    { name: `npm-package-${sha}-3` }, { name: `npm-package-${sha}-01` }, { expired: true }, { expires_at: '2000-01-01' }]) {
    assert.throws(() => resolveArtifact(context, (endpoint, key) => endpoint.includes('/artifacts/') ? { ...artifact, ...change } : api(endpoint, key)));
  }
  for (const change of [{ head_sha: 'b'.repeat(40) }, { head_branch: 'main' }, { path: '.github/workflows/ci.yml' }, { id: 8 }]) {
    assert.throws(() => resolveArtifact(context, (endpoint, key) => endpoint.includes('/artifacts/') ? artifact : key === 'jobs' ? jobs : { ...run, ...change }));
  }
  const withJobs = replacement => (endpoint, key) => (key === 'jobs' ? replacement : api(endpoint, key));
  assert.throws(() => resolveArtifact(context, withJobs([{ ...jobs[0], conclusion: 'failure' }, jobs[1]])), /producer did not succeed/);
  assert.throws(() => resolveArtifact(context, withJobs([{ ...jobs[0], status: 'in_progress', conclusion: null }, jobs[1]])), /producer did not succeed/);
  assert.throws(() => resolveArtifact(context, withJobs([jobs[0], { ...jobs[1], conclusion: 'failure' }])), /published before npm/);
  assert.throws(() => resolveArtifact(context, withJobs([jobs[1]])), /exactly one collect job/);
  assert.throws(() => resolveArtifact(context, withJobs([jobs[0], jobs[0], jobs[1]])), /exactly one collect job/);
  for (const request of [context, manual]) {
    assert.throws(() => resolveArtifact(request, withJobs([jobs[0]])), /exactly one publish-cli job/);
    assert.throws(() => resolveArtifact(request, withJobs([jobs[0], jobs[1], jobs[1]])), /exactly one publish-cli job/);
    assert.throws(() => resolveArtifact(request, withJobs([jobs[0], { name: 'publish', status: 'completed', conclusion: 'success' }])), /exactly one publish-cli job/);
    for (const conclusion of ['failure', 'skipped']) {
      assert.throws(() => resolveArtifact(request, withJobs([jobs[0], { ...jobs[1], conclusion }])), /published before npm/);
    }
    assert.throws(() => resolveArtifact(request, withJobs([jobs[0], { ...jobs[1], status: 'in_progress', conclusion: null }])), /published before npm/);
  }
  assert.throws(() => resolveArtifact(manual, () => []), /one completed release run/);
  assert.throws(() => resolveArtifact(manual, (endpoint, key) => key === 'workflow_runs'
    ? [api(endpoint, key)[0], api(endpoint, key)[0]] : api(endpoint, key)), /one completed release run/);
  assert.throws(() => resolveArtifact(manual, (endpoint, key) => key === 'workflow_runs'
    ? [{ ...run, status: 'in_progress', conclusion: null }] : api(endpoint, key)), /one completed release run/);
  const packument = (latest = version, published = true) => ({
    versions: { ...(published ? { [version]: { dist: { integrity } } } : {}),
      ...(latest && latest !== version ? { [latest]: { dist: { integrity: 'sha512-other' } } } : {}) },
    'dist-tags': latest ? { hourly: latest } : {},
  });
  for (const [candidate, previous, next, channel] of [
    ['1.10.0', '1.9.9', '1.11.0', 'latest'],
    ['1.2.3-rc.10', '1.2.3-rc.9', '1.2.3-rc.11', 'rc'],
  ]) {
    const registry = (latest, published = false) => ({
      versions: { ...(published ? { [candidate]: { dist: { integrity } } } : {}),
        [latest]: { dist: { integrity: latest === candidate ? integrity : 'sha512-other' } } },
      'dist-tags': { [channel]: latest, hourly: newer },
    });
    assert.equal(registryState(registry(previous), candidate, integrity), 'absent');
    assert.equal(registryState(registry(next), candidate, integrity), 'superseded');
    assert.equal(registryState(registry(next, true), candidate, integrity), 'complete');
    assert.throws(() => registryState(registry(candidate, true), candidate, 'sha512-mismatch'), /integrity mismatch/);
    let writes = 0;
    const reads = [registry(previous), registry(candidate, true)];
    assert.equal(await publishOnce(candidate, integrity, { read: async () => reads.shift(),
      publish: () => { writes++; throw new Error('publish response lost'); }, pause: async () => {}, attempts: 1 }), 'complete');
    assert.equal(writes, 1);
    assert.equal(await publishOnce(candidate, integrity, { read: async () => registry(next),
      publish: () => { writes++; } }), 'superseded');
    assert.equal(writes, 1);
    assert.throws(() => registryState({ versions: { [version]: { dist: { integrity } } },
      'dist-tags': { [channel]: version } }, candidate, integrity), /own CLI channel/);
  }
  assert.equal(registryState(packument(), version, integrity), 'complete');
  assert.equal(registryState(packument(newer), version, integrity), 'complete');
  assert.equal(registryState(null, version, integrity), 'absent');
  assert.equal(registryState(packument('0.0.1-hourly.2026110523', false), version, integrity), 'absent');
  assert.throws(() => registryState(packument(), version, 'sha512-different'), /integrity mismatch/);
  assert.equal(registryState(packument(newer, false), version, integrity), 'superseded');
  assert.equal(registryState({ versions: {}, 'dist-tags': {} }, version, integrity), 'absent');
  assert.equal(await publishOnce(version, integrity, { read: async () => packument(newer, false), publish: () => assert.fail('superseded must not publish') }), 'superseded');
  assert.throws(() => parseReleaseVersion('0.0.1-nightly.20261106.1'), /invalid component release version/);
  assert.throws(() => parseReleaseVersion('0.0.1-hourly.2026110624'), /invalid hourly date/);
  for (const value of [{}, [], { versions: [], 'dist-tags': {} }, { versions: {}, 'dist-tags': { hourly: 'latest' } }]) {
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
  for (const latest of [null, '0.0.1-hourly.2026110523']) {
    await assert.rejects(publishOnce(version, integrity, { read: async () => packument(latest), publish }), /reconcile without republishing/);
    assert.equal(writes, 0);
  }
  await assert.rejects(publishOnce(version, 'sha512-different', { read: async () => packument(), publish }), /integrity mismatch/);
  assert.equal(writes, 0);
  const reads = [packument('0.0.1-hourly.2026110523', false), null, new Error('transient read failed'), packument(null), packument(newer)];
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
  console.log('npm publication self-test passed: CLI tag/source/artifact trust, successful collect and publish-cli jobs, stable/rc/hourly channel ordering, registry integrity, failure recovery and publish-once reconciliation.');
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
    assert.equal(parseReleaseTag(tag).app, 'cli', 'CLI cli-v tag is required');
    const version = parseReleaseTag(tag).version;
    const channel = npmDistTag(parseReleaseVersion(version).channel);
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
      const state = await publishOnce(version, receipt.integrity, { publish: () => {
        const result = spawnSync('npm', ['publish', archive, '--tag', channel, '--access', 'public', '--provenance',
          '--ignore-scripts', `--registry=${registry}`], { stdio: 'inherit' });
        return result.error?.message ?? `npm exit ${result.status}`;
      } });
      console.log(state === 'superseded'
        ? `Skipped ${packageName}@${version}; ${channel} already points to a newer version.`
        : `Verified ${packageName}@${version} integrity with ${channel} at this version or newer.`);
    }
  }
} catch (error) {
  console.error(`vibeface npm publication: ${error.message}`);
  process.exitCode = 1;
}
