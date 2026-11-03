import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { randomUUID } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { artifact, artifacts, compile, requireDependency, root } from './compile.mjs';
import { checkRuntime, observe, parseArguments, validateBundle } from './observe-settlement.mjs';

const { Contract, JsonRpcProvider, Transaction, getAddress, sha256 } = requireDependency('ethers');
const HASH = /^0x[0-9a-fA-F]{64}$/;
const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
class Unverified extends Error {}
function interval(condition, message) { if (!condition) throw new Unverified(message); }
function publicError(error) { return error.message.replace(/0x[0-9a-fA-F]{67,}/g, '[long hex omitted]'); }
export const CASES = Object.freeze(['offline-refund', 'partial-refund', 'held-timeout', 'strict-late-open', 'late-priority',
  'hold-reorg', 'capsule-reorg', 'keeper', 'token-false', 'token-frozen', 'token-fee', 'token-sender-tax',
  'token-precision', 'token-nonreturn', 'token-reentrancy', 'node-refund-hook',
  'incoming-false', 'incoming-frozen', 'incoming-fee', 'incoming-sender-tax', 'incoming-precision', 'incoming-reentrancy', 'incoming-nonreturn']);
export const REQUIRED_CASES = Object.freeze([...CASES, 'mixed-held-rotate']);

export function expectedCase(name) {
  assert(REQUIRED_CASES.includes(name), 'unknown case expectation');
  let phases = [3, 0], earned = '12', withdrawn = '12', refunded = '0', orderPresent = true;
  if (name.startsWith('incoming-')) {
    orderPresent = ['incoming-nonreturn', 'incoming-reentrancy'].includes(name);
    return { orderPresent, phases: orderPresent ? [0, 0] : null, earned: '0', withdrawn: '0', refunded: '0', fundReceiptStatus: orderPresent ? 1 : 0 };
  }
  if (['offline-refund', 'held-timeout', 'strict-late-open'].includes(name)) { phases = [4, 4]; earned = withdrawn = '0'; refunded = '29'; }
  if (['partial-refund', 'node-refund-hook', 'mixed-held-rotate'].includes(name)) { phases = [3, 4]; refunded = '17'; }
  if (name === 'late-priority') { phases = [3, 3]; earned = withdrawn = '24'; refunded = '5'; }
  if (name === 'hold-reorg' || name === 'capsule-reorg') { phases = name === 'hold-reorg' ? [0, 0] : [1, 0]; earned = withdrawn = '0'; }
  return { orderPresent, phases, earned, withdrawn, refunded };
}

function keys(value, required, optional = []) {
  assert(value && typeof value === 'object' && !Array.isArray(value));
  for (const name of required) assert(Object.hasOwn(value, name), `missing ${name}`);
  for (const name of Object.keys(value)) assert([...required, ...optional].includes(name), `unexpected ${name}`);
}
function label(value) { assert(typeof value === 'string' && /^[A-Za-z0-9][A-Za-z0-9_-]{0,79}$/.test(value), 'invalid checkpoint label'); }
function quiesced(value) {
  keys(value, ['nodesStopped', 'walletsLocked', 'proversIdle', 'nodeDataCheckpoint']);
  assert(value.nodesStopped === true && value.walletsLocked === true && value.proversIdle === true, 'owner must quiesce all task Nodes and provers');
  label(value.nodeDataCheckpoint);
}
export function validateCommand(command) {
  assert(command && typeof command.op === 'string');
  const common = ['id', 'op'];
  const shapes = {
    'capture-pre-prepare': ['quiesced', 'wallets', 'receiverPeer'],
    'capture-source-ready': ['quiesced', 'wallets', 'orderId', 'receiverPeer'],
    'capture-post-fund': ['quiesced', 'wallets', 'orderId', 'fundTxHash'],
    'resume-main': [], 'verify-core': ['quiesced', 'fileSha256'],
    'exercise-case': ['quiesced', 'case'], 'restore-core': ['quiesced'], 'abort': ['quiesced'],
    'lifecycle-held': ['quiesced'], 'record-key': ['quiesced', 'action', 'txHash'],
  };
  assert(Object.hasOwn(shapes, command.op), 'unknown finite exercise operation');
  keys(command, [...common, ...shapes[command.op]]); label(command.id);
  if (command.quiesced) quiesced(command.quiesced);
  if (command.wallets) {
    keys(command.wallets, ['S', 'R', 'P']);
    for (const address of Object.values(command.wallets)) assert(ADDRESS.test(address), 'invalid public wallet');
    assert.equal(new Set(Object.values(command.wallets).map(getAddress)).size, 3, 'fixture wallets must be distinct');
  }
  for (const name of ['receiverPeer', 'orderId', 'fundTxHash', 'txHash']) if (command[name]) assert(HASH.test(command[name]), `invalid ${name}`);
  if (command.fileSha256) assert(/^[0-9a-f]{64}$/.test(command.fileSha256), 'invalid public file SHA-256');
  if (command.case) assert(CASES.includes(command.case), 'unknown finite genuine branch');
  if (command.action) assert(['rotate', 'revoke', 'register'].includes(command.action), 'unknown key lifecycle action');
  return command;
}

export function validateAcknowledgement(value, checkpointId, writeMode = 'none') {
  assert(['none', 'refund', 'rotate'].includes(writeMode));
  keys(value, ['op', 'checkpointId', 'quiesced', 'nodeObservation', 'rawBroadcastDelta'], ['views', 'readSha256', 'nodeTransactions']);
  assert.equal(value.op, 'checkpoint-ack'); assert.equal(value.checkpointId, checkpointId); quiesced(value.quiesced);
  assert(['observed', 'not_observed'].includes(value.nodeObservation));
  assert(Number.isSafeInteger(value.rawBroadcastDelta) && value.rawBroadcastDelta >= 0);
  if (writeMode === 'none') assert.equal(value.rawBroadcastDelta, 0, 'locked observation broadcast a raw transaction');
  if (value.nodeObservation === 'observed') {
    assert(value.views && Object.keys(value.views).length > 0, 'observed Node views required');
    for (const [role, view] of Object.entries(value.views)) {
      assert(['S', 'R', 'P'].includes(role)); keys(view, ['phase', 'evidence', 'payment', 'windows']);
      assert(['prepared', 'proving', 'funding_pending', 'funded', 'transferring', 'cipher_held', 'key_published', 'recoverable',
        'paid', 'partial', 'refundable', 'refunded', 'reconciling', 'key_unavailable', 'failed'].includes(view.phase));
      assert(['none', 'charity_receipt', 'ciphertext_held', 'recoverable', 'decryption_proven'].includes(view.evidence));
      keys(view.payment, ['earned', 'withdrawn', 'refunded']);
      for (const amount of Object.values(view.payment)) assert(typeof amount === 'string' && amount.length <= 78 && /^(0|[1-9][0-9]*)$/.test(amount));
      assert(Array.isArray(view.windows) && view.windows.length === 2);
      for (const window of view.windows) { keys(window, ['state', 'decryption_proven']); assert(['pending', 'stored', 'held', 'key_published', 'earned', 'refunded'].includes(window.state) && typeof window.decryption_proven === 'boolean'); }
    }
  } else assert(value.views === undefined, 'unobserved checkpoint cannot carry asserted Node views');
  if (value.readSha256 !== undefined) assert(/^[0-9a-f]{64}$/.test(value.readSha256));
  if (value.nodeTransactions !== undefined) {
    assert(writeMode !== 'none' && Array.isArray(value.nodeTransactions) && value.nodeTransactions.length > 0 && value.nodeTransactions.length <= 4);
    assert(value.nodeTransactions.every(hash => HASH.test(hash)));
    assert.equal(new Set(value.nodeTransactions.map(hash => hash.toLowerCase())).size, value.nodeTransactions.length);
  }
  if (writeMode === 'refund') assert(value.nodeObservation === 'observed' && value.views?.S && value.nodeTransactions?.length,
    'native refund requires actual Source view and transaction hashes');
  if (writeMode === 'rotate') assert(value.nodeTransactions?.length === 1, 'one actual Rotate transaction required');
  return value;
}

export function serializedPublicTransaction(tx) {
  assert(tx.signature && [0, 1, 2].includes(tx.type), 'original broadcast signature/type unavailable');
  const value = Transaction.from({ type: tx.type, chainId: tx.chainId, nonce: tx.nonce, gasLimit: tx.gasLimit,
    gasPrice: tx.type < 2 ? tx.gasPrice : null, maxFeePerGas: tx.type === 2 ? tx.maxFeePerGas : null,
    maxPriorityFeePerGas: tx.type === 2 ? tx.maxPriorityFeePerGas : null, accessList: tx.type > 0 ? tx.accessList : null,
    to: tx.to, value: tx.value, data: tx.data, signature: tx.signature });
  assert.equal(value.hash.toLowerCase(), tx.hash.toLowerCase(), 'public transaction cannot reconstruct the original signed hash');
  assert.equal(value.from, getAddress(tx.from));
  return value.serialized;
}

