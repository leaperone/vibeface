import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { artifact, requireDependency, root } from './compile.mjs';

const [workItem, directory, inputDirectory] = process.argv.slice(2);
assert(process.argv.length === 4 || process.argv.length === 5,
  'usage: experiment-delivery.mjs <prepare|relation-N|accept> <new-absolute-run-directory> [normalized-public-input-directory]');
assert(/^(prepare|relation-[1-4]|accept)$/.test(workItem));
assert(path.isAbsolute(directory));
assert(inputDirectory === undefined || path.isAbsolute(inputDirectory));
fs.mkdirSync(directory, { mode: 0o700 });
const privateDir = path.join(directory, 'private'), publicDir = path.join(directory, 'public');
const proofDir = path.join(privateDir, 'proofs'), material = path.join(privateDir, 'material');
for (const name of [privateDir, publicDir, proofDir]) fs.mkdirSync(name, { mode: 0o700 });
const encode = value => `${JSON.stringify(value, (_, item) => typeof item === 'bigint' ? item.toString() : item, 2)}\n`;
const write = (file, value) => fs.writeFileSync(file, encode(value), { flag: 'wx', mode: 0o600 });
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const read = (file, limit) => {
  const stat = fs.lstatSync(file);
  assert(stat.isFile() && stat.nlink === 1 && stat.size > 0 && stat.size <= limit, 'bounded regular file without links required');
  return fs.readFileSync(file);
};
const readJson = (file, limit = 1024 * 1024) => JSON.parse(read(file, limit));
const prover = '/artifacts/proof-target/release/vibeface-delivery-prover';
const fixture = '/artifacts/domain-target/debug/examples/fixture';
const profilePath = path.resolve(root, '../../proofs/delivery/profile.json');
const recipe = 'delivery-small-public-test-v1';
const relations = [1, 2, 3, 4];
const sourceCommit = process.env.VIBEFACE_DELIVERY_SOURCE_COMMIT;
const deadline = Number(process.env.VIBEFACE_DELIVERY_DEADLINE_UNIX_MS);
const result = { schemaVersion: 1, sourceCommit: /^[0-9a-f]{40}$/.test(sourceCommit ?? '') ? sourceCommit : null,
  workItem, status: 'RUNNING', stage: 'initialize', guest: null,
  file: null, contextSha256: null, executionControls: [], legacyImagePreTermsRejected: false,
  wrongJournals: [], reused: false, replayChecks: 0, proofs: [], branches: [], transactions: [], processes: [],
  limits: { memoryBytes: 11 * 1024 ** 3, swapBytes: 0, pids: 64 },
  resourceScope: 'CPU deltas cover each entire sandbox interval; memory.peak is cumulative sandbox lifetime, not per-process RSS',
  coverage: 'synthetic raw 4096-byte file, deterministic public test keys and controlled Ganache branches; no production confidentiality, product Node, UI, mobile, real asset, or public-chain finality',
  deployment: null, corruptERejectedWithoutOutput: false, failure: null };
function progress(stage) {
  result.stage = stage;
  fs.writeFileSync(path.join(publicDir, 'summary.json'), encode(result), { mode: 0o600 });
  console.log(`delivery-small ${workItem} ${stage}`);
}
let compiled, profile, profileSha256, cgroup;
let active, deploymentProcess, deploymentExit, provider, interruption;
function remaining() {
  if (interruption) throw interruption;
  assert(Number.isSafeInteger(deadline), 'work-item absolute deadline required');
  if (deadline <= Date.now()) throw Object.assign(new Error('work-item absolute deadline reached'), { name: 'WorkItemDeadline' });
  return deadline - Date.now();
}
function terminateChild(child, signal = 'SIGTERM') {
  if (!child?.pid) return;
  try { process.kill(-child.pid, signal); }
  catch (error) { if (error.code !== 'ESRCH') throw error; }
}
function interrupt(kind) {
  interruption ??= Object.assign(new Error(kind), { name: kind });
  terminateChild(active);
  terminateChild(deploymentProcess);
  if (provider) void provider.destroy();
}
const terminate = () => interrupt('WorkItemInterrupted');
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, terminate);
let deadlineTimer;
const counters = () => Object.fromEntries(['cpu.stat', 'memory.current', 'memory.peak', 'memory.events', 'pids.current']
  .filter(name => fs.existsSync(path.join(cgroup, name))).map(name => [name, fs.readFileSync(path.join(cgroup, name), 'utf8').trim()]));
