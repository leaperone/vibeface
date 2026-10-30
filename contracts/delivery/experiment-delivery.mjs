import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { artifact, requireDependency, root } from './compile.mjs';

assert.equal(process.argv.length, 3, 'usage: experiment-delivery.mjs <new-absolute-run-directory>');
const directory = process.argv[2];
assert(path.isAbsolute(directory));
fs.mkdirSync(directory, { mode: 0o700 });
const privateDir = path.join(directory, 'private'), publicDir = path.join(directory, 'public');
const proofDir = path.join(privateDir, 'proofs');
for (const name of [privateDir, publicDir, proofDir]) fs.mkdirSync(name, { mode: 0o700 });
const encode = value => `${JSON.stringify(value, (_, item) => typeof item === 'bigint' ? item.toString() : item, 2)}\n`;
const write = (file, value) => fs.writeFileSync(file, encode(value), { flag: 'wx', mode: 0o600 });
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const readJson = file => JSON.parse(fs.readFileSync(file, 'utf8'));
const compiled = readJson(path.resolve(root, '../../.artifacts/vdp/host-info.json'));
const prover = '/artifacts/proof-target/release/vibeface-delivery-prover';
const fixture = '/artifacts/domain-target/debug/examples/fixture';
const profilePath = path.resolve(root, '../../proofs/delivery/profile.json');
const membership = fs.readFileSync('/proc/self/cgroup', 'utf8').split('\n').find(line => line.startsWith('0::')).slice(3);
const cgroup = path.join('/sys/fs/cgroup', membership);
const counters = () => Object.fromEntries(['cpu.stat', 'memory.current', 'memory.peak', 'memory.events', 'pids.current']
  .filter(name => fs.existsSync(path.join(cgroup, name))).map(name => [name, fs.readFileSync(path.join(cgroup, name), 'utf8').trim()]));
const sourceCommit = process.env.VIBEFACE_DELIVERY_SOURCE_COMMIT;
assert.match(sourceCommit ?? '', /^[0-9a-f]{40}$/, 'the sandbox launcher must bind the exact source commit');
const result = { schemaVersion: 1, sourceCommit, status: 'RUNNING', stage: 'deploy', guest: compiled,
  file: null, proofs: [], branches: [], transactions: [], processes: [],
  limits: { memoryBytes: 11 * 1024 ** 3, swapBytes: 0, pids: 64 },
  resourceScope: 'CPU deltas cover each entire sandbox interval; memory.peak is cumulative sandbox lifetime, not per-process RSS',
  coverage: 'synthetic raw 4096-byte file, deterministic public test keys and controlled Ganache branches; no production confidentiality, product Node, UI, mobile, real asset, or public-chain finality' };