export function nextBaseFee(block) {
  const base = block.baseFeePerGas ?? 0n, target = block.gasLimit / 2n;
  assert(target > 0n);
  if (block.gasUsed === target) return base;
  if (block.gasUsed < target) return base - base * (target - block.gasUsed) / target / 8n;
  const increment = base * (block.gasUsed - target) / target / 8n;
  return base + (increment > 0n ? increment : 1n);
}

function regularJson(file) {
  const stat = fs.lstatSync(file); assert(stat.isFile() && stat.size > 0 && stat.size <= 1048576, 'bounded regular public JSON required');
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}
function publicOrder(order) {
  return { earned: order.earned.toString(), withdrawn: order.withdrawn.toString(), refunded: order.refunded.toString(),
    bonusEarned: order.bonusEarned, bonusRefunded: order.bonusRefunded,
    windows: order.windows.map(window => ({ phase: Number(window.phase), decryptionProven: window.decryptionProven,
      capsule: window.capsule, capsuleHash: window.spec.capsuleHash })) };
}
function sameHash(a, b) { assert.equal(a.toLowerCase(), b.toLowerCase()); }
function callErrorName(contract, error) {
  if (error?.code !== 'CALL_EXCEPTION' || typeof error.data !== 'string') return undefined;
  try { return contract.interface.parseError(error.data)?.name; } catch { return undefined; }
}