const expectedErrors = {
  'decoder-eof': 'Error: io error: unexpected end of file',
  'journal-mismatch': 'Error: receipt journal mismatch',
  'corrupt-E': 'Error: Error("complete capsule hash or length")',
  preterms: 'Error: statement suite, order id, or PreTerms hash',
};
async function invoke(label, program, args, input, output, expected = 'success') {
  assert(expected === 'success' || Object.hasOwn(expectedErrors, expected));
  remaining(); progress(label);
  const stdout = output ?? path.join(privateDir, `${label}.stdout`);
  const stderr = path.join(privateDir, `${label}.stderr`);
  const fds = [input ? fs.openSync(input, 'r') : 'ignore', fs.openSync(stdout, 'wx', 0o600), fs.openSync(stderr, 'wx', 0o600)];
  const before = counters(), start = performance.now();
  let code;
  try {
    active = spawn(program, args, { cwd: path.resolve(root, '../..'), stdio: fds, detached: true });
    code = await new Promise((resolve, reject) => { active.once('error', reject); active.once('close', resolve); });
  } finally {
    active = undefined;
    for (const fd of fds) if (typeof fd === 'number') fs.closeSync(fd);
    const after = counters();
    const usage = value => Number(/^usage_usec ([0-9]+)$/m.exec(value['cpu.stat'])[1]);
    const errors = fs.readFileSync(stderr);
    const markers = { memory: /out of memory|memory allocation|AllocError/i, processLimit: /Resource temporarily unavailable|pthread_create|ThreadPoolBuildError/i,
      cycles: /session limit|cycle limit/i, panic: /panicked at/i, compression: /confined Groth16/i };
    const checkpointIndex = args.indexOf('--succinct-receipt');
    const checkpoint = checkpointIndex < 0 ? null : args[checkpointIndex + 1];
    result.processes.push({ label, exitCode: code ?? null, elapsedMs: performance.now() - start,
      sandboxCpuUsec: usage(after) - usage(before), before, after, stderrSha256: digest(errors),
      diagnosticMarkers: Object.keys(markers).filter(name => markers[name].test(errors.toString('utf8'))),
      succinctCheckpointBytes: checkpoint && fs.existsSync(checkpoint) ? fs.statSync(checkpoint).size : 0 });
    fs.writeFileSync(path.join(publicDir, 'summary.json'), encode(result), { mode: 0o600 });
  }
  remaining();
  assert(expected === 'success' ? code === 0 : Number.isInteger(code) && code > 0, `${label} exit ${code}; private logs retained`);
  if (expected !== 'success') assert.equal(fs.readFileSync(stderr, 'utf8').trim(), expectedErrors[expected], `${label} rejected for the intended reason`);
  return stdout;
}
const { JsonRpcProvider, Contract, AbiCoder, TypedDataEncoder, getBytes, sha256 } = requireDependency('ethers');
const rpc = async (method, params = []) => { remaining(); const value = await provider.send(method, params); remaining(); return value; };
const latest = () => rpc('eth_getBlockByNumber', ['latest', false]);
async function stopDeployment() {
  const connected = provider, child = deploymentProcess, exited = deploymentExit;
  provider = undefined; deploymentProcess = undefined; deploymentExit = undefined;
  try {
    if (connected) await connected.destroy();
  } finally {
    if (child) {
      if (child.exitCode === null && child.signalCode === null) terminateChild(child);
      const outcome = await Promise.race([exited, delay(5000, null, { ref: false })]);
      if (!outcome) {
        terminateChild(child, 'SIGKILL');
        await exited;
        throw Object.assign(new Error('owned deployment required forced termination'), { name: 'DeploymentShutdown' });
      }
      assert(!outcome.error && outcome.code === 0 && outcome.signal === null, 'owned deployment failed to stop cleanly');
    }
  }
}
async function startDeployment(label) {
  remaining(); progress(label);
  assert(!deploymentProcess && !provider);
  const deploymentFile = path.join(privateDir, `${label}.json`);
  const log = fs.openSync(path.join(privateDir, `${label}.log`), 'wx', 0o600);
  deploymentProcess = spawn(process.execPath, [path.join(root, 'deploy-local.mjs'), '--experiment-fixture', '--output', deploymentFile],
    { stdio: ['ignore', log, log], detached: true });
  fs.closeSync(log);
  const child = deploymentProcess;
  deploymentExit = new Promise(resolve => {
    child.once('error', error => resolve({ error }));
    child.once('close', (code, signal) => resolve({ code, signal }));
  });
  let deployment;
  while (!deployment) {
    remaining();
    assert(child.exitCode === null && child.signalCode === null, 'owned deployment exited before readiness');
    if (fs.existsSync(deploymentFile) && fs.statSync(deploymentFile).size) {
      try { deployment = readJson(deploymentFile); } catch (error) { if (!(error instanceof SyntaxError)) throw error; }
    }
    if (!deployment) await delay(50);
  }
  provider = new JsonRpcProvider(deployment.rpc_url, undefined, { cacheTimeout: -1 });
  assert.equal((await provider.getNetwork()).chainId, 31337n);
  assert.deepEqual(deployment.profile, profile);
  assert.equal(deployment.experiment_fixture?.recipe, recipe);
  const pool = await rpc('txpool_content');
  assert.deepEqual(pool.pending, {}); assert.deepEqual(pool.queued, {});
  return deployment;
}
async function projectDeployment(deployment) {
  const marker = deployment.experiment_fixture;
  const accounts = (await rpc('eth_accounts')).map(address => address.toLowerCase());
  assert.deepEqual(accounts, marker.accounts);
  assert.equal(accounts[0], deployment.funder);
  const block = await latest();
  const baselineBlock = { number: Number(BigInt(block.number)), hash: block.hash, timestamp: Number(BigInt(block.timestamp)) };
  assert.deepEqual(baselineBlock, deployment.baseline_block);
  const token = new Contract(deployment.asset, artifact('TestToken').abi, provider);
  assert.equal(await token.mode(), 0n); assert.equal(await token.decimals(), 6n);
  assert.equal(await new Contract(deployment.escrow, artifact('DeliveryEscrow').abi, provider).profileId(), deployment.profile_id);
  const nativeWei = {}, tokenAtoms = {};
  for (const [role, address] of Object.entries({ payer: accounts[0], provider: accounts[1], escrow: deployment.escrow })) {
    nativeWei[role] = BigInt(await rpc('eth_getBalance', [address, block.number])).toString();
    tokenAtoms[role] = (await token.balanceOf(address, { blockTag: baselineBlock.number })).toString();
  }
  const transaction = (value, nonce) => ({ hash: value.hash, blockNumber: value.block_number, blockHash: value.block_hash, gasUsed: value.gas_used, nonce });
  const projection = { chainId: deployment.chain_id, confirmations: deployment.confirmations, accounts,
    escrow: deployment.escrow, asset: deployment.asset, adapter: deployment.adapter, verifier: deployment.verifier,
    payer: accounts[0], provider: accounts[1], tokenDecimals: 6, funderNonce: Number(BigInt(await rpc('eth_getTransactionCount', [accounts[0], 'latest']))),
    mining: { genesisTimestamp: marker.genesis_block.timestamp, timestampIncrement: marker.timestamp_increment,
      blockTime: marker.block_time, gasPriceWei: marker.gas_price_wei, coinbase: marker.coinbase },
    genesisBlock: marker.genesis_block, runtimeCodeHashes: deployment.runtime_code_hashes, baselineBlock,
    baselineBalances: { nativeWei, tokenAtoms },
    deploymentTransactions: { asset: transaction(deployment.deployment_transactions.asset, marker.deployment_nonces.asset),
      escrow: transaction(deployment.deployment_transactions.escrow, marker.deployment_nonces.escrow) } };
  assert.equal(projection.chainId, 31337); assert.equal(projection.confirmations, 2); assert.equal(projection.funderNonce, 2);
  assert.deepEqual(projection.mining, { genesisTimestamp: 1700000000, timestampIncrement: 1, blockTime: 0, gasPriceWei: '2000000000',
    coinbase: '0x0000000000000000000000000000000000000000' });
  assert.equal(baselineBlock.number, 4); assert.equal(baselineBlock.timestamp, 1700000004);
  assert.deepEqual(tokenAtoms, { payer: '1000000000000', provider: '0', escrow: '0' });
  return projection;
}
const original = Buffer.from(Array.from({ length: 4096 }, (_, i) => (i * 37 + 19) % 251));
const originalFile = path.join(privateDir, 'original.bin');
let fields, coder, statementType;
function changedJournal(journal, field) {
  const bytes = Buffer.from(journal, 'hex'), wordIndex = fields.findIndex(item => item.name === field);
  assert(wordIndex >= 0 && bytes.length === compiled.statementBytes && bytes.length === fields.length * 32);
  bytes[wordIndex * 32] ^= 1;
  const changed = bytes.toString('hex'); assert.notEqual(changed, journal);
  return changed;
}
async function rebuild(projection, destination, label) {
  const contextFile = path.join(path.dirname(destination), 'fixture-context.json');
  write(contextFile, { escrow: [...getBytes(projection.escrow)], asset: [...getBytes(projection.asset)],
    payer: [...getBytes(projection.payer)], provider_wallet: [...getBytes(projection.provider)], base_timestamp: projection.baselineBlock.timestamp });
  await invoke(label, fixture, ['experiment', contextFile, originalFile, destination, profilePath]);
  const vectors = readJson(path.join(destination, 'vectors.json'));
  assert.deepEqual(vectors.profile, profile); assert.equal(vectors.schemaHash, compiled.schemaHash);
  assert.equal(vectors.windows.length, 1);
  const expectedJournals = [{ relation: 1, journal: vectors.keyJournal, journalHash: digest(Buffer.from(vectors.keyJournal, 'hex')) },
    ...vectors.windows[0].statements.map(value => ({ relation: value.relation, journal: value.journal, journalHash: value.journalHash }))];
  assert.deepEqual(expectedJournals.map(value => value.relation), relations);
  for (const value of expectedJournals) {
    assert.match(value.journal, /^[0-9a-f]+$/); assert.equal(value.journal.length, compiled.statementBytes * 2);
    assert.equal(value.journalHash, digest(Buffer.from(value.journal, 'hex')));
    const decoded = coder.decode([statementType], `0x${value.journal}`)[0];
    assert.equal(decoded.relation, BigInt(value.relation));
    assert.equal(decoded.profileId, `0x${vectors.profileId}`); assert.equal(decoded.guestImageId, `0x${compiled.imageId}`);
  }
  return { vectors, context: { schemaVersion: 1, sourceCommit, recipe, guest: compiled, profileId: vectors.profileId,
    profileSha256, file: result.file, deployment: projection, orderId: vectors.orderId, expectedJournals } };
}
async function controls(context) {
  const empty = path.join(privateDir, 'empty-receipt'); fs.writeFileSync(empty, '', { flag: 'wx', mode: 0o600 });
  result.wrongJournals = context.expectedJournals.map(value => ({ relation: value.relation,
    journal: changedJournal(value.journal, value.relation === 1 ? 'guestImageId' : 'ciphertextHash') }));
  for (const value of context.expectedJournals) {
    const relation = value.relation;
    const execution = readJson(await invoke(`execute-${relation}`, prover, ['execute'], path.join(material, `relation-${relation}.private.json`)));
    assert.deepEqual(Object.keys(execution).sort(), ['mode', 'imageId', 'profileId', 'relation', 'journalHash', 'cycles', 'segments', 'proved'].sort());
    assert.equal(execution.mode, 'execute'); assert.equal(execution.imageId, compiled.imageId); assert.equal(execution.profileId, context.profileId);
    assert.equal(execution.relation, relation); assert.equal(execution.journalHash, value.journalHash); assert.equal(execution.proved, false);
    assert(Number.isSafeInteger(execution.cycles) && execution.cycles > 0 && execution.cycles <= profile.max_guest_cycles);
    assert(Number.isSafeInteger(execution.segments) && execution.segments > 0);
    result.executionControls.push({ relation, cycles: execution.cycles, segments: execution.segments, journalHash: value.journalHash,
      executed: true, proved: false, originalJournalDecoderEof: false, wrongJournalDecoderEof: false });
  }
  for (const value of context.expectedJournals) {
    const control = result.executionControls[value.relation - 1];
    await invoke(`parser-original-${value.relation}`, prover, ['verify', value.journal], empty, undefined, 'decoder-eof');
    control.originalJournalDecoderEof = true;
    await invoke(`parser-wrong-${value.relation}`, prover, ['verify', result.wrongJournals[value.relation - 1].journal], empty, undefined, 'decoder-eof');
    control.wrongJournalDecoderEof = true;
  }
  await invoke('legacy-image-preterms', prover, ['verify', changedJournal(context.expectedJournals[1].journal, 'guestImageId')], empty, undefined, 'preterms');
  result.legacyImagePreTermsRejected = true;
  progress('controls-complete');
}
async function verifyPair(context, relation, imported) {
  const receipt = path.join(proofDir, `relation-${relation}.receipt`), publicFile = path.join(proofDir, `relation-${relation}.json`);
  const proof = readJson(publicFile, 32 * 1024), expected = context.expectedJournals[relation - 1];
  assert.deepEqual(Object.keys(proof).sort(), ['mode', 'kind', 'imageId', 'profileId', 'relation', 'journal', 'journalHash', 'seal'].sort());
  assert.equal(proof.mode, 'prove'); assert.equal(proof.kind, 'groth16'); assert.equal(proof.relation, relation);
  assert.equal(proof.imageId, compiled.imageId); assert.equal(proof.profileId, context.profileId);
  assert.equal(proof.journal, expected.journal); assert.equal(proof.journalHash, expected.journalHash);
  assert.match(proof.seal, /^[0-9a-f]+$/); assert.equal(proof.seal.length % 2, 0);
  const receiptBytes = read(receipt, 16 * 1024 * 1024).length;
  const verified = readJson(await invoke(`verify-${relation}`, prover, ['verify', expected.journal], receipt));
  assert.deepEqual(verified, { verified: true, kind: 'groth16', seal: proof.seal, imageId: compiled.imageId, journalHash: expected.journalHash });
  await invoke(`wrong-journal-${relation}`, prover, ['verify', result.wrongJournals[relation - 1].journal], receipt, undefined, 'journal-mismatch');
  const control = result.executionControls[relation - 1];
  result.proofs.push({ relation, cycles: control.cycles, segments: control.segments, receiptBytes,
    sealBytes: getBytes(`0x${proof.seal}`).length, journalHash: proof.journalHash, independentlyVerified: true, wrongJournalRejected: true, reused: imported });
  return proof;
}
async function run() {
  assert.match(sourceCommit ?? '', /^[0-9a-f]{40}$/, 'the sandbox launcher must bind the exact source commit');
  remaining(); deadlineTimer = setTimeout(() => interrupt('WorkItemDeadline'), Math.min(remaining(), 2 ** 31 - 1));
  const membership = fs.readFileSync('/proc/self/cgroup', 'utf8').split('\n').find(line => line.startsWith('0::'));
  assert(membership, 'unified sandbox cgroup required'); cgroup = path.join('/sys/fs/cgroup', membership.slice(3));
  compiled = readJson(path.resolve(root, '../../.artifacts/vdp/host-info.json'));
  assert.deepEqual(Object.keys(compiled).sort(), ['imageId', 'schemaHash', 'elfSha256', 'statementBytes'].sort());
  for (const field of ['imageId', 'schemaHash', 'elfSha256']) assert.match(compiled[field], /^[0-9a-f]{64}$/);
  assert.equal(compiled.statementBytes, 1504); result.guest = compiled;
  const profileBytes = read(profilePath, 16384); profile = JSON.parse(profileBytes); profileSha256 = digest(profileBytes);
  for (const [field, profileKey] of [['imageId', 'guest_image_id'], ['schemaHash', 'statement_schema_hash']])
    assert.equal(Buffer.from(profile[profileKey]).toString('hex'), compiled[field]);
  fields = artifact('DeliveryEscrow').abi.find(item => item.name === 'preview').inputs[0].components;
  statementType = `tuple(${fields.map(item => `${item.type} ${item.name}`).join(',')})`; coder = AbiCoder.defaultAbiCoder();
  result.file = { bytes: original.length, sha256: digest(original) };
  fs.writeFileSync(originalFile, original, { flag: 'wx', mode: 0o600 });
  let inputBytes, expectedContext;
  if (inputDirectory) {
    assert(fs.lstatSync(inputDirectory).isDirectory(), 'normalized input directory required');
    inputBytes = read(path.join(inputDirectory, 'context.json'), 64 * 1024); expectedContext = JSON.parse(inputBytes);
    assert.equal(expectedContext.sourceCommit, sourceCommit, 'source drift refuses automatic proof reuse');
    assert.equal(expectedContext.recipe, recipe); assert.deepEqual(expectedContext.guest, compiled);
    assert.equal(expectedContext.profileSha256, profileSha256); assert.deepEqual(expectedContext.file, result.file);
  } else assert.equal(workItem, 'prepare', 'this work item requires a normalized public context');
  let built;
  if (workItem === 'prepare') {
    const first = await startDeployment('deploy-prepare-1');
    built = await rebuild(await projectDeployment(first), material, 'fixture');
    assert.equal(built.context.profileId, first.profile_id.slice(2)); result.replayChecks++;
    await stopDeployment();
    const replayDir = path.join(privateDir, 'replay-2'); fs.mkdirSync(replayDir, { mode: 0o700 });
    const second = await startDeployment('deploy-prepare-2');
    const replay = await rebuild(await projectDeployment(second), path.join(replayDir, 'material'), 'fixture-replay-2');
    assert.equal(replay.context.profileId, second.profile_id.slice(2));
    assert.equal(encode(replay.context), encode(built.context), 'fresh deployment identity and all four journals must replay exactly');
    result.replayChecks++; await stopDeployment();
  } else built = await rebuild(expectedContext.deployment, material, 'fixture');
  const context = built.context, vectors = built.vectors, contextBytes = Buffer.from(encode(context));
  if (expectedContext) assert.deepEqual(contextBytes, inputBytes, 'reconstructed context and all four journals must match exact normalized input bytes');
  result.contextSha256 = digest(contextBytes);
  await controls(context);
  fs.writeFileSync(path.join(publicDir, 'context.json'), contextBytes, { flag: 'wx', mode: 0o600 });
  if (workItem === 'prepare') { result.status = 'PREPARED'; progress('complete'); return; }
  const selected = workItem === 'accept' ? relations : [Number(workItem.slice('relation-'.length))];
  const proofs = [];
  for (const relation of selected) {
    const source = workItem === 'accept' ? path.join(inputDirectory, 'proofs') : path.join(inputDirectory, 'partials', `relation-${relation}`);
    const imported = workItem === 'accept' || fs.existsSync(source);
    if (imported) {
      assert(fs.lstatSync(source).isDirectory(), 'normalized proof directory required');
      if (workItem !== 'accept') assert.deepEqual(fs.readdirSync(source).sort(), [`relation-${relation}.json`, `relation-${relation}.receipt`].sort());
      for (const [suffix, limit] of [['json', 32 * 1024], ['receipt', 16 * 1024 * 1024]])
        fs.writeFileSync(path.join(proofDir, `relation-${relation}.${suffix}`), read(path.join(source, `relation-${relation}.${suffix}`), limit), { flag: 'wx', mode: 0o600 });
      result.reused = true;
    } else await invoke(`prove-${relation}`, prover, ['prove', path.join(proofDir, `relation-${relation}.receipt`), '--succinct-receipt',
      path.join(privateDir, `relation-${relation}.succinct.receipt`)], path.join(material, `relation-${relation}.private.json`), path.join(proofDir, `relation-${relation}.json`));
    proofs.push(await verifyPair(context, relation, imported));
  }
  if (workItem !== 'accept') { assert.equal(result.proofs.length, 1); result.status = 'PROOF_VERIFIED'; progress('complete'); return; }
  const manifest = { schemaVersion: 1, imageId: compiled.imageId, schemaHash: compiled.schemaHash, elfSha256: compiled.elfSha256,
    proofs: relations.map(relation => ({ receipt: `relation-${relation}.receipt`, public: `relation-${relation}.json` })) };
  const inputProofs = path.join(inputDirectory, 'proofs');
  assert.deepEqual(fs.readdirSync(inputProofs).sort(), ['manifest.json', ...relations.flatMap(relation => [`relation-${relation}.json`, `relation-${relation}.receipt`])].sort());
  assert.deepEqual(readJson(path.join(inputProofs, 'manifest.json'), 32 * 1024), manifest);
  write(path.join(proofDir, 'manifest.json'), manifest);
  const deployment = await startDeployment('deploy-accept');
  const replayDir = path.join(privateDir, 'accept-replay'); fs.mkdirSync(replayDir, { mode: 0o700 });
  const replay = await rebuild(await projectDeployment(deployment), path.join(replayDir, 'material'), 'fixture-accept-replay');
  assert.equal(replay.context.profileId, deployment.profile_id.slice(2));
  assert.equal(encode(replay.context), encode(context), 'accept must genuinely replay deployment and all journals before financial writes');
  result.replayChecks++;
  await invoke('adapter', process.execPath, [path.join(root, 'verify-delivery.mjs'), proofDir]);
  const accounts = await rpc('eth_accounts'), payer = accounts[0], relay = accounts[1];
  let clock = Number(BigInt((await latest()).timestamp));
  assert.equal(payer.toLowerCase(), deployment.funder);
  const escrow = new Contract(deployment.escrow, artifact('DeliveryEscrow').abi, provider);
  const token = new Contract(deployment.asset, artifact('TestToken').abi, provider);
  progress('fund-and-hold');
  const decode = proof => {
    const decoded = coder.decode([statementType], `0x${proof.journal}`)[0];
    return Object.fromEntries(fields.map(item => [item.name, decoded[item.name]]));
  };
  const key = decode(proofs[0]), source = decode(proofs[1]);
  const spec = { index: source.windowIndex, offset: source.windowOffset, plaintextBytes: source.windowPlaintextBytes,
    ciphertextBytes: source.windowCiphertextBytes, ciphertextHash: source.ciphertextHash, capsuleHash: source.capsuleHash,
    plaintextCommitment: source.plaintextCommitment, deliveryFee: source.windowDeliveryFee, reservationFee: source.windowReservationFee };
  assert.equal(source.totalPlaintextBytes, 4096n); assert.equal(source.windowCount, 1n); assert.equal(source.acceptance, 0n);
  assert.equal(source.windowCiphertextBytes, 4164n); assert.equal(source.escrow.toLowerCase(), deployment.escrow);
  const canonical = value => coder.encode([statementType], [value]);
  assert.equal(canonical(await escrow.previewBinding(key)), `0x${proofs[0].journal}`);
  assert.equal(canonical((await escrow.preview(source, [spec]))[0]), `0x${proofs[1].journal}`);
  const id = source.orderId, capsule = `0x${vectors.windows[0].capsule}`;
  async function mine(timestamp = clock + 1) {
    assert(Number.isSafeInteger(timestamp) && timestamp > clock);
    await rpc('evm_mine', [timestamp]); clock = Number(BigInt((await latest()).timestamp)); assert.equal(clock, timestamp);
  }
  const call = (contract, method, args, from = relay) => ({ from, to: contract.target, data: contract.interface.encodeFunctionData(method, args), gas: '0xb71b00' });
  function record(label, receipt) {
    const events = receipt.logs.filter(log => log.address.toLowerCase() === deployment.escrow)
      .map(log => escrow.interface.parseLog(log)).filter(Boolean).map(event => ({ name: event.name, args: Array.from(event.args) }));
    const value = { label, hash: receipt.transactionHash, blockHash: receipt.blockHash, blockNumber: Number(BigInt(receipt.blockNumber)),
      status: Number(BigInt(receipt.status)), gasUsed: BigInt(receipt.gasUsed), effectiveGasPrice: BigInt(receipt.effectiveGasPrice),
      nativeCostWei: BigInt(receipt.gasUsed) * BigInt(receipt.effectiveGasPrice), events };
    result.transactions.push(value); return value;
  }
  async function send(label, contract, method, args, from = relay) {
    const hash = await rpc('eth_sendTransaction', [call(contract, method, args, from)]);
    await mine(); const receipt = await rpc('eth_getTransactionReceipt', [hash]);
    assert(receipt); assert.equal(receipt.status, '0x1', label); record(label, receipt); return receipt;
  }
  await send('register', escrow, 'registerKey', [key, `0x${proofs[0].seal}`]);
  await send('approve', token, 'approve', [deployment.escrow, 17n], payer);
  const quote = TypedDataEncoder.getPayload({ name: 'VibefaceDelivery', version: '1', chainId: 31337, verifyingContract: deployment.escrow },
    { DeliveryQuote: [{ name: 'quoteDigest', type: 'bytes32' }, { name: 'providerPeer', type: 'bytes32' }] },
    { quoteDigest: source.quoteDigest, providerPeer: source.providerPeer });
  const signature = await rpc('eth_signTypedData_v4', [relay, quote]);
  const balance = async () => ({ payer: await token.balanceOf(payer), provider: await token.balanceOf(relay), escrow: await token.balanceOf(deployment.escrow) });
  const beforeFund = await balance();
  const funded = await send('fund', escrow, 'fund', [source, [spec], [`0x${proofs[1].seal}`], signature], payer);
  assert(result.transactions.at(-1).events.some(event => event.name === 'Funded' && event.args[2] === 17n));
  await send('hold', escrow, 'hold', [id, 0, `0x${proofs[2].seal}`]);
  const heldBalances = await balance();
  assert.deepEqual(heldBalances, { payer: beforeFund.payer - 17n, provider: beforeFund.provider, escrow: beforeFund.escrow + 17n });
  async function state() {
    const order = await escrow.getOrder(id), window = await escrow.getWindow(id, 0), block = await latest();
    return { block: { number: Number(BigInt(block.number)), hash: block.hash, timestamp: Number(BigInt(block.timestamp)) },
      earned: order.earned, withdrawn: order.withdrawn, refunded: order.refunded, phase: window.phase, decryptionProven: window.decryptionProven,
      capsuleBytes: getBytes(window.capsule).length, token: await balance(),
      nativeWei: { payer: await rpc('eth_getBalance', [payer, 'latest']), provider: await rpc('eth_getBalance', [relay, 'latest']) } };
  }
  const held = await state();
  assert.equal(held.phase, 1n); assert.equal(held.capsuleBytes, 0);
  for (const name of ['earned', 'withdrawn', 'refunded']) assert.equal(held[name], 0n);
  for (const relation of [2, 3, 4]) assert.equal(canonical(await escrow.publicValues(id, 0, relation)), `0x${proofs[relation - 1].journal}`);
  let checkpoint = await rpc('evm_snapshot');
  async function restore() {
    assert.equal(await rpc('evm_revert', [checkpoint]), true);
    assert.deepEqual(await state(), held, 'Held state and actual balances restored');
    const pool = await rpc('txpool_content'); assert.deepEqual(pool.pending, {}); assert.deepEqual(pool.queued, {});
    clock = held.block.timestamp; checkpoint = await rpc('evm_snapshot');
  }
  async function observe(hash, pending) {
    const transaction = await rpc('eth_getTransactionByHash', [hash]); assert(transaction);
    if (pending) { assert.equal(transaction.blockHash, null); assert.equal(await rpc('eth_getTransactionReceipt', [hash]), null); }
    const parsed = escrow.interface.parseTransaction({ data: transaction.input, value: transaction.value });
    assert.equal(parsed.name, 'publish'); assert.equal(parsed.args[0], id); assert.equal(parsed.args[1], 0n);
    const observed = getBytes(parsed.args[2]); assert.equal(observed.length, 80); assert.equal(sha256(observed), source.capsuleHash);
    return { observed: Buffer.from(observed), source: { transactionHash: hash, wasPending: pending, capsuleSha256: digest(observed) } };
  }
  async function recovery(branch, observation) {
    const destination = path.join(privateDir, `recover-${branch}`); fs.mkdirSync(destination, { mode: 0o700 });
    const holdFile = path.join(destination, 'hold.json'), eFile = path.join(destination, 'observed-E.bin'), recovered = path.join(destination, 'recovered.bin');
    fs.copyFileSync(path.join(material, 'relation-3.private.json'), holdFile, fs.constants.COPYFILE_EXCL);
    fs.writeFileSync(eFile, observation.observed, { flag: 'wx', mode: 0o600 });
    await invoke(`recover-${branch}`, fixture, ['recover', holdFile, eFile, recovered]);
    const bytes = fs.readFileSync(recovered); assert.deepEqual(bytes, original);
    return { ...observation.source, bytes: bytes.length, sha256: digest(bytes), inputs: ['Hold witness', 'observed transaction E'],
      claim: 'separate offline recovery program receives only two input paths; no OS access or network sandbox claimed' };
  }
  progress('normal');
  const published = await send('normal-publish', escrow, 'publish', [id, 0, capsule]);
  const ordinaryE = await observe(published.transactionHash, false);
  const earned = await state(); assert.equal(earned.earned, 17n); assert.equal(earned.withdrawn, 0n);
  await send('normal-open', escrow, 'open', [id, 0, `0x${proofs[3].seal}`]);
  const opened = await state(); assert.equal(opened.earned, 17n); assert.equal(opened.decryptionProven, true);
  await send('normal-withdraw', escrow, 'withdraw', [id]);
  const normal = await state();
  assert.equal(normal.withdrawn, 17n); assert.equal(normal.refunded, 0n); assert.equal(normal.phase, 3n);
  assert.deepEqual(normal.token, { payer: heldBalances.payer, provider: heldBalances.provider + 17n, escrow: heldBalances.escrow - 17n });
  result.branches.push({ name: 'normal', held, final: normal, recovery: await recovery('normal', ordinaryE), laterRestoredByExperiment: true });
  await restore();
  progress('pending-late');
  await escrow.publish.staticCall(id, 0, capsule);
  const pendingCall = call(escrow, 'publish', [id, 0, capsule]);
  const pendingHash = await rpc('eth_sendTransaction', [pendingCall]);
  const pendingE = await observe(pendingHash, true);
  await mine(Number(source.keyDeadline) + 1);
  const failed = await rpc('eth_getTransactionReceipt', [pendingHash]); assert.equal(failed.status, '0x0');
  assert(BigInt(failed.gasUsed) < BigInt(pendingCall.gas));
  await assert.rejects(escrow.publish.staticCall(id, 0, capsule, { blockTag: Number(BigInt(failed.blockNumber)) }),
    error => error.code === 'CALL_EXCEPTION' && escrow.interface.parseError(error.data)?.name === 'Deadline');
  record('pending-late-publish', failed);
  const rejected = await state(); assert.equal(rejected.phase, 1n); assert.equal(rejected.capsuleBytes, 0);
  assert.equal(rejected.earned, 0n); assert.equal(rejected.withdrawn, 0n); assert.equal(rejected.refunded, 0n); assert.deepEqual(rejected.token, heldBalances);
  async function refund(label) {
    await send(label, escrow, 'refund', [id, 0]);
    const final = await state(); assert.equal(final.phase, 4n); assert.equal(final.refunded, 17n); assert.equal(final.earned, 0n); assert.equal(final.withdrawn, 0n);
    assert.deepEqual(final.token, { payer: heldBalances.payer + 17n, provider: heldBalances.provider, escrow: heldBalances.escrow - 17n });
    assert(result.transactions.at(-1).events.some(event => event.name === 'Refunded' && event.args[2] === 17n));
    return final;
  }
  result.branches.push({ name: 'pending-late', held, rejected, revert: 'Deadline()', final: await refund('pending-refund'),
    recovery: await recovery('pending', pendingE), laterRestoredByExperiment: true });
  await restore();
  progress('reorg');
  const removed = await send('reorg-publish', escrow, 'publish', [id, 0, capsule]);
  const reorgE = await observe(removed.transactionHash, false);
  assert.equal((await state()).earned, 17n);
  await restore();
  assert.equal(await rpc('eth_getTransactionReceipt', [removed.transactionHash]), null);
  await mine(Number(source.keyDeadline) + 1);
  const canonicalBlock = await rpc('eth_getBlockByNumber', [removed.blockNumber, false]);
  assert(canonicalBlock && canonicalBlock.hash !== removed.blockHash, 'removed publish block is not canonical');
  result.branches.push({ name: 'controlled-reorg', held, removedTransaction: removed.transactionHash, removedBlock: removed.blockHash,
    newCanonicalBlock: canonicalBlock.hash, final: await refund('reorg-refund'), recovery: await recovery('reorg', reorgE), absoluteFinality: false });
  const invalidE = path.join(privateDir, 'invalid-E.bin'), invalidOutput = path.join(privateDir, 'invalid-recovery.bin');
  fs.writeFileSync(invalidE, pendingE.observed.subarray(0, 79), { flag: 'wx', mode: 0o600 });
  await invoke('reject-corrupt-E', fixture, ['recover', path.join(material, 'relation-3.private.json'), invalidE, invalidOutput], undefined, undefined, 'corrupt-E');
  assert(!fs.existsSync(invalidOutput));
  result.corruptERejectedWithoutOutput = true;
  result.deployment = { chainId: 31337, escrow: deployment.escrow, asset: deployment.asset, payer, provider: relay,
    profileId: deployment.profile_id, adapter: deployment.adapter, verifier: deployment.verifier, tokenDecimals: 6,
    orderId: id, fundTransaction: funded.transactionHash, runtimeCodeHashes: deployment.runtime_code_hashes };
  assert.equal(result.proofs.length, 4); assert.equal(result.branches.length, 3);
  result.status = 'PASS-accept-candidate'; progress('complete');
}
try {
  await run();
} catch (error) {
  result.status = 'FAIL'; result.failure = { stage: result.stage,
    kind: /^[A-Za-z][A-Za-z0-9_]{0,63}$/.test(error.name ?? '') ? error.name : 'Error',
    code: /^[A-Z][A-Z0-9_]{0,63}$/.test(error.code ?? '') ? error.code : null };
  fs.writeFileSync(path.join(privateDir, 'failure.log'), String(error.stack ?? error), { mode: 0o600 });
  progress(result.stage); process.exitCode = 1;
} finally {
  terminateChild(active);
  try {
    await stopDeployment();
    if (result.status !== 'FAIL') remaining();
  }
  catch (error) {
    result.status = 'FAIL'; result.failure = { stage: 'cleanup', kind: error.name === 'WorkItemDeadline' || error.name === 'WorkItemInterrupted' ? error.name : 'DeploymentShutdown', code: null };
    fs.writeFileSync(path.join(privateDir, 'cleanup-failure.log'), String(error.stack ?? error), { mode: 0o600 });
    progress('cleanup'); process.exitCode = 1;
  }
  clearTimeout(deadlineTimer);
  for (const signal of ['SIGINT', 'SIGTERM']) process.removeListener(signal, terminate);
}