function progress(stage) {
  result.stage = stage;
  fs.writeFileSync(path.join(publicDir, 'summary.json'), encode(result), { mode: 0o600 });
  console.log(`delivery-small ${stage}`);
}
let active;
async function invoke(label, program, args, input, output, expectedFailure = false) {
  progress(label);
  const stdout = output ?? path.join(privateDir, `${label}.stdout`);
  const stderr = path.join(privateDir, `${label}.stderr`);
  const fds = [input ? fs.openSync(input, 'r') : 'ignore', fs.openSync(stdout, 'wx', 0o600), fs.openSync(stderr, 'wx', 0o600)];
  const before = counters(), start = performance.now();
  let code;
  try {
    active = spawn(program, args, { cwd: path.resolve(root, '../..'), stdio: fds });
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
  assert(expectedFailure ? Number.isInteger(code) && code > 0 : code === 0, `${label} exit ${code}; private logs retained`);
  if (expectedFailure) {
    const reason = label.startsWith('wrong-journal-') ? 'receipt journal mismatch' : 'complete capsule hash or length';
    assert(fs.readFileSync(stderr, 'utf8').includes(reason), `${label} rejected for the intended reason`);
  }
  return stdout;
}
let deploymentProcess, provider;
try {
  progress('deploy');
  const deploymentFile = path.join(privateDir, 'deployment.json');
  const log = fs.openSync(path.join(privateDir, 'deployment.log'), 'wx', 0o600);
  deploymentProcess = spawn(process.execPath, [path.join(root, 'deploy-local.mjs'), '--output', deploymentFile], { stdio: ['ignore', log, log] });
  fs.closeSync(log);
  let deployment;
  while (!deployment) {
    assert(deploymentProcess.exitCode === null && deploymentProcess.signalCode === null, 'owned deployment exited before readiness');
    if (fs.existsSync(deploymentFile) && fs.statSync(deploymentFile).size) {
      try { deployment = readJson(deploymentFile); } catch (error) { if (!(error instanceof SyntaxError)) throw error; }
    }
    if (!deployment) await delay(50);
  }
  const { JsonRpcProvider, Contract, AbiCoder, TypedDataEncoder, getBytes, hexlify, sha256 } = requireDependency('ethers');
  provider = new JsonRpcProvider(deployment.rpc_url, undefined, { cacheTimeout: -1 });
  const rpc = (method, params = []) => provider.send(method, params);
  assert.equal((await provider.getNetwork()).chainId, 31337n);
  assert.equal(await rpc('miner_stop'), true);
  const latest = () => rpc('eth_getBlockByNumber', ['latest', false]);
  let clock = Number(BigInt((await latest()).timestamp));
  const accounts = await rpc('eth_accounts');
  const payer = accounts[0], relay = accounts[1];
  assert.equal(payer.toLowerCase(), deployment.funder);
  const escrow = new Contract(deployment.escrow, artifact('DeliveryEscrow').abi, provider);
  const token = new Contract(deployment.asset, artifact('TestToken').abi, provider);
  assert.equal(await token.mode(), 0n); assert.equal(await token.decimals(), 6n);
  assert.equal(await escrow.profileId(), deployment.profile_id);
  assert.deepEqual(deployment.profile, readJson(profilePath));
  for (const field of ['imageId', 'schemaHash']) {
    const profileKey = field === 'imageId' ? 'guest_image_id' : 'statement_schema_hash';
    assert.equal(Buffer.from(deployment.profile[profileKey]).toString('hex'), compiled[field]);
  }
  const original = Buffer.from(Array.from({ length: 4096 }, (_, i) => (i * 37 + 19) % 251));
  const originalFile = path.join(privateDir, 'original.bin');
  fs.writeFileSync(originalFile, original, { flag: 'wx', mode: 0o600 });
  result.file = { bytes: original.length, sha256: digest(original) };
  const context = { escrow: [...getBytes(deployment.escrow)], asset: [...getBytes(deployment.asset)],
    payer: [...getBytes(payer)], provider_wallet: [...getBytes(relay)], base_timestamp: clock };
  const contextFile = path.join(privateDir, 'context.json'), material = path.join(privateDir, 'material');
  write(contextFile, context);
  await invoke('fixture', fixture, ['experiment', contextFile, originalFile, material, profilePath]);
  const vectors = readJson(path.join(material, 'vectors.json'));
  assert.deepEqual(vectors.profile, deployment.profile);
  assert.equal(vectors.profileId, deployment.profile_id.slice(2));
  const fields = artifact('DeliveryEscrow').abi.find(item => item.name === 'preview').inputs[0].components;
  const statementType = `tuple(${fields.map(item => `${item.type} ${item.name}`).join(',')})`, coder = AbiCoder.defaultAbiCoder();
  const proofs = [];
  for (let relation = 1; relation <= 4; relation++) {
    const input = path.join(material, `relation-${relation}.private.json`);
    const receipt = path.join(proofDir, `relation-${relation}.receipt`);
    const publicFile = path.join(proofDir, `relation-${relation}.json`);
    const execution = readJson(await invoke(`execute-${relation}`, prover, ['execute'], input));
    assert.equal(execution.proved, false); assert.equal(execution.imageId, compiled.imageId);
    await invoke(`prove-${relation}`, prover, ['prove', receipt, '--succinct-receipt', path.join(privateDir, `relation-${relation}.succinct.receipt`)], input, publicFile);
    const proof = readJson(publicFile);
    assert.deepEqual(Object.keys(proof).sort(), ['mode', 'kind', 'imageId', 'profileId', 'relation', 'journal', 'journalHash', 'seal'].sort());
    assert.equal(proof.mode, 'prove'); assert.equal(proof.kind, 'groth16'); assert.equal(proof.relation, relation);
    assert.equal(proof.imageId, compiled.imageId); assert.equal(proof.profileId, vectors.profileId);
    const verified = readJson(await invoke(`verify-${relation}`, prover, ['verify', proof.journal], receipt));
    assert.deepEqual(verified, { verified: true, kind: 'groth16', seal: proof.seal, imageId: compiled.imageId, journalHash: proof.journalHash });
    const wrong = Buffer.from(proof.journal, 'hex');
    const wordIndex = fields.findIndex(item => item.name === (relation === 1 ? 'guestImageId' : 'ciphertextHash'));
    assert(wordIndex >= 0 && wrong.length === fields.length * 32);
    wrong[wordIndex * 32] ^= 1;
    const wrongJournal = wrong.toString('hex');
    assert.notEqual(wrongJournal, proof.journal);
    await invoke(`wrong-journal-${relation}`, prover, ['verify', wrongJournal], receipt, undefined, true);
    proofs.push(proof);
    result.proofs.push({ relation, cycles: execution.cycles, segments: execution.segments,
      receiptBytes: fs.statSync(receipt).size, sealBytes: getBytes(`0x${proof.seal}`).length, journalHash: proof.journalHash, independentlyVerified: true });
  }
  write(path.join(proofDir, 'manifest.json'), { schemaVersion: 1, imageId: compiled.imageId, schemaHash: compiled.schemaHash,
    elfSha256: compiled.elfSha256, proofs: proofs.map(proof => ({ receipt: `relation-${proof.relation}.receipt`, public: `relation-${proof.relation}.json` })) });
  await invoke('adapter', process.execPath, [path.join(root, 'verify-delivery.mjs'), proofDir]);
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
  await invoke('reject-corrupt-E', fixture, ['recover', path.join(material, 'relation-3.private.json'), invalidE, invalidOutput], undefined, undefined, true);
  assert(!fs.existsSync(invalidOutput));
  result.corruptERejectedWithoutOutput = true;
  result.deployment = { chainId: 31337, escrow: deployment.escrow, asset: deployment.asset, payer, provider: relay,
    profileId: deployment.profile_id, adapter: deployment.adapter, verifier: deployment.verifier, tokenDecimals: 6,
    orderId: id, fundTransaction: funded.transactionHash, runtimeCodeHashes: deployment.runtime_code_hashes };
  assert.equal(result.proofs.length, 4); assert.equal(result.branches.length, 3);
  result.status = 'PASS'; progress('complete');
} catch (error) {
  result.status = 'FAIL'; result.failure = { stage: result.stage, kind: error.name, code: error.code ?? null };
  fs.writeFileSync(path.join(privateDir, 'failure.log'), String(error.stack ?? error), { mode: 0o600 });
  progress(result.stage); process.exitCode = 1;
} finally {
  if (active) active.kill('SIGTERM');
  if (provider) await provider.destroy();
  if (deploymentProcess && deploymentProcess.exitCode === null && deploymentProcess.signalCode === null) {
    const stopped = new Promise(resolve => deploymentProcess.once('close', resolve));
    deploymentProcess.kill('SIGTERM');
    if (await stopped !== 0) {
      result.status = 'FAIL'; result.failure = { stage: 'cleanup', kind: 'DeploymentShutdown' };
      progress('cleanup'); process.exitCode = 1;
    }
  }
}