export class Exercise {
  constructor(options, lines, emit) {
    this.options = options; this.lines = lines; this.emit = emit;
    this.sessionId = randomUUID(); this.phase = 'capture'; this.snapshots = {}; this.completed = [];
    this.privateDirectory = path.join(artifacts, 'private-branches', this.sessionId);
    fs.mkdirSync(this.privateDirectory, { recursive: true, mode: 0o700 }); fs.chmodSync(this.privateDirectory, 0o700);
    this.publicDirectory = path.join(artifacts, 'branch-observations', this.sessionId);
    fs.mkdirSync(this.publicDirectory, { recursive: true, mode: 0o700 });
    this.keyCases = [];
    this.results = new Map();
    this.requestIds = new Set();
  }
  privateRecord(name, value) {
    const descriptor = fs.openSync(path.join(this.privateDirectory, name), 'a', 0o600);
    try { fs.writeSync(descriptor, `${JSON.stringify(value, (_, item) => typeof item === 'bigint' ? item.toString() : item)}\n`); fs.fsyncSync(descriptor); }
    finally { fs.closeSync(descriptor); }
  }
  publicRecord(name, value) {
    if (name.startsWith('branch-') && REQUIRED_CASES.includes(value.case)) this.results.set(value.case, value);
    fs.writeFileSync(path.join(this.publicDirectory, name), `${JSON.stringify({ schemaVersion: 1, sessionId: this.sessionId, ...value }, null, 2)}\n`, { mode: 0o600 });
    if (name !== 'index.json') this.index();
  }
  index() {
    const restored = this.phase === 'restored' && this.restoreResult;
    const cases = [...this.results.values()].map(item => {
      const file = `branch-${item.case}.json`, order = item.state?.order;
      const finalObservations = (item.nodeObservations ?? []).filter(entry => entry.status === 'observed'
        && ['-result', '-incoming-result', '-mixed-held-result', '-native-refund-ready', '-after-reorg'].some(suffix => entry.checkpointId.endsWith(suffix)));
      const roles = [...new Set(finalObservations.flatMap(entry => entry.roles))].sort();
      const actual = item.state ? { orderPresent: Boolean(order), phases: order?.windows.map(window => window.phase) ?? null,
        earned: order?.earned ?? '0', withdrawn: order?.withdrawn ?? '0', refunded: order?.refunded ?? '0',
        ...(item.actualFundReceiptStatus !== undefined ? { fundReceiptStatus: item.actualFundReceiptStatus } : {}) } : null;
      return { case: item.case, status: item.status, file, fileSha256: sha256(fs.readFileSync(path.join(this.publicDirectory, file))).slice(2),
        expected: expectedCase(item.case), actual, nodeObservation: { status: roles.length ? 'OBSERVED' : 'NOT_OBSERVED', roles },
        nodeAutonomy: item.nodeAutonomy ?? 'NOT_VERIFIED' };
    });
    const restore = { status: restored ? 'PASS' : 'PENDING', file: 'core-restored.json',
      ...(restored ? { fileSha256: sha256(fs.readFileSync(path.join(this.publicDirectory, 'core-restored.json'))).slice(2),
        confirmationAnchor: this.restoreResult.confirmationPolicy } : {}) };
    this.publicRecord('index.json', { kind: 'genuine-settlement-branch-index', requiredCases: REQUIRED_CASES, coreRestored: Boolean(restored), restore,
      imageId: this.core?.result.imageId ?? ('0x' + Buffer.from(this.deployment.profile.guest_image_id).toString('hex')),
      profileId: this.core?.result.profileId ?? this.deployment.profile_id, absoluteFinality: false, cases,
      lifecycle: this.keyCases.map(item => ({ case: item.case, status: item.status, file: `key-${item.case}.json`, txHash: item.txHash,
        fileSha256: sha256(fs.readFileSync(path.join(this.publicDirectory, `key-${item.case}.json`))).slice(2) })) });
  }
  async initialize() {
    const deployment = regularJson(this.options.deploymentFile);
    assert.equal(deployment.schema_version, 1); assert.equal(deployment.chain_id, 31337); assert.equal(deployment.confirmations, 2);
    assert.equal(new URL(deployment.rpc_url).href, this.options.rpcUrl);
    for (const name of ['escrow', 'asset', 'funder']) assert(ADDRESS.test(deployment[name]));
    assert.deepEqual(deployment.profile, regularJson(path.resolve(root, '../../crates/delivery/proof/profile.json')), 'deployment differs from fixed product profile');
    compile({ checkTrust: true });
    this.provider = new JsonRpcProvider(this.options.rpcUrl, undefined, { cacheTimeout: -1 });
    assert.equal((await this.provider.getNetwork()).chainId, 31337n);
    const accounts = await this.provider.send('eth_accounts', []); sameHash(accounts[0], deployment.funder);
    this.deployment = deployment; this.funder = getAddress(deployment.funder);
    this.escrow = new Contract(deployment.escrow, artifact('DeliveryEscrow').abi, this.provider);
    this.asset = new Contract(deployment.asset, artifact('TestToken').abi, this.provider);
    this.trust = regularJson(path.join(root, 'trust.json'));
    await this.identity();
    this.index();
    this.emit({ event: 'ready', sessionId: this.sessionId, chainId: 31337, escrow: deployment.escrow,
      profileId: deployment.profile_id, publicIndex: path.relative(path.resolve(root, '../..'), path.join(this.publicDirectory, 'index.json')),
      proofStatus: 'NOT_VERIFIED_UNTIL_VERIFY_CORE', nodeDataOwner: 'acceptance runner only' });
  }
  async identity() {
    checkRuntime(this.trust.contracts.DeliveryEscrow, await this.provider.getCode(this.deployment.escrow));
    checkRuntime(this.trust.contracts.TestToken, await this.provider.getCode(this.deployment.asset));
    sameHash(await this.escrow.profileId(), this.deployment.profile_id);
    const adapterAddress = await this.escrow.adapter();
    checkRuntime(this.trust.contracts.RiscZeroAdapter, await this.provider.getCode(adapterAddress));
    const adapter = new Contract(adapterAddress, artifact('RiscZeroAdapter').abi, this.provider);
    sameHash(await adapter.imageId(), '0x' + Buffer.from(this.deployment.profile.guest_image_id).toString('hex'));
    checkRuntime(this.trust.contracts.RiscZeroGroth16Verifier, await this.provider.getCode(await adapter.verifier()));
  }
  async stopMiner() { assert.equal(await this.provider.send('miner_stop', []), true); this.mining = false; }
  async startMiner() {
    const head = await this.latest();
    await this.provider.send('evm_setTime', [Math.max(Date.now(), (head.timestamp + 1) * 1000)]);
    assert.equal(await this.provider.send('miner_start', [1]), true); this.mining = true;
  }
  async nonceFacts() {
    const result = {};
    for (const address of [...new Set([...Object.values(this.wallets ?? {}), this.funder].map(getAddress))]) {
      const latest = await this.provider.getTransactionCount(address, 'latest'), pending = await this.provider.getTransactionCount(address, 'pending');
      assert.equal(pending, latest, 'pending transactions prevent a deterministic snapshot'); result[address] = latest;
    }
    return result;
  }
  async latest() { const block = await this.provider.getBlock('latest'); assert(block?.hash); return block; }
  async confirmationPolicy() {
    const tip = await this.latest(), anchor = await this.provider.getBlock(tip.number - 1);
    assert(anchor?.hash, 'two confirmed experimental blocks are required');
    assert((this.caseTransactions ?? []).every(record => record.blockNumber <= anchor.number), 'branch transaction is not confirmed');
    return { kind: 'experimental-block-depth', confirmations: 2, observedTip: tip.number,
      anchorNumber: anchor.number, anchorHash: anchor.hash, absoluteFinality: false };
  }
  async state() {
    const block = await this.latest();
    let order = null;
    if (this.orderId) {
      try { order = publicOrder(await this.escrow.getOrder(this.orderId)); }
      catch (error) { if (!this.allowUnfunded || callErrorName(this.escrow, error) !== 'State') throw error; }
    }
    const balances = {};
    for (const address of [...new Set([...Object.values(this.wallets ?? {}), this.funder, this.deployment.escrow].map(getAddress))]) {
      balances[address] = (await this.asset.balanceOf(address)).toString();
    }
    return { anchor: { number: block.number, hash: block.hash, timestamp: block.timestamp }, order, tokenBalances: balances,
      tokenMode: Number(await this.asset.mode()), tokenDecimals: Number(await this.asset.decimals()) };
  }
  async capture(name, command) {
    assert(this.phase === 'capture', 'snapshots must be captured before core verification');
    assert(!this.snapshots[name], 'checkpoint already exists'); await this.stopMiner();
    const wallets = Object.fromEntries(Object.entries(command.wallets).map(([role, address]) => [role, getAddress(address)]));
    if (this.wallets) assert.deepEqual(wallets, this.wallets, 'fixture wallets changed'); else this.wallets = wallets;
    if (name === 'C1') {
      if (this.orderId) sameHash(command.orderId, this.orderId);
      this.orderId = command.orderId;
      this.allowUnfunded = false;
      const receipt = await this.provider.getTransactionReceipt(command.fundTxHash), block = await this.latest();
      assert(receipt?.status === 1 && receipt.blockNumber <= block.number - 1, 'fund requires two real experimental confirmations');
      const tx = await this.provider.getTransaction(command.fundTxHash); sameHash(tx.to, this.deployment.escrow);
      assert.equal(this.escrow.interface.parseTransaction({ data: tx.data }).name, 'fund');
      const order = await this.escrow.getOrder(this.orderId);
      assert.equal(order.terms.acceptance, 1n); assert.equal(order.terms.windowCount, 2n); assert.equal(order.terms.escrowAmount, 29n);
      assert(order.windows.every(window => window.phase === 0n)); assert.equal(order.earned + order.withdrawn + order.refunded, 0n);
      sameHash(order.terms.payer, this.wallets.S); sameHash(order.terms.providerWallet, this.wallets.P);
      this.terms = order.terms; this.fundTxHash = command.fundTxHash;
    } else {
      if (name === 'C0Prime') {
        assert(this.snapshots.C0, 'source-ready must follow the actual initial C0 barrier');
        this.orderId = command.orderId; this.allowUnfunded = true;
      }
      this.receiverPeer = command.receiverPeer;
      const key = await this.escrow.keys(command.receiverPeer); assert(key.active && key.serial > 0n, 'C0 requires genuine current registration');
    }
    const state = await this.state(), nonces = await this.nonceFacts();
    if (name === 'C0Prime') {
      assert.equal(state.order, null, 'C0Prime must precede actual funding');
      assert.deepEqual(nonces, this.snapshots.C0.nonces, 'approve/fund reservation already escaped the source-ready RPC barrier');
      assert.deepEqual(state.tokenBalances, this.snapshots.C0.state.tokenBalances);
    }
    assert.equal(state.tokenMode, 0); assert.equal(state.tokenDecimals, 6);
    const id = await this.provider.send('evm_snapshot', []);
    this.snapshots[name] = { id, state, nonces, nodeDataCheckpoint: command.quiesced.nodeDataCheckpoint };
    this.privateRecord('checkpoints.jsonl', { name, id, state, nonces, owner: command.quiesced });
    this.emit({ event: 'captured', id: command.id, checkpoint: name, state, nodeDataCheckpoint: command.quiesced.nodeDataCheckpoint,
      validity: 'this live Ganache session only', incomingCoverage: name !== 'C1' ? 'CAPTURED_NOT_YET_EXERCISED' : undefined });
  }
  async resetC1() {
    await this.stopMiner();
    const saved = this.snapshots.C1; assert(saved, 'C1 was not captured at the real post-fund barrier');
    assert.equal(await this.provider.send('evm_revert', [saved.id]), true, 'C1 snapshot expired or invalid');
    await this.identity(); assert.deepEqual(await this.nonceFacts(), saved.nonces);
    const state = await this.state(); assert.deepEqual(state, saved.state, 'C1 reverted state differs from its original fingerprint');
    saved.id = await this.provider.send('evm_snapshot', []);
    this.privateRecord('checkpoints.jsonl', { name: 'C1-recreated', id: saved.id, state });
  }
  async resetIncoming() {
    await this.stopMiner();
    const name = this.snapshots.C0Prime ? 'C0Prime' : 'C0', saved = this.snapshots[name];
    interval(saved?.id, 'incoming cases require a real captured pre-fund snapshot');
    assert.equal(await this.provider.send('evm_revert', [saved.id]), true, 'incoming snapshot expired');
    this.snapshots.C1.id = undefined;
    this.allowUnfunded = true; await this.identity();
    assert.deepEqual(await this.nonceFacts(), saved.nonces);
    const state = await this.state(); assert.deepEqual(state, saved.state);
    saved.id = await this.provider.send('evm_snapshot', []);
    this.privateRecord('checkpoints.jsonl', { name: `${name}-recreated`, id: saved.id, state });
    return saved;
  }
  async mineAt(timestamp) {
    const parent = await this.latest(), value = BigInt(timestamp);
    assert(value > BigInt(parent.timestamp) && value <= BigInt(Number.MAX_SAFE_INTEGER), 'timestamp must advance from the current reverted parent');
    await this.provider.send('evm_mine', [Number(value)]);
    const block = await this.latest(); assert.equal(BigInt(block.timestamp), value, 'fixture did not honor explicit mining timestamp');
    return block;
  }
  async advancePast(deadline) { const block = await this.latest(); await this.mineAt(BigInt(block.timestamp) >= deadline ? BigInt(block.timestamp) + 1n : deadline + 1n); }
  async retainBroadcast(hash, label) {
    const tx = await this.provider.getTransaction(hash); assert(tx, 'broadcast transaction unavailable');
    const raw = serializedPublicTransaction(tx);
    this.privateRecord('broadcast-signed-transactions.jsonl', { label, hash, from: tx.from, nonce: tx.nonce, raw });
    return { tx, raw };
  }
  async mined(hash, label, { rejectedWith, timestamp } = {}) {
    const before = await this.latest(); await this.mineAt(timestamp ?? BigInt(before.timestamp) + 1n);
    const receipt = await this.provider.getTransactionReceipt(hash);
    assert(receipt && receipt.status === (rejectedWith ? 0 : 1), `${label} did not reach its required actual receipt status`);
    const { tx } = await this.retainBroadcast(hash, label);
    if (rejectedWith) {
      let rejection;
      try {
        await this.provider.send('eth_call', [{ from: tx.from, to: tx.to, data: tx.data,
          value: `0x${tx.value.toString(16)}`, gas: `0x${tx.gasLimit.toString(16)}` }, `0x${receipt.blockNumber.toString(16)}`]);
      } catch (error) { rejection = callErrorName(this.escrow, error); }
      assert.equal(rejection, rejectedWith, `${label} reverted for a different reason or exhausted gas`);
      this.privateRecord('rejection-reasons.jsonl', { hash, blockNumber: receipt.blockNumber, expected: rejectedWith, actual: rejection });
    }
    const included = await this.provider.getBlock(receipt.blockNumber); sameHash(included.hash, receipt.blockHash);
    await this.mineAt(BigInt(included.timestamp) + 1n);
    this.caseTransactions?.push({ label, hash, status: receipt.status, blockNumber: receipt.blockNumber, blockHash: receipt.blockHash,
      gasUsed: receipt.gasUsed.toString(), executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString() });
    return receipt;
  }
  async keeper(contract, method, args, rejectedWith) {
    const before = rejectedWith ? await this.state() : undefined;
    const data = contract.interface.encodeFunctionData(method, args);
    // Ganache's default tip can exceed its generated fee cap after long idle proof runs.
    const tip = 1_000_000_000n, cap = nextBaseFee(await this.latest()) * 2n + tip;
    const hash = await this.provider.send('eth_sendTransaction', [{ from: this.funder, to: await contract.getAddress(), data,
      gas: '0x5b8d80', value: '0x0', maxPriorityFeePerGas: `0x${tip.toString(16)}`, maxFeePerGas: `0x${cap.toString(16)}` }]);
    const receipt = await this.mined(hash, method, { rejectedWith });
    if (rejectedWith) { const after = await this.state(); assert.deepEqual(after.order, before.order, 'reverted call changed order'); assert.deepEqual(after.tokenBalances, before.tokenBalances, 'reverted call moved token balances'); }
    return receipt;
  }
  async replay(record, { confirm = true, timestamp = record.timestamp, rejectedWith } = {}) {
    assert.equal(await this.provider.getTransactionCount(record.tx.from, 'pending'), record.tx.nonce, 'original nonce is not available');
    const cap = record.tx.type === 2 ? record.tx.maxFeePerGas : record.tx.gasPrice;
    for (let attempt = 0; attempt < 64 && nextBaseFee(await this.latest()) > cap; attempt++) {
      const parent = await this.latest(); interval(BigInt(parent.timestamp) + 1n < BigInt(timestamp), 'original fee cap cannot fit the restored mining interval');
      await this.mineAt(BigInt(parent.timestamp) + 1n);
    }
    const parent = await this.latest(); interval(nextBaseFee(parent) <= cap, 'original fee cap still insufficient; no replacement signature permitted');
    interval(BigInt(timestamp) > BigInt(parent.timestamp), 'original transaction timestamp no longer fits restored parent');
    const hash = await this.provider.send('eth_sendRawTransaction', [record.raw]); sameHash(hash, record.tx.hash);
    if (confirm) return this.mined(hash, `original-${record.method}`, { timestamp: BigInt(timestamp), rejectedWith });
    return hash;
  }
  proof(relation, index) { return this.proofs.get(`${relation}:${index}`).seal; }
  async hold(index) { interval(BigInt((await this.latest()).timestamp) + 2n <= this.terms.holdDeadline, 'hold deadline has no valid fixture interval'); return this.keeper(this.escrow, 'hold', [this.orderId, index, this.proof(3, index)]); }
  async publish(index) { interval(BigInt((await this.latest()).timestamp) + 2n <= this.terms.keyDeadline, 'publish deadline has no valid interval'); return this.keeper(this.escrow, 'publish', [this.orderId, index, this.capsules[index]]); }
  async open(index) { interval(BigInt((await this.latest()).timestamp) + 2n <= this.terms.openDeadline, 'open deadline has no valid interval'); return this.keeper(this.escrow, 'open', [this.orderId, index, this.proof(4, index)]); }
  async complete(index) { await this.hold(index); await this.publish(index); await this.open(index); }
  async expect(phases, earned, withdrawn, refunded) {
    const state = await this.state(); assert.deepEqual(state.order.windows.map(window => window.phase), phases);
    for (const [name, value] of Object.entries({ earned, withdrawn, refunded })) assert.equal(state.order[name], value);
    const baseline = this.snapshots.C1.state.tokenBalances;
    const payer = getAddress(this.wallets.S), provider = getAddress(this.wallets.P), escrow = getAddress(this.deployment.escrow);
    assert.equal(BigInt(state.tokenBalances[payer]) - BigInt(baseline[payer]), BigInt(refunded));
    assert.equal(BigInt(state.tokenBalances[provider]) - BigInt(baseline[provider]), BigInt(withdrawn));
    assert.equal(BigInt(baseline[escrow]) - BigInt(state.tokenBalances[escrow]), BigInt(refunded) + BigInt(withdrawn));
    return state;
  }
  async checkpoint(point, { nativeRefund = false, keyRotation = false, nodeDataCheckpoint, expected } = {}) {
    const state = await this.state(), confirmed = await this.provider.getBlock(state.anchor.number - 1);
    assert(confirmed?.hash, 'checkpoint requires two experimental blocks');
    const checkpointId = `${this.requestId}-${point}`;
    const writeMode = nativeRefund ? 'refund' : keyRotation ? 'rotate' : 'none';
    if (writeMode !== 'none') await this.startMiner();
    this.emit({ event: 'checkpoint', checkpointId, case: this.currentCase ?? 'core-restoration', state,
      confirmationAnchor: { number: confirmed.number, hash: confirmed.hash, confirmations: 2, absoluteFinality: false },
      nodeMode: nativeRefund ? 'owner-native-refund' : keyRotation ? 'owner-key-rotate' : 'locked-observe', nodeDataCheckpoint: nodeDataCheckpoint ?? this.core.nodeDataCheckpoint,
      expected, noNodeDataCopyByHelper: true });
    const line = await this.lines.next(); assert(!line.done, 'checkpoint acknowledgement missing');
    assert(Buffer.byteLength(line.value) <= 65536, 'JSONL acknowledgement exceeds bound');
    const ack = validateAcknowledgement(JSON.parse(line.value), checkpointId, writeMode);
    if (writeMode !== 'none') { await this.stopMiner(); await this.nonceFacts(); }
    if (ack.readSha256 !== undefined) assert.equal(ack.readSha256, this.core.fileSha256, 'original file SHA changed');
    const after = await this.state();
    if (writeMode === 'none') assert.deepEqual(after, state, 'chain changed during a locked observation');
    const facts = (writeMode !== 'none' ? after.order : state.order) ?? { earned: '0', withdrawn: '0', refunded: '0',
      windows: [{ phase: 0, decryptionProven: false }, { phase: 0, decryptionProven: false }] };
    if (ack.views && writeMode !== 'rotate') for (const view of Object.values(ack.views)) {
      for (const name of ['earned', 'withdrawn', 'refunded']) assert.equal(view.payment[name], facts[name], `Node ${name} did not converge`);
      const names = ['pending', 'held', 'key_published', 'earned', 'refunded'];
      for (let i = 0; i < 2; i++) {
        assert.equal(view.windows[i].state, names[facts.windows[i].phase], 'Node window did not converge');
        assert.equal(view.windows[i].decryption_proven, facts.windows[i].decryptionProven, 'Node retained stale proof strength');
      }
      if (!facts.windows.every(window => window.phase === 3)) assert.notEqual(view.phase, 'paid', 'Node retained stale paid phase');
    }
    this.privateRecord('node-checkpoint-acks.jsonl', { checkpointId, acknowledgement: ack, state: after });
    this.caseNodeObservations?.push({ checkpointId, roles: Object.keys(ack.views ?? {}), status: ack.nodeObservation,
      rawBroadcastDelta: ack.rawBroadcastDelta, nativeRefund, keyRotation });
    return ack;
  }
  async verifyCore(command) {
    assert(this.snapshots.C1 && !this.core, 'a real C1 snapshot must precede core verification'); await this.stopMiner();
    const result = await observe({ bundleFile: this.options.bundleFile, rpcUrl: this.options.rpcUrl });
    interval(result.status === 'PASS', `original genuine strict oracle did not pass: ${result.error ?? ''}`);
    const bundle = validateBundle(regularJson(this.options.bundleFile));
    sameHash(bundle.orderId, this.orderId); sameHash(bundle.escrow, this.deployment.escrow); sameHash(bundle.asset, this.deployment.asset);
    sameHash(bundle.transactions.fund, this.fundTxHash);
    const names = ['registerKey', ...(bundle.transactions.approve ? ['approve'] : []), 'fund', 'hold', 'publish', 'open', 'withdraw'];
    const records = [];
    for (const method of names) for (const hash of [bundle.transactions[method]].flat()) {
      const tx = await this.provider.getTransaction(hash), receipt = await this.provider.getTransactionReceipt(hash);
      assert(tx && receipt?.status === 1); const included = await this.provider.getBlock(receipt.blockNumber);
      records.push({ tx, receipt, raw: serializedPublicTransaction(tx), method, timestamp: included.timestamp,
        index: ['hold', 'publish', 'open'].includes(method) ? Number(this.escrow.interface.parseTransaction({ data: tx.data }).args[1]) : null });
    }
    records.sort((a, b) => a.receipt.blockNumber - b.receipt.blockNumber || a.receipt.index - b.receipt.index);
    const finalNonces = await this.nonceFacts();
    for (const [address, nonce] of Object.entries(this.snapshots.C1.nonces)) {
      const count = records.filter(record => getAddress(record.tx.from) === address && record.receipt.blockNumber > this.snapshots.C1.state.anchor.number).length;
      assert.equal(finalNonces[address], nonce + count, 'unlisted post-C1 transaction prevents exact original-nonce restoration');
    }
    this.proofs = new Map();
    for (const entry of bundle.proofs) {
      const value = regularJson(path.join(path.dirname(this.options.bundleFile), entry.public));
      this.proofs.set(`${entry.relation}:${entry.windowIndex ?? 'key'}`, { seal: `0x${value.seal}`, journalHash: value.journalHash });
    }
    this.capsules = [0, 1].map(index => `0x${bundle.capsules.find(value => value.windowIndex === index).capsule}`);
    this.core = { records, finalNonces, fileSha256: command.fileSha256, nodeDataCheckpoint: command.quiesced.nodeDataCheckpoint, result };
    this.core.postFundRecords = records.filter(record => ['hold', 'publish', 'open', 'withdraw'].includes(record.method));
    assert.equal(this.terms.receiverKeySerial, 1n, 'approved lifecycle sequence starts with binding1');
    this.privateRecord('main-success-broadcast-transactions.jsonl', records.map(record => ({ hash: record.tx.hash, raw: record.raw,
      from: record.tx.from, nonce: record.tx.nonce, method: record.method, index: record.index, timestamp: record.timestamp,
      blockNumber: record.receipt.blockNumber, transactionIndex: record.receipt.index })));
    this.phase = 'verified';
    this.index();
    this.emit({ event: 'core-verified', id: command.id, imageId: result.imageId, profileId: result.profileId,
      genuineProofs: 7, newProofsByExercise: 0, originalSignedTransactionsBackedUp: records.length });
  }
  async exerciseCase(command) {
    assert(this.core && ['verified', 'restored'].includes(this.phase) && !this.rotation && !this.lifecycleCanonical,
      'genuine branches must precede new key lifecycle');
    this.phase = 'verified'; this.index();
    this.currentCase = command.case; this.requestId = command.id; this.caseTransactions = []; this.caseNodeObservations = [];
    if (command.case.startsWith('incoming-')) return this.incomingCase(command);
    await this.resetC1();
    await this.checkpoint('restore-data', { nodeDataCheckpoint: this.core.nodeDataCheckpoint });
    const kind = command.case;
    if (kind === 'offline-refund') {
      await this.advancePast(this.terms.holdDeadline);
      await this.keeper(this.escrow, 'refund', [this.orderId, 0]); await this.keeper(this.escrow, 'refund', [this.orderId, 1]);
      await this.expect([4, 4], '0', '0', '29');
    } else if (kind === 'partial-refund' || kind === 'node-refund-hook') {
      await this.complete(0); await this.keeper(this.escrow, 'withdraw', [this.orderId]); await this.advancePast(this.terms.holdDeadline);
      if (kind === 'node-refund-hook') {
        const ack = await this.checkpoint('native-refund-ready', { nativeRefund: true,
          nodeDataCheckpoint: this.snapshots.C1.nodeDataCheckpoint, expected: { phases: [3, 4], earned: '12', withdrawn: '12', refunded: '17' } });
        let actualRefund = false;
        for (const hash of ack.nodeTransactions) {
          const receipt = await this.provider.getTransactionReceipt(hash), tx = await this.provider.getTransaction(hash);
          assert(tx && receipt); await this.retainBroadcast(hash, 'owner-native-refund');
          this.caseTransactions.push({ label: 'owner-native-refund', hash, status: receipt.status, blockNumber: receipt.blockNumber,
            blockHash: receipt.blockHash, gasUsed: receipt.gasUsed.toString(), executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString() });
          sameHash(tx.from, this.wallets.S); sameHash(tx.to, this.deployment.escrow); assert.equal(receipt.status, 1, 'Node refund operation reverted');
          const parsed = this.escrow.interface.parseTransaction({ data: tx.data });
          assert(['refund', 'refundBonus'].includes(parsed.name)); sameHash(parsed.args[0], this.orderId);
          if (parsed.name === 'refund') { assert.equal(parsed.args[1], 1n); actualRefund = true; }
          const head = await this.latest(); assert(receipt.blockNumber <= head.number - 1, 'native refund is not experimentally confirmed');
        }
        assert(actualRefund, 'Source refund transaction is missing');
      } else await this.keeper(this.escrow, 'refund', [this.orderId, 1]);
      await this.expect([3, 4], '12', '12', '17');
    } else if (kind === 'held-timeout' || kind === 'strict-late-open') {
      await this.hold(0);
      if (kind === 'strict-late-open') await this.publish(0);
      await this.checkpoint(kind === 'held-timeout' ? 'held' : 'key-published');
      await this.keeper(this.escrow, 'refund', [this.orderId, 0], 'Deadline');
      await this.advancePast(kind === 'held-timeout' ? this.terms.keyDeadline : this.terms.openDeadline);
      if (kind === 'held-timeout') await this.keeper(this.escrow, 'publish', [this.orderId, 0, this.capsules[0]], 'Deadline');
      else await this.keeper(this.escrow, 'open', [this.orderId, 0, this.proof(4, 0)], 'Deadline');
      await this.keeper(this.escrow, 'refund', [this.orderId, 0]); await this.keeper(this.escrow, 'refund', [this.orderId, 1]);
      await this.keeper(this.escrow, 'open', [this.orderId, 0, this.proof(4, 0)], 'State');
      const state = await this.expect([4, 4], '0', '0', '29');
      assert.equal(state.order.windows[0].capsule, kind === 'held-timeout' ? '0x' : this.capsules[0]);
    } else if (kind === 'late-priority') {
      for (const index of [0, 1]) { await this.hold(index); await this.publish(index); }
      await this.advancePast(this.terms.priorityDeadline);
      interval(BigInt((await this.latest()).timestamp) + 9n <= this.terms.openDeadline, 'late-priority branch has no legal open interval');
      await this.keeper(this.escrow, 'refundBonus', [this.orderId]);
      await this.open(0); await this.open(1); await this.keeper(this.escrow, 'withdraw', [this.orderId]);
      await this.expect([3, 3], '24', '24', '5');
    } else if (kind === 'hold-reorg' || kind === 'capsule-reorg') {
      const hold = this.core.records.find(record => record.method === 'hold' && record.index === 0);
      let previous = await this.provider.send('evm_snapshot', []);
      await this.replay(hold);
      if (kind === 'capsule-reorg') {
        previous = await this.provider.send('evm_snapshot', []);
        await this.replay(this.core.records.find(record => record.method === 'publish' && record.index === 0));
      }
      await this.checkpoint('before-reorg');
      const removed = this.core.records.find(record => record.method === (kind === 'hold-reorg' ? 'hold' : 'publish') && record.index === 0);
      assert.equal(await this.provider.send('evm_revert', [previous]), true); await this.nonceFacts();
      const parent = await this.latest(); await this.mineAt(BigInt(parent.timestamp) + 1n); await this.mineAt(BigInt(parent.timestamp) + 2n);
      assert.equal(await this.provider.getTransactionReceipt(removed.tx.hash), null, 'original receipt survived the controlled reorg');
      const state = await this.expect(kind === 'hold-reorg' ? [0, 0] : [1, 0], '0', '0', '0');
      assert.equal(state.order.windows[0].capsule, '0x');
      await this.checkpoint('after-reorg');
    } else if (kind === 'keeper') {
      await this.keeper(this.escrow, 'hold', [this.orderId, 1, this.proof(3, 0)], 'Proof');
      await this.hold(0); await this.keeper(this.escrow, 'publish', [this.orderId, 0, `0x${'ff'.repeat(80)}`], 'Invalid');
      await this.publish(0); await this.open(0); await this.keeper(this.escrow, 'withdraw', [this.orderId]);
      await this.keeper(this.escrow, 'withdraw', [this.orderId], 'State'); await this.keeper(this.escrow, 'cancel', [this.orderId], 'Invalid');
      await this.expect([3, 0], '12', '12', '0');
    } else {
      const modes = { 'token-false': 2, 'token-frozen': 3, 'token-fee': 4, 'token-sender-tax': 6,
        'token-precision': 7, 'token-nonreturn': 1, 'token-reentrancy': 5 };
      assert(Object.hasOwn(modes, kind)); await this.complete(0);
      if (kind === 'token-reentrancy') await this.keeper(this.asset, 'attack', [this.deployment.escrow, this.escrow.interface.encodeFunctionData('withdraw', [this.orderId])]);
      await this.keeper(this.asset, 'configure', [modes[kind]]);
      const fails = !['token-nonreturn', 'token-reentrancy'].includes(kind);
      await this.keeper(this.escrow, 'withdraw', [this.orderId], fails ? 'Token' : undefined);
      if (kind === 'token-reentrancy') assert.equal(await this.asset.reentrySucceeded(), false);
      if (fails) { const state = await this.expect([3, 0], '12', '0', '0'); assert.equal(state.order.windows[0].capsule, this.capsules[0]); }
      await this.keeper(this.asset, 'configure', [0]);
      if (fails) await this.keeper(this.escrow, 'withdraw', [this.orderId]);
      await this.expect([3, 0], '12', '12', '0');
    }
    await this.checkpoint('result');
    const result = { case: kind, status: 'EVM_PASS', newProofs: 0, originalGenuineProofSet: 7, state: await this.state(),
      imageId: this.core.result.imageId, profileId: this.core.result.profileId,
      confirmationPolicy: await this.confirmationPolicy(),
      transactions: [...this.caseTransactions], nodeObservations: [...this.caseNodeObservations],
      nodeAutonomy: kind === 'node-refund-hook' ? 'OWNER_PROCESS_EVIDENCE_AND_SOURCE_TRANSACTIONS_CHECKED' : 'NOT_TESTED_KEEPER_OR_ORIGINAL_RAW_ONLY' };
    this.completed.push(result); this.privateRecord('branch-results.jsonl', result); this.publicRecord(`branch-${kind}.json`, result);
    await this.resetC1(); await this.checkpoint('case-reset', { nodeDataCheckpoint: this.snapshots.C1.nodeDataCheckpoint });
    this.emit({ event: 'case-complete', id: command.id, ...result }); this.currentCase = undefined;
  }
  async rebuildC1() {
    await this.resetIncoming();
    const previous = this.snapshots.C1;
    const approve = this.core.records.find(record => record.method === 'approve'), fund = this.core.records.find(record => record.method === 'fund');
    interval(approve && fund, 'original actual approve/fund records required for C1 reconstruction');
    await this.replay(approve); await this.replay(fund);
    this.allowUnfunded = false;
    const state = await this.state(), nonces = await this.nonceFacts();
    assert.deepEqual(state.order, previous.state.order); assert.deepEqual(state.tokenBalances, previous.state.tokenBalances);
    assert.equal(state.tokenMode, 0); assert.equal(state.tokenDecimals, 6); assert.deepEqual(nonces, previous.nonces);
    const id = await this.provider.send('evm_snapshot', []);
    this.snapshots.C1 = { ...previous, id, state, nonces };
    const ack = await this.checkpoint('rebuild-C1-data', { nodeDataCheckpoint: previous.nodeDataCheckpoint });
    this.snapshots.C1.nodeDataCheckpoint = ack.quiesced.nodeDataCheckpoint;
    this.privateRecord('checkpoints.jsonl', { name: 'C1-rebuilt-from-original-raw', id, state, nonces, nodeDataCheckpoint: ack.quiesced.nodeDataCheckpoint });
  }
  async incomingCase(command) {
    const modes = { 'incoming-false': 2, 'incoming-frozen': 3, 'incoming-fee': 4, 'incoming-sender-tax': 6,
      'incoming-precision': 7, 'incoming-reentrancy': 5, 'incoming-nonreturn': 1 };
    assert(Object.hasOwn(modes, command.case));
    const baseline = await this.resetIncoming();
    await this.checkpoint('restore-C0-data', { nodeDataCheckpoint: baseline.nodeDataCheckpoint });
    interval(BigInt((await this.latest()).timestamp) + 12n <= this.terms.quoteExpiresAt, 'incoming token branch has no legal quote interval');
    if (command.case === 'incoming-reentrancy') await this.keeper(this.asset, 'attack', [this.deployment.escrow, this.escrow.interface.encodeFunctionData('withdraw', [this.orderId])]);
    await this.keeper(this.asset, 'configure', [modes[command.case]]);
    const approve = this.core.records.find(record => record.method === 'approve'), fund = this.core.records.find(record => record.method === 'fund');
    assert(approve && fund, 'original real approved/funded transactions are missing');
    const nextTime = async () => BigInt((await this.latest()).timestamp) + 1n;
    await this.replay(approve, { timestamp: await nextTime() });
    const before = await this.state(), fails = !['incoming-nonreturn', 'incoming-reentrancy'].includes(command.case);
    const timestamp = await nextTime(); interval(timestamp >= this.terms.keyValidFrom && timestamp <= this.terms.quoteExpiresAt, 'fund time would not isolate the token rule');
    await this.replay(fund, { timestamp, rejectedWith: fails ? 'Token' : undefined });
    const after = await this.state();
    if (fails) {
      assert.equal(after.order, null, 'reverted incoming transfer created an order');
      assert.deepEqual(after.tokenBalances, before.tokenBalances, 'reverted incoming transfer moved funds');
      assert.equal(after.tokenDecimals, 6, 'precision mutation escaped rollback');
    } else {
      assert.deepEqual(after.order.windows.map(window => window.phase), [0, 0]);
      assert.equal(after.order.earned, '0'); assert.equal(after.order.withdrawn, '0'); assert.equal(after.order.refunded, '0');
      const payer = getAddress(this.wallets.S), escrow = getAddress(this.deployment.escrow), provider = getAddress(this.wallets.P);
      assert.equal(BigInt(before.tokenBalances[payer]) - BigInt(after.tokenBalances[payer]), 29n);
      assert.equal(BigInt(after.tokenBalances[escrow]) - BigInt(before.tokenBalances[escrow]), 29n);
      assert.equal(after.tokenBalances[provider], before.tokenBalances[provider]);
      if (command.case === 'incoming-reentrancy') assert.equal(await this.asset.reentrySucceeded(), false);
    }
    await this.checkpoint('incoming-result', { nodeDataCheckpoint: baseline.nodeDataCheckpoint });
    const result = { case: command.case, status: 'EVM_PASS', newProofs: 0, originalGenuineProofSet: 7,
      imageId: this.core.result.imageId, profileId: this.core.result.profileId, snapshotKind: this.snapshots.C0Prime ? 'C0Prime' : 'C0',
      actualFundReceiptStatus: fails ? 0 : 1, state: after, transactions: [...this.caseTransactions], nodeObservations: [...this.caseNodeObservations],
      nodeAutonomy: 'NOT_TESTED_ORIGINAL_BROADCAST_RAW_ONLY', confirmationPolicy: await this.confirmationPolicy() };
    this.completed.push(result); this.publicRecord(`branch-${command.case}.json`, result); this.privateRecord('branch-results.jsonl', result);
    await this.rebuildC1();
    this.emit({ event: 'case-complete', id: command.id, ...result }); this.currentCase = undefined;
  }
  async keyTransaction(hash, action, priorSerial) {
    const receipt = await this.provider.getTransactionReceipt(hash), tx = await this.provider.getTransaction(hash), head = await this.latest();
    assert(tx && receipt?.status === 1 && receipt.blockNumber <= head.number - 1, 'key transaction is not successfully confirmed');
    sameHash(tx.to, this.deployment.escrow); sameHash(tx.from, this.wallets.R);
    const included = await this.provider.getBlock(receipt.blockNumber); sameHash(included.hash, receipt.blockHash);
    const parsed = this.escrow.interface.parseTransaction({ data: tx.data }); assert.equal(parsed.name, 'registerKey');
    const statement = await this.escrow.previewBinding(parsed.args[0]);
    const fields = artifact('DeliveryEscrow').abi.find(item => item.name === 'preview').inputs[0].components;
    const type = `tuple(${fields.map(field => `${field.type} ${field.name}`).join(',')})`;
    const { AbiCoder } = requireDependency('ethers');
    const journal = AbiCoder.defaultAbiCoder().encode([type], [statement]), journalHash = sha256(journal);
    assert.equal(statement.receiverPeer, this.terms.receiverPeer); assert.equal(statement.receiverKeySerial, priorSerial + 1n);
    assert.equal(statement.bindingAction, action === 'revoke' ? 2n : 1n); sameHash(statement.profileId, this.core.result.profileId);
    sameHash(statement.guestImageId, this.core.result.imageId);
    const adapter = new Contract(await this.escrow.adapter(), artifact('RiscZeroAdapter').abi, this.provider);
    assert.equal(await adapter.verify(parsed.args[1], journalHash), true, 'actual key binding seal rejected by production adapter');
    const registry = await this.escrow.keys(statement.receiverPeer);
    assert.equal(registry.serial, statement.receiverKeySerial); assert.equal(registry.bindingHash, statement.keyBindingHash);
    assert.equal(registry.active, action !== 'revoke');
    const publicEvents = receipt.logs.filter(log => log.address.toLowerCase() === this.deployment.escrow.toLowerCase())
      .map(log => this.escrow.interface.parseLog(log)).filter(log => log?.name === 'KeyUpdated');
    assert.equal(publicEvents.length, 1); assert.equal(publicEvents[0].args[0], statement.receiverPeer);
    assert.equal(publicEvents[0].args[1], registry.serial); assert.equal(publicEvents[0].args[2], registry.bindingHash);
    const raw = serializedPublicTransaction(tx); await this.retainBroadcast(hash, `real-key-${action}`);
    return { tx, receipt, raw, method: 'registerKey', timestamp: included.timestamp, statement,
      public: { case: action, status: 'PASS', serial: Number(registry.serial), bindingAction: Number(statement.bindingAction),
        bindingHash: registry.bindingHash, txHash: hash, registryConfirmed: true, helperNewProofs: 0, proofGenerationOwner: 'acceptance',
        imageId: this.core.result.imageId, profileId: this.core.result.profileId,
        proof: { relation: 1, journal: journal.slice(2), journalHash: journalHash.slice(2), seal: parsed.args[1].slice(2), verification: 'production-adapter' },
        blockNumber: receipt.blockNumber, blockHash: receipt.blockHash, gasUsed: receipt.gasUsed.toString(),
        executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString(), absoluteFinality: false } };
  }
  async lifecycleHeld(command) {
    assert(this.phase === 'restored' && !this.rotation && this.keyCases.length === 0, 'mixed-held Rotate follows core restoration exactly once');
    this.requestId = command.id; this.currentCase = 'mixed-held-rotate'; this.caseTransactions = []; this.caseNodeObservations = [];
    this.phase = 'verified'; this.index();
    interval(BigInt(Math.floor(Date.now() / 1000)) + 2n < this.terms.holdDeadline, 'real host certificate time no longer fits the original hold deadline; fresh proofs/order required');
    await this.resetC1();
    await this.checkpoint('restore-rotation-data', { nodeDataCheckpoint: this.core.nodeDataCheckpoint });
    await this.replay(this.core.records.find(record => record.method === 'hold' && record.index === 0));
    const ack = await this.checkpoint('rotate-ready', { keyRotation: true,
      expected: { action: 'rotate', priorSerial: this.terms.receiverKeySerial.toString(), nextSerial: (this.terms.receiverKeySerial + 1n).toString() } });
    const rotation = await this.keyTransaction(ack.nodeTransactions[0], 'rotate', this.terms.receiverKeySerial);
    this.rotation = { ...rotation, nodeDataCheckpoint: ack.quiesced.nodeDataCheckpoint };
    assert(ack.quiesced.nodeDataCheckpoint !== this.core.nodeDataCheckpoint, 'preserve a distinct whole AfterRotateData checkpoint including the new key material');
    interval(BigInt((await this.latest()).timestamp) + 2n <= this.terms.holdDeadline, 'Rotate completed after the unheld-key rejection interval');
    await this.keeper(this.escrow, 'hold', [this.orderId, 1, this.proof(3, 1)], 'State');
    await this.keeper(this.escrow, 'refund', [this.orderId, 1]);
    await this.publish(0); await this.open(0); await this.keeper(this.escrow, 'withdraw', [this.orderId]);
    await this.expect([3, 4], '12', '12', '17');
    const read = await this.checkpoint('mixed-held-result', { nodeDataCheckpoint: this.rotation.nodeDataCheckpoint });
    assert(read.nodeObservation === 'observed' && read.views?.R, 'mixed-held lifecycle requires an actual receiver projection');
    assert.equal(read.readSha256, this.core.fileSha256, 'mixed-held rotation requires actual old-key file recovery');
    this.rotation.nodeDataCheckpoint = read.quiesced.nodeDataCheckpoint;
    const result = { case: 'mixed-held-rotate', status: 'EVM_PASS', helperNewProofs: 0, newOwnerKeyBindings: 1,
      state: await this.state(), txHash: rotation.tx.hash, nodeObservations: [...this.caseNodeObservations], transactions: [...this.caseTransactions],
      oldFileSha256: read.readSha256, imageId: this.core.result.imageId, profileId: this.core.result.profileId,
      restoreRequired: true, nodeAutonomy: 'REAL_OWNER_ROTATE_ONLY_KEEPER_OLD_WINDOW_SETTLEMENT' };
    this.completed.push(result); this.publicRecord('branch-mixed-held-rotate.json', result);
    rotation.public.mixedHeld = 'PASS'; rotation.public.oldOrderReadSha256 = read.readSha256;
    this.keyCases.push(rotation.public); this.publicRecord('key-rotate.json', rotation.public);
    this.phase = 'verified';
    this.emit({ event: 'lifecycle-held-complete', id: command.id, ...result }); this.currentCase = undefined;
  }
  async recordKey(command) {
    assert(this.phase === 'restored', 'record key lifecycle only on the restored canonical chain'); await this.stopMiner();
    this.requestId = command.id; this.currentCase = `key-${command.action}`;
    if (command.action !== 'rotate') this.lifecycleCanonical = true;
    const expectedActions = ['rotate', 'revoke', 'register'];
    assert.equal(command.action, expectedActions[this.keyCases.length], 'key lifecycle must advance Rotate/Revoke/Register without rollback');
    const prior = this.keyCases.length ? BigInt(this.keyCases.at(-1).serial) : this.terms.receiverKeySerial;
    const record = await this.keyTransaction(command.txHash, command.action, prior);
    const ack = await this.checkpoint(`${command.action}-observed`, { nodeDataCheckpoint: command.quiesced.nodeDataCheckpoint });
    assert(ack.views?.R && ack.readSha256 === this.core.fileSha256, 'key lifecycle requires real receiver projection and old-key file read');
    record.public.oldOrderReadSha256 = ack.readSha256; record.public.mixedHeld = 'NOT_TESTED';
    if (command.action === 'rotate') this.rotation = { ...record, nodeDataCheckpoint: ack.quiesced.nodeDataCheckpoint };
    this.keyCases.push(record.public); this.publicRecord(`key-${command.action}.json`, record.public);
    this.emit({ event: 'key-recorded', id: command.id, ...record.public }); this.currentCase = undefined;
  }
  async restoreCore(command) {
    assert(this.core && !this.lifecycleCanonical, 'core restoration is forbidden after canonical Revoke/Register; preserve the new key history');
    this.requestId = command.id; this.currentCase = undefined;
    await this.resetC1(); await this.checkpoint('restore-main-data', { nodeDataCheckpoint: this.rotation?.nodeDataCheckpoint ?? this.core.nodeDataCheckpoint });
    const records = this.core.postFundRecords;
    for (let index = 0; index < records.length;) {
      const group = [], number = records[index].receipt.blockNumber;
      while (index < records.length && records[index].receipt.blockNumber === number) group.push(records[index++]);
      const parent = await this.latest(); interval(BigInt(group[0].timestamp) > BigInt(parent.timestamp), 'original mining timestamp no longer fits');
      for (const record of group) await this.replay(record, { confirm: false });
      await this.mineAt(BigInt(group[0].timestamp));
      for (const record of group) { const receipt = await this.provider.getTransactionReceipt(record.tx.hash); assert(receipt?.status === 1, 'original signed raw failed restoration'); }
    }
    const head = await this.latest(); await this.mineAt(BigInt(head.timestamp) + 1n);
    assert.deepEqual(await this.nonceFacts(), this.core.finalNonces, 'restoration changed an original nonce');
    if (this.rotation) {
      const receipt = await this.replay(this.rotation);
      Object.assign(this.rotation.public, { blockNumber: receipt.blockNumber, blockHash: receipt.blockHash,
        gasUsed: receipt.gasUsed.toString(), executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString() });
      this.publicRecord('key-rotate.json', this.rotation.public);
    }
    const result = await observe({ bundleFile: this.options.bundleFile, rpcUrl: this.options.rpcUrl });
    assert.equal(result.status, 'PASS', `restored original strict observer failed: ${result.error ?? ''}`);
    const restoredAck = await this.checkpoint('core-restored', { nodeDataCheckpoint: this.rotation?.nodeDataCheckpoint ?? this.core.nodeDataCheckpoint });
    assert(restoredAck.nodeObservation === 'observed' && restoredAck.views?.R && restoredAck.readSha256 === this.core.fileSha256,
      'canonical restoration requires actual receiver projection and original-file read');
    this.phase = 'restored';
    const restored = { event: 'core-restored', id: command.id, status: 'PASS', scope: 'original strict core only', originalHashesAndNonces: true,
      restoredAuthorizedRotation: this.rotation ? { txHash: this.rotation.tx.hash, serial: Number(this.rotation.statement.receiverKeySerial) } : null,
      branches: [...this.results.values()].map(value => ({ case: value.case, status: value.status, nodeAutonomy: value.nodeAutonomy ?? 'NOT_VERIFIED' })),
      imageId: result.imageId, profileId: result.profileId, confirmationPolicy: result.confirmationPolicy,
      state: await this.state(), oldFileSha256: restoredAck.readSha256, absoluteFinality: false };
    this.restoreResult = restored;
    this.publicRecord('core-restored.json', restored); this.emit(restored);
  }
  async dispatch(command) {
    validateCommand(command);
    assert(!this.requestIds.has(command.id), 'JSONL request ID reused'); this.requestIds.add(command.id);
    if (this.phase === 'failed') assert(['abort', 'restore-core'].includes(command.op), 'failed branch requires explicit restoration or abort');
    if (command.op === 'capture-pre-prepare') return this.capture('C0', command);
    if (command.op === 'capture-source-ready') return this.capture('C0Prime', command);
    if (command.op === 'capture-post-fund') return this.capture('C1', command);
    if (command.op === 'verify-core') return this.verifyCore(command);
    if (command.op === 'exercise-case') return this.exerciseCase(command);
    if (command.op === 'lifecycle-held') return this.lifecycleHeld(command);
    if (command.op === 'record-key') return this.recordKey(command);
    if (command.op === 'restore-core') return this.restoreCore(command);
    if (command.op === 'resume-main') {
      assert(['capture', 'restored'].includes(this.phase)); await this.startMiner();
      this.emit({ event: 'main-miner-resumed', id: command.id }); return;
    }
    await this.stopMiner(); this.phase = 'aborted';
    this.index();
    this.emit({ event: 'aborted', id: command.id, status: 'UNVERIFIED', chainRestored: false, signedRawBackupsPreserved: true });
  }
}

export function exerciseArguments(args) {
  assert.equal(args.length, 6, 'Usage: node exercise-settlement.mjs --settlement <future-main-bundle.json> --rpc-url <original-loopback-rpc> --deployment <public-deployment.json>');
  const values = {};
  for (let i = 0; i < args.length; i += 2) { assert(['--settlement', '--rpc-url', '--deployment'].includes(args[i]) && values[args[i]] === undefined); values[args[i]] = args[i + 1]; }
  assert(values['--deployment']);
  return { ...parseArguments(['--settlement', values['--settlement'], '--rpc-url', values['--rpc-url']]), deploymentFile: path.resolve(values['--deployment']) };
}

async function selfTest() {
  const { Interface, Wallet } = requireDependency('ethers');
  assert.equal(CASES.length, 23); assert.equal(new Set(REQUIRED_CASES).size, 24);
  assert.deepEqual(expectedCase('partial-refund'), { orderPresent: true, phases: [3, 4], earned: '12', withdrawn: '12', refunded: '17' });
  assert.deepEqual(expectedCase('late-priority'), { orderPresent: true, phases: [3, 3], earned: '24', withdrawn: '24', refunded: '5' });
  assert.deepEqual(expectedCase('incoming-false'), { orderPresent: false, phases: null, earned: '0', withdrawn: '0', refunded: '0', fundReceiptStatus: 0 });
  assert.equal(expectedCase('incoming-nonreturn').fundReceiptStatus, 1);
  assert.throws(() => expectedCase('unknown'));
  const quiesced = { nodesStopped: true, walletsLocked: true, proversIdle: true, nodeDataCheckpoint: 'C1Data' };
  const command = { id: 'case-one', op: 'exercise-case', quiesced, case: 'offline-refund' };
  assert.equal(validateCommand(command), command);
  for (const invalid of [{ ...command, case: 'unknown' }, { ...command, path: '/private/node' },
    { ...command, quiesced: { ...quiesced, proversIdle: false } }, { ...command, quiesced: { ...quiesced, nodeDataCheckpoint: '../node' } }]) {
    assert.throws(() => validateCommand(invalid));
  }
  const ack = { op: 'checkpoint-ack', checkpointId: 'case-one-result', quiesced, nodeObservation: 'not_observed', rawBroadcastDelta: 0 };
  assert.equal(validateAcknowledgement(ack, ack.checkpointId), ack);
  for (const invalid of [{ ...ack, rawBroadcastDelta: 1 }, { ...ack, views: {} }, { ...ack, raw: 'unbroadcast' }]) {
    assert.throws(() => validateAcknowledgement(invalid, ack.checkpointId));
  }
  assert.throws(() => validateAcknowledgement(ack, ack.checkpointId, 'refund'));
  assert.throws(() => validateAcknowledgement(ack, ack.checkpointId, 'rotate'));
  const contract = { interface: new Interface(['error State()', 'error Token()']) };
  const data = contract.interface.encodeErrorResult('State');
  assert.equal(callErrorName(contract, { code: 'CALL_EXCEPTION', data }), 'State');
  for (const error of [{ code: 'NETWORK_ERROR', data }, { code: 'CALL_EXCEPTION', data: '0x' },
    { code: 'CALL_EXCEPTION', data: '0x12345678' }, { code: 'CALL_EXCEPTION', data: null }]) assert.equal(callErrorName(contract, error), undefined);
  assert.equal(nextBaseFee({ baseFeePerGas: 1000n, gasLimit: 20n, gasUsed: 10n }), 1000n);
  assert.equal(nextBaseFee({ baseFeePerGas: 1000n, gasLimit: 20n, gasUsed: 0n }), 875n);
  assert.equal(nextBaseFee({ baseFeePerGas: 1n, gasLimit: 20n, gasUsed: 11n }), 2n);
  const wallet = new Wallet(`0x${'01'.repeat(32)}`);
  for (const type of [0, 1, 2]) {
    const signed = await wallet.signTransaction({ type, chainId: 31337, nonce: 0, gasLimit: 21000n, to: wallet.address, value: 0,
      ...(type === 2 ? { maxFeePerGas: 20n, maxPriorityFeePerGas: 1n } : { gasPrice: 20n }) });
    const decoded = Transaction.from(signed);
    const record = Object.fromEntries(['type', 'chainId', 'nonce', 'gasLimit', 'gasPrice', 'maxFeePerGas', 'maxPriorityFeePerGas',
      'accessList', 'to', 'value', 'data', 'signature', 'hash', 'from'].map(field => [field, decoded[field]]));
    assert.equal(serializedPublicTransaction(record), signed);
    assert.throws(() => serializedPublicTransaction({ ...record, nonce: 1 }));
  }
  const fixture = { latest: async () => ({ number: 12 }), provider: { getBlock: async number => ({ number, hash: `0x${'02'.repeat(32)}` }) },
    caseTransactions: [{ blockNumber: 11 }] };
  assert.deepEqual(await Exercise.prototype.confirmationPolicy.call(fixture), { kind: 'experimental-block-depth', confirmations: 2,
    observedTip: 12, anchorNumber: 11, anchorHash: `0x${'02'.repeat(32)}`, absoluteFinality: false });
  fixture.caseTransactions[0].blockNumber = 12;
  await assert.rejects(() => Exercise.prototype.confirmationPolicy.call(fixture));
  const args = ['--settlement', '/future/settlement.json', '--rpc-url', 'http://127.0.0.1:8545', '--deployment', '/future/deployment.json'];
  assert.equal(exerciseArguments(args).caseName, 'strict');
  assert.throws(() => exerciseArguments(args.map(value => value === 'http://127.0.0.1:8545' ? 'https://example.com' : value)));
  assert.throws(() => exerciseArguments([...args, '--command', 'arbitrary']));
  const hash = value => `0x${value.toString(16).padStart(64, '0')}`;
  const ordinary = { schemaVersion: 1, escrow: `0x${'11'.repeat(20)}`, asset: `0x${'22'.repeat(20)}`, orderId: hash(1),
    proofs: [1, 2, 3, 4].map(relation => ({ relation, ...(relation === 1 ? {} : { windowIndex: 0 }),
      public: `relation-${relation}.json`, receipt: `relation-${relation}.receipt` })),
    capsules: [{ windowIndex: 0, capsule: '00'.repeat(80) }], transactions: { registerKey: hash(2), fund: hash(3),
      hold: [hash(4)], publish: [hash(5)], open: [hash(6)], withdraw: [hash(7)] } };
  assert.equal(validateBundle(ordinary, 'ordinary'), ordinary);
  assert.throws(() => validateBundle(ordinary));
  const wrongWindow = structuredClone(ordinary); wrongWindow.proofs[1].windowIndex = 1;
  assert.throws(() => validateBundle(wrongWindow, 'ordinary'));
  const wrongPath = structuredClone(ordinary); wrongPath.proofs[0].receipt = '../private.receipt';
  assert.throws(() => validateBundle(wrongPath, 'ordinary'));
  console.log('PASS finite commands, acknowledgements, error attribution, public transaction hashes and confirmation policies; no chain, Node or proof ran');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href && process.argv[2] === '--self-test') {
  assert.equal(process.argv.length, 3); await selfTest();
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href && process.argv[2] !== '--self-test') {
  const iterator = readline.createInterface({ input: process.stdin, crlfDelay: Infinity })[Symbol.asyncIterator]();
  const emit = value => process.stdout.write(`${JSON.stringify({ schemaVersion: 1, ...value })}\n`);
  let exercise;
  try {
    exercise = new Exercise(exerciseArguments(process.argv.slice(2)), iterator, emit); await exercise.initialize();
    for (;;) {
      const line = await iterator.next(); if (line.done) break;
      assert(Buffer.byteLength(line.value) <= 65536, 'JSONL command exceeds bound');
      try { await exercise.dispatch(JSON.parse(line.value)); }
      catch (error) {
        exercise.phase = 'failed'; await exercise.stopMiner();
        const failure = { event: 'error', case: exercise.currentCase, status: error instanceof Unverified ? 'UNVERIFIED' : 'FAIL',
          error: publicError(error), chainRestored: false, signedRawBackupsPreserved: true };
        if (exercise.currentCase) exercise.publicRecord(`branch-${exercise.currentCase}.json`, { ...failure, transactions: exercise.caseTransactions ?? [] });
        emit(failure);
      }
      if (exercise.phase === 'aborted') break;
    }
    if (!exercise.mining) await exercise.stopMiner();
    const complete = exercise.phase === 'restored';
    emit({ event: 'session-end', status: complete ? 'CORE_RESTORED' : 'UNVERIFIED', phase: exercise.phase,
      originalCoreRestored: complete, signedRawBackupsPreserved: true });
    if (!complete) process.exitCode = 1;
  } catch (error) { emit({ event: 'error', status: 'FAIL', error: publicError(error), chainRestored: false }); process.exitCode = 1; }
  finally { if (exercise?.provider) await exercise.provider.destroy(); }
}
