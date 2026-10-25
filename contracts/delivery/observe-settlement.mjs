import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { compile, artifact, artifacts, requireDependency, root, schema } from './compile.mjs';

const {
  AbiCoder, Contract, JsonRpcProvider, getAddress, getBytes, hexlify, recoverAddress, sha256, toUtf8Bytes,
} = requireDependency('ethers');
const coder = AbiCoder.defaultAbiCoder();
const CONFIRMATIONS = 2;
const HASH = /^0x[0-9a-fA-F]{64}$/;
const ADDRESS = /^0x[0-9a-fA-F]{40}$/;

class Unverified extends Error {}

export function observationCase(name = 'strict') {
  assert(['strict', 'ordinary'].includes(name), 'unknown observation case');
  return name === 'strict' ? { name, windows: 2, proofs: 7, acceptance: 1n, total: 29n }
    : { name, windows: 1, proofs: 4, acceptance: 0n, total: 17n };
}

function keys(value, required, optional = []) {
  assert(value && typeof value === 'object' && !Array.isArray(value), 'expected an object');
  for (const key of required) assert(Object.hasOwn(value, key), `missing ${key}`);
  for (const key of Object.keys(value)) assert([...required, ...optional].includes(key), `unexpected ${key}`);
}

function bareHex(value, bytes, label) {
  assert(typeof value === 'string' && /^[0-9a-f]+$/.test(value) && value.length % 2 === 0, `${label} must be lowercase hex without 0x`);
  if (bytes !== undefined) assert.equal(value.length, bytes * 2, label);
  return `0x${value}`;
}

function filename(value) {
  assert(typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/.test(value), 'proof filenames must be relative basenames');
}

export function validateBundle(bundle, caseName = 'strict') {
  const selected = observationCase(caseName), indexes = Array.from({ length: selected.windows }, (_, index) => index);
  keys(bundle, ['schemaVersion', 'escrow', 'asset', 'orderId', 'proofs', 'capsules', 'transactions']);
  assert.equal(bundle.schemaVersion, 1);
  assert(ADDRESS.test(bundle.escrow) && ADDRESS.test(bundle.asset), '0x-prefixed contract addresses required');
  assert(HASH.test(bundle.orderId), '0x-prefixed order ID required');
  assert(Array.isArray(bundle.proofs) && bundle.proofs.length === selected.proofs, `exactly ${selected.proofs} proofs required for ${caseName}`);
  const seen = new Set(), files = new Set();
  for (const proof of bundle.proofs) {
    keys(proof, proof.relation === 1 ? ['relation', 'public', 'receipt'] : ['relation', 'windowIndex', 'public', 'receipt']);
    assert([1, 2, 3, 4].includes(proof.relation), 'unknown proof relation');
    if (proof.relation !== 1) assert(indexes.includes(proof.windowIndex), 'window index outside the selected case');
    const id = `${proof.relation}:${proof.windowIndex ?? 'key'}`;
    assert(!seen.has(id), `duplicate proof ${id}`); seen.add(id);
    for (const name of [proof.public, proof.receipt]) { filename(name); assert(!files.has(name), 'duplicate proof filename'); files.add(name); }
  }
  for (const id of ['1:key', ...indexes.flatMap(index => [2, 3, 4].map(relation => `${relation}:${index}`))]) assert(seen.has(id), `missing proof ${id}`);
  assert(Array.isArray(bundle.capsules) && bundle.capsules.length === selected.windows, 'one public capsule per selected window required');
  const capsuleIndexes = new Set();
  for (const item of bundle.capsules) {
    keys(item, ['windowIndex', 'capsule']); assert(indexes.includes(item.windowIndex));
    assert(!capsuleIndexes.has(item.windowIndex), 'duplicate capsule'); capsuleIndexes.add(item.windowIndex);
    bareHex(item.capsule, 80, 'capsule');
  }
  keys(bundle.transactions, ['registerKey', 'fund', 'hold', 'publish', 'open', 'withdraw'], ['approve']);
  const hashes = [bundle.transactions.registerKey, bundle.transactions.fund];
  if (bundle.transactions.approve !== undefined) hashes.push(bundle.transactions.approve);
  for (const name of ['hold', 'publish', 'open']) {
    assert(Array.isArray(bundle.transactions[name]) && bundle.transactions[name].length === selected.windows, `${name} requires one transaction per window`);
    hashes.push(...bundle.transactions[name]);
  }
  assert(Array.isArray(bundle.transactions.withdraw) && bundle.transactions.withdraw.length >= 1 && bundle.transactions.withdraw.length <= selected.windows,
    'withdraw requires the actual transaction hashes within the selected window count');
  hashes.push(...bundle.transactions.withdraw);
  const unique = new Set();
  for (const hash of hashes) { assert(typeof hash === 'string' && HASH.test(hash), 'invalid transaction hash'); assert(!unique.has(hash.toLowerCase()), 'transaction hash reused'); unique.add(hash.toLowerCase()); }
  return bundle;
}

export function checkRuntime(entry, code) {
  keys(entry, ['runtime', 'immutable_ranges', 'immutable_groups']);
  const template = getBytes(entry.runtime), actual = getBytes(code);
  assert.equal(actual.length, template.length, 'runtime length mismatch');
  assert(Array.isArray(entry.immutable_groups) && Array.isArray(entry.immutable_ranges), 'immutable metadata missing');
  const ranges = [];
  for (const group of entry.immutable_groups) {
    assert(Array.isArray(group) && group.length > 0, 'empty immutable group');
    let expected;
    for (const range of group) {
      keys(range, ['start', 'length']);
      assert(Number.isSafeInteger(range.start) && range.start >= 0 && range.length === 32 && range.start + range.length <= actual.length,
        'immutable range out of bounds');
      const value = hexlify(actual.slice(range.start, range.start + range.length));
      if (expected !== undefined) assert.equal(value, expected, 'inconsistent references to the same immutable');
      expected = value; ranges.push(range);
    }
  }
  ranges.sort((a, b) => a.start - b.start);
  assert.deepEqual(ranges, entry.immutable_ranges, 'immutable groups and ranges differ');
  let previousEnd = 0;
  for (const range of ranges) {
    assert(range.start >= previousEnd, 'overlapping immutable ranges'); previousEnd = range.start + range.length;
    assert(template.slice(range.start, previousEnd).every(byte => byte === 0), 'template immutable must be zero');
    actual.fill(0, range.start, previousEnd);
  }
  assert.equal(hexlify(actual), hexlify(template), 'runtime template mismatch');
}

export function parseArguments(args) {
  assert([4, 6].includes(args.length), 'Usage: node observe-settlement.mjs --settlement <public-bundle.json> --rpc-url <original-local-rpc> [--case ordinary]');
  const values = {};
  for (let i = 0; i < args.length; i += 2) {
    assert(['--settlement', '--rpc-url', '--case'].includes(args[i]) && values[args[i]] === undefined, 'unknown or duplicate argument');
    values[args[i]] = args[i + 1];
  }
  assert(values['--settlement'] && values['--rpc-url'], 'both arguments are required');
  const url = new URL(values['--rpc-url']);
  assert(url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname) && !url.username && !url.password,
    'only the original loopback HTTP RPC is supported');
  const caseName = observationCase(values['--case']).name;
  return { bundleFile: path.resolve(values['--settlement']), rpcUrl: url.href, caseName };
}

function readRegular(file) {
  const stat = fs.lstatSync(file);
  assert(stat.isFile() && stat.size > 0 && stat.size <= 1048576, 'expected a nonempty regular public file of at most 1 MiB');
  return fs.readFileSync(file);
}

function sameAddress(a, b) { assert.equal(getAddress(a), getAddress(b), 'address mismatch'); }
function sameHex(a, b, message) { assert.equal(a.toLowerCase(), b.toLowerCase(), message); }
function before(a, b) { return a.receipt.blockNumber < b.receipt.blockNumber || (a.receipt.blockNumber === b.receipt.blockNumber && a.receipt.index < b.receipt.index); }

export async function observe({ bundleFile, rpcUrl, caseName = 'strict' }) {
  const report = { schemaVersion: 1, status: 'UNVERIFIED', case: caseName, scope: 'read-only observation of genuine local delivery settlement', checks: [], transactions: [], proofs: [] };
  let provider;
  const passed = name => report.checks.push({ name, passed: true });
  try {
    const selected = observationCase(caseName);
    const bundle = validateBundle(JSON.parse(readRegular(bundleFile)), caseName);
    report.orderId = bundle.orderId; report.escrow = bundle.escrow; report.asset = bundle.asset;
    compile({ checkTrust: true });
    const escrowArtifact = artifact('DeliveryEscrow');
    const fields = escrowArtifact.abi.find(item => item.name === 'preview').inputs[0].components;
    assert.equal(fields.length, 47); assert.equal(fields[42].name, 'priorityDeadline');
    const statementType = `tuple(${fields.map(field => `${field.type} ${field.name}`).join(',')})`;
    const object = value => Object.fromEntries(fields.map(field => [field.name, value[field.name]]));
    const statementBytes = value => coder.encode([statementType], [object(value)]);
    const profileFields = escrowArtifact.abi.find(item => item.type === 'constructor').inputs[0].components;
    const profileType = `tuple(${profileFields.map(field => `${field.type} ${field.name}`).join(',')})`;
    const canonical = JSON.parse(readRegular(path.resolve(root, '../../proofs/delivery/profile.json')));
    keys(canonical, ['guest_image_id', 'statement_schema_hash', 'crypto_suite', 'max_windows', 'max_window_bytes', 'max_total_bytes', 'max_guest_cycles', 'max_prover_memory_bytes']);
    const word = value => {
      assert(Array.isArray(value) && value.length === 32 && value.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255), 'invalid canonical profile Word');
      return hexlify(Uint8Array.from(value));
    };
    for (const name of ['crypto_suite', 'max_windows', 'max_window_bytes', 'max_total_bytes', 'max_guest_cycles', 'max_prover_memory_bytes']) {
      assert(Number.isSafeInteger(canonical[name]) && canonical[name] > 0, `invalid profile ${name}`);
    }
    const profile = { domain: sha256(toUtf8Bytes('vibeface-delivery-profile-v1')), guestImageId: word(canonical.guest_image_id),
      statementSchemaHash: word(canonical.statement_schema_hash), cryptoSuite: canonical.crypto_suite, maxWindows: canonical.max_windows,
      maxWindowBytes: canonical.max_window_bytes, maxTotalBytes: canonical.max_total_bytes, maxGuestCycles: canonical.max_guest_cycles,
      maxProverMemoryBytes: canonical.max_prover_memory_bytes };
    assert.equal(profile.statementSchemaHash, sha256(toUtf8Bytes(schema)), 'canonical profile schema mismatch');
    const profileBytes = coder.encode([profileType], [profile]);
    const profileId = sha256(profileBytes);
    report.imageId = profile.guestImageId; report.profileId = profileId;
    const proofDirectory = path.dirname(bundleFile), proofs = new Map();
    for (const entry of bundle.proofs) {
      const metadata = JSON.parse(readRegular(path.join(proofDirectory, entry.public)));
      keys(metadata, ['mode', 'kind', 'imageId', 'profileId', 'relation', 'journal', 'journalHash', 'seal']);
      assert.equal(metadata.mode, 'prove'); assert.equal(metadata.kind, 'groth16'); assert.equal(metadata.relation, entry.relation);
      assert.equal(bareHex(metadata.imageId, 32, 'image ID'), profile.guestImageId, 'proof image differs from fixed product image');
      assert.equal(bareHex(metadata.profileId, 32, 'profile ID'), profileId, 'proof profile differs from fixed product profile');
      const journal = bareHex(metadata.journal, 1504, 'journal'), seal = bareHex(metadata.seal, undefined, 'seal');
      assert(getBytes(seal).length > 4, 'missing Groth16 seal');
      const digest = bareHex(metadata.journalHash, 32, 'journal hash'); assert.equal(sha256(journal), digest);
      const statement = coder.decode([statementType], journal)[0]; assert.equal(statementBytes(statement), journal, 'noncanonical journal');
      assert.equal(statement.domain, sha256(toUtf8Bytes('vibeface-delivery-statement-v1')));
      assert.equal(statement.chainId, 31337n); sameAddress(statement.escrow, bundle.escrow);
      assert.equal(statement.relation, BigInt(entry.relation)); assert.equal(statement.profileId, profileId); assert.equal(statement.guestImageId, profile.guestImageId);
      if (entry.relation !== 1) {
        sameHex(statement.orderId, bundle.orderId); assert.equal(statement.windowIndex, BigInt(entry.windowIndex));
        assert.equal(statement.windowCount, BigInt(selected.windows)); assert(statement.windowCount <= BigInt(profile.maxWindows));
        assert.equal(statement.acceptance, selected.acceptance, 'acceptance mode differs from selected case');
        assert.equal(statement.cryptoSuite, BigInt(profile.cryptoSuite));
        assert(statement.windowPlaintextBytes <= BigInt(profile.maxWindowBytes) && statement.totalPlaintextBytes <= BigInt(profile.maxTotalBytes));
      }
      const binary = readRegular(path.join(proofDirectory, entry.receipt));
      const id = `${entry.relation}:${entry.windowIndex ?? 'key'}`;
      proofs.set(id, { ...entry, metadata, journal, seal, digest, statement });
      report.proofs.push({ relation: entry.relation, windowIndex: entry.windowIndex ?? null, journalHash: digest,
        receiptSha256: sha256(binary), binaryReceiptBinding: 'not checked here; the host must separately verify the indexed binary receipt; this observer verifies the public EVM seal' });
    }
    passed(`${selected.proofs} complete public proofs match the fixed product image and profile`);
    provider = new JsonRpcProvider(rpcUrl, undefined, { cacheTimeout: -1 });
    assert.equal((await provider.getNetwork()).chainId, 31337n, 'wrong chain');
    const tip = await provider.getBlockNumber(), anchorNumber = tip - (CONFIRMATIONS - 1);
    if (anchorNumber < 0) throw new Unverified('not enough blocks for the fixed experimental confirmation policy');
    const anchor = await provider.getBlock(anchorNumber);
    if (!anchor) throw new Unverified('confirmation anchor unavailable');
    const at = { blockTag: anchorNumber };
    report.confirmationPolicy = { kind: 'experimental-block-depth', confirmations: CONFIRMATIONS, observedTip: tip,
      anchorNumber, anchorHash: anchor.hash, absoluteFinality: false };
    const trust = JSON.parse(readRegular(path.join(root, 'trust.json')));
    assert.equal(trust.schemaVersion, 1);
    const escrow = new Contract(bundle.escrow, escrowArtifact.abi, provider);
    const token = new Contract(bundle.asset, artifact('TestToken').abi, provider);
    const trustedCode = async (name, address) => {
      checkRuntime(trust.contracts[name], await provider.getCode(address, anchorNumber));
      passed(`${name} runtime and immutable reference groups match product trust`);
    };
    await trustedCode('DeliveryEscrow', bundle.escrow); await trustedCode('TestToken', bundle.asset);
    assert.equal(await escrow.profileId(at), profileId);
    assert.equal(coder.encode([profileType], [await escrow.getProfile(at)]), profileBytes, 'deployed profile body mismatch');
    const adapterAddress = await escrow.adapter(at);
    const adapter = new Contract(adapterAddress, artifact('RiscZeroAdapter').abi, provider);
    await trustedCode('RiscZeroAdapter', adapterAddress);
    assert.equal(await adapter.imageId(at), profile.guestImageId);
    const verifierAddress = await adapter.verifier(at);
    await trustedCode('RiscZeroGroth16Verifier', verifierAddress);
    const verifier = new Contract(verifierAddress, artifact('RiscZeroGroth16Verifier').abi, provider);
    const control = getBytes(await adapter.CONTROL_ROOT(at));
    assert.equal(await verifier.CONTROL_ROOT_0(at), hexlify(control.slice(0, 16).reverse()));
    assert.equal(await verifier.CONTROL_ROOT_1(at), hexlify(control.slice(16).reverse()));
    assert.equal(await verifier.BN254_CONTROL_ID(at), await adapter.BN254_CONTROL_ID(at));
    assert.equal(await verifier.SELECTOR(at), await adapter.SELECTOR(at));
    report.adapter = adapterAddress; report.verifier = verifierAddress;
    const order = await escrow.getOrder(bundle.orderId, at);
    sameAddress(order.terms.asset, bundle.asset); assert.equal(order.terms.windowCount, BigInt(selected.windows)); assert.equal(order.windows.length, selected.windows);
    assert.equal(order.terms.acceptance, selected.acceptance); assert.equal(order.terms.escrowAmount, selected.total);
    assert.equal(order.terms.wholeOrderBonus, 5n);
    if (caseName === 'ordinary') assert.equal(order.terms.totalPlaintextBytes, 4096n + 6n + BigInt(Buffer.byteLength('ordinary-paid-ui.bin')), 'ordinary UI sample must contain the expected 4096-byte file envelope');
    sameHex(order.terms.orderId, bundle.orderId);
    const key = proofs.get('1:key');
    assert.equal(key.statement.bindingAction, 1n);
    if (caseName === 'ordinary') assert.equal(key.statement.receiverKeySerial, 4n, 'ordinary supplement must use the final active Register4 binding');
    assert.equal(statementBytes(await escrow.previewBinding(object(key.statement), at)), key.journal, 'key statement reconstruction mismatch');
    assert.equal(key.statement.keyBindingHash, order.terms.keyBindingHash);
    const registry = await escrow.keys(key.statement.receiverPeer, at);
    assert(registry.serial >= key.statement.receiverKeySerial, 'registry serial regressed');
    if (registry.serial === key.statement.receiverKeySerial) { assert.equal(registry.active, true); assert.equal(registry.bindingHash, key.statement.keyBindingHash); }
    const windows = [];
    for (let index = 0; index < selected.windows; index++) {
      windows.push(await escrow.getWindow(bundle.orderId, index, at));
      assert.equal(windows[index].spec.deliveryFee, 10n); assert.equal(windows[index].spec.reservationFee, 2n);
      for (const relation of [2, 3, 4]) {
        const proof = proofs.get(`${relation}:${index}`);
        assert.equal(statementBytes(await escrow.publicValues(bundle.orderId, index, relation, at)), proof.journal,
          `contract publicValues mismatch for relation ${relation} window ${index}`);
      }
    }
    report.totalPlaintextBytes = order.terms.totalPlaintextBytes.toString();
    report.windows = windows.map(window => ({ index: Number(window.spec.index), offset: window.spec.offset.toString(),
      plaintextBytes: window.spec.plaintextBytes.toString(), ciphertextBytes: window.spec.ciphertextBytes.toString(),
      deliveryFee: window.spec.deliveryFee.toString(), reservationFee: window.spec.reservationFee.toString(),
      phase: Number(window.phase), decryptionProven: window.decryptionProven, retainedCapsuleBytes: getBytes(window.capsule).length }));
    for (const proof of proofs.values()) {
      assert.equal(await adapter.verify(proof.seal, proof.digest, at), true, `genuine seal rejected for relation ${proof.relation} window ${proof.windowIndex ?? 'key'}`);
    }
    passed(`all ${selected.proofs} genuine seals verify and journals match contract-constructed public values`);
    const blockCache = new Map();
    const block = async number => {
      if (!blockCache.has(number)) {
        const value = await provider.getBlock(number);
        if (!value) throw new Unverified(`block ${number} unavailable`);
        blockCache.set(number, value);
      }
      return blockCache.get(number);
    };
    const records = [];
    const transaction = async (hash, contract, method, label) => {
      const receipt = await provider.getTransactionReceipt(hash);
      if (!receipt) throw new Unverified(`${label} receipt missing or pending`);
      assert.equal(receipt.status, 1, `${label} reverted`);
      if (receipt.blockNumber > anchorNumber) throw new Unverified(`${label} has fewer than ${CONFIRMATIONS} experimental confirmations`);
      const included = await block(receipt.blockNumber);
      if (included.hash !== receipt.blockHash) throw new Unverified(`${label} receipt is not canonical`);
      const tx = await provider.getTransaction(hash);
      if (!tx || tx.blockHash !== receipt.blockHash || tx.blockNumber !== receipt.blockNumber) throw new Unverified(`${label} transaction inclusion unavailable`);
      sameAddress(tx.to, await contract.getAddress()); assert.equal(tx.chainId, 31337n); assert.equal(tx.value, 0n);
      const parsed = contract.interface.parseTransaction({ data: tx.data, value: tx.value });
      assert(parsed && parsed.name === method, `${label} calldata method mismatch`);
      sameHex(contract.interface.encodeFunctionData(parsed.fragment, parsed.args), tx.data, `${label} noncanonical calldata`);
      if (typeof receipt.gasPrice !== 'bigint' || typeof receipt.gasUsed !== 'bigint') throw new Unverified(`${label} effective gas price unavailable`);
      const record = { receipt, tx, parsed, label, timestamp: BigInt(included.timestamp) };
      records.push(record);
      report.transactions.push({ label, hash: receipt.hash, blockNumber: receipt.blockNumber, blockHash: receipt.blockHash,
        from: tx.from, to: tx.to, method, gasUsed: receipt.gasUsed.toString(), effectiveGasPrice: receipt.gasPrice.toString(),
        executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString() });
      return record;
    };
    const event = (record, name) => {
      const matching = record.receipt.logs.filter(log => log.address.toLowerCase() === bundle.escrow.toLowerCase())
        .map(log => escrow.interface.parseLog(log)).filter(log => log?.name === name);
      assert.equal(matching.length, 1, `${record.label} expected exactly one ${name} event`);
      return matching[0].args;
    };
    const windowEvent = (record, index, phase) => {
      const args = event(record, 'WindowUpdated'); sameHex(args[0], bundle.orderId); assert.equal(args[1], BigInt(index)); assert.equal(args[2], phase);
    };
    const registered = await transaction(bundle.transactions.registerKey, escrow, 'registerKey', 'registerKey');
    assert.equal(statementBytes(await escrow.previewBinding(registered.parsed.args[0], at)), key.journal);
    sameHex(registered.parsed.args[1], key.seal);
    const keyEvent = event(registered, 'KeyUpdated');
    assert.equal(keyEvent[0], key.statement.receiverPeer); assert.equal(keyEvent[1], key.statement.receiverKeySerial);
    assert.equal(keyEvent[2], key.statement.keyBindingHash); assert.equal(keyEvent[3], true);
    let approval;
    if (bundle.transactions.approve) {
      approval = await transaction(bundle.transactions.approve, token, 'approve', 'approve');
      sameAddress(approval.tx.from, order.terms.payer); sameAddress(approval.parsed.args[0], bundle.escrow);
      assert(approval.parsed.args[1] >= order.terms.escrowAmount, 'approval does not cover funded amount');
    }
    const funded = await transaction(bundle.transactions.fund, escrow, 'fund', 'fund');
    assert(before(registered, funded), 'fund preceded key registration');
    if (approval) assert(before(approval, funded), 'fund preceded listed approval');
    sameAddress(funded.tx.from, order.terms.payer);
    const sourceStatements = await escrow.preview(funded.parsed.args[0], funded.parsed.args[1], at);
    assert.equal(funded.parsed.args[2].length, selected.windows);
    for (let index = 0; index < selected.windows; index++) {
      assert.equal(statementBytes(sourceStatements[index]), proofs.get(`2:${index}`).journal);
      sameHex(funded.parsed.args[2][index], proofs.get(`2:${index}`).seal);
    }
    sameAddress(recoverAddress(await escrow.quoteWalletDigest(order.terms.quoteDigest, order.terms.providerPeer, at), funded.parsed.args[3]), order.terms.providerWallet);
    const fundingEvent = event(funded, 'Funded'); sameHex(fundingEvent[0], bundle.orderId);
    sameAddress(fundingEvent[1], order.terms.payer); assert.equal(fundingEvent[2], order.terms.escrowAmount);
    assert(funded.timestamp <= order.terms.quoteExpiresAt && funded.timestamp >= order.terms.keyValidFrom);
    const capsules = new Map(bundle.capsules.map(item => [item.windowIndex, bareHex(item.capsule, 80, 'capsule')]));
    const completions = [];
    for (let index = 0; index < selected.windows; index++) {
      const held = await transaction(bundle.transactions.hold[index], escrow, 'hold', `hold-${index}`);
      const published = await transaction(bundle.transactions.publish[index], escrow, 'publish', `publish-${index}`);
      const opened = await transaction(bundle.transactions.open[index], escrow, 'open', `open-${index}`);
      assert(before(funded, held) && before(held, published) && before(published, opened), 'window transaction order mismatch');
      for (const record of [held, published, opened]) { sameHex(record.parsed.args[0], bundle.orderId); assert.equal(record.parsed.args[1], BigInt(index)); }
      sameHex(held.parsed.args[2], proofs.get(`3:${index}`).seal); sameHex(opened.parsed.args[2], proofs.get(`4:${index}`).seal);
      sameHex(published.parsed.args[2], capsules.get(index)); assert.equal(sha256(capsules.get(index)), windows[index].spec.capsuleHash);
      sameHex(windows[index].capsule, capsules.get(index)); assert.equal(windows[index].phase, 3n); assert.equal(windows[index].decryptionProven, true);
      windowEvent(held, index, 1n); windowEvent(published, index, order.terms.acceptance === 0n ? 3n : 2n); windowEvent(opened, index, 3n);
      const evidence = event(opened, 'DecryptionProven'); sameHex(evidence[0], bundle.orderId); assert.equal(evidence[1], BigInt(index));
      assert(held.timestamp <= order.terms.holdDeadline && published.timestamp <= order.terms.keyDeadline);
      if (order.terms.acceptance === 1n) assert(opened.timestamp <= order.terms.openDeadline);
      completions.push(order.terms.acceptance === 0n ? published : opened);
    }
    const withdrawals = [];
    let withdrawn = 0n;
    for (let index = 0; index < bundle.transactions.withdraw.length; index++) {
      const record = await transaction(bundle.transactions.withdraw[index], escrow, 'withdraw', `withdraw-${index}`);
      if (index > 0) assert(before(withdrawals[index - 1], record), 'withdrawal hashes are not in execution order');
      sameHex(record.parsed.args[0], bundle.orderId);
      const args = event(record, 'Withdrawn'); sameHex(args[0], bundle.orderId); sameAddress(args[1], order.terms.providerWallet);
      assert(args[2] > 0n); withdrawn += args[2];
      assert(completions.some(completion => before(completion, record)), 'withdrawal preceded earning');
      withdrawals.push({ ...record, amount: args[2] });
    }
    const fees = windows.reduce((sum, window) => sum + window.spec.deliveryFee + window.spec.reservationFee, 0n);
    const timely = completions.every(completion => completion.timestamp <= order.terms.priorityDeadline);
    const expectedEarned = fees + (timely ? order.terms.wholeOrderBonus : 0n);
    assert.equal(order.earnedWindows, BigInt(selected.windows)); assert.equal(order.refundedWindows, 0n); assert.equal(order.refunded, 0n);
    assert.equal(order.earned, expectedEarned); assert.equal(order.withdrawn, expectedEarned); assert.equal(withdrawn, expectedEarned);
    assert.equal(order.bonusEarned, timely);
    if (!timely && order.terms.wholeOrderBonus > 0n) throw new Unverified('all window fees were earned, but the missed priority bonus is outside this complete-flow bundle');
    assert.equal(order.terms.escrowAmount - order.withdrawn - order.refunded, 0n, 'order has unsettled funds');
    passed('actual calldata, successful receipts, events, retained capsules and all selected terminal windows match the complete order');
    const accounts = [...new Set([order.terms.payer, order.terms.providerWallet, bundle.escrow].map(getAddress))];
    const balance = async (address, number) => {
      try { return await token.balanceOf(address, { blockTag: number }); }
      catch { throw new Unverified(`historical token balance unavailable at block ${number}`); }
    };
    const isolated = async record => {
      const included = await block(record.receipt.blockNumber);
      if (included.transactions.length !== 1 || included.transactions[0].toLowerCase() !== record.receipt.hash.toLowerCase()) {
        throw new Unverified(`${record.label} shares a block; this token has no Transfer events to isolate receipt-level balances`);
      }
    };
    const movements = [];
    const expectedNet = new Map(accounts.map(address => [address, 0n]));
    const verifyMovement = async (record, from, to, amount) => {
      await isolated(record);
      const number = record.receipt.blockNumber;
      if (number === 0) throw new Unverified('pre-transaction balance block unavailable');
      const changes = new Map(accounts.map(address => [address, 0n]));
      changes.set(getAddress(from), changes.get(getAddress(from)) - amount);
      changes.set(getAddress(to), changes.get(getAddress(to)) + amount);
      const rows = [];
      for (const address of accounts) {
        const beforeBalance = await balance(address, number - 1), afterBalance = await balance(address, number);
        assert.equal(afterBalance - beforeBalance, changes.get(address), `${record.label} actual token delta mismatch for ${address}`);
        expectedNet.set(address, expectedNet.get(address) + changes.get(address));
        rows.push({ address, before: beforeBalance.toString(), after: afterBalance.toString(), delta: changes.get(address).toString() });
      }
      movements.push({ label: record.label, blockNumber: number, accounts: rows });
    };
    await verifyMovement(funded, order.terms.payer, bundle.escrow, order.terms.escrowAmount);
    for (const record of withdrawals) await verifyMovement(record, bundle.escrow, order.terms.providerWallet, record.amount);
    if (approval) {
      await isolated(approval);
      let allowance;
      try { allowance = await token.allowance(order.terms.payer, bundle.escrow, { blockTag: approval.receipt.blockNumber }); }
      catch { throw new Unverified('historical approval allowance unavailable'); }
      assert.equal(allowance, approval.parsed.args[1]);
    }
    const aggregate = [];
    for (const address of accounts) {
      const beforeBalance = await balance(address, funded.receipt.blockNumber - 1), afterBalance = await balance(address, anchorNumber);
      if (afterBalance - beforeBalance !== expectedNet.get(address)) throw new Unverified(`unattributed token movement for ${address} across the observation interval`);
      aggregate.push({ address, before: beforeBalance.toString(), after: afterBalance.toString(), delta: expectedNet.get(address).toString() });
    }
    report.money = { asset: bundle.asset, payer: order.terms.payer, provider: order.terms.providerWallet,
      funded: order.terms.escrowAmount.toString(), earned: order.earned.toString(), withdrawn: order.withdrawn.toString(),
      refunded: order.refunded.toString(), perTransaction: movements, aggregate };
    passed('historical payer, provider and escrow token balances independently confirm exact receipt and payment amounts');
    const gasBySender = new Map();
    for (const record of records) {
      const sender = getAddress(record.tx.from), fee = record.receipt.gasUsed * record.receipt.gasPrice;
      gasBySender.set(sender, (gasBySender.get(sender) ?? 0n) + fee);
    }
    report.gas = { scope: 'listed local EVM operations only; excludes deployments, faucets, proving costs and any unlisted pre-existing approval',
      includesListedApproval: Boolean(approval), executionGasUsed: records.reduce((sum, record) => sum + record.receipt.gasUsed, 0n).toString(),
      executionGasFeeWei: [...gasBySender.values()].reduce((sum, value) => sum + value, 0n).toString(),
      bySender: Object.fromEntries([...gasBySender].map(([address, fee]) => [address, fee.toString()])) };
    const shared = records.filter(record => ['registerKey', 'approve', 'fund'].includes(record.label) || record.label.startsWith('withdraw-'));
    const sharedGas = shared.reduce((sum, record) => sum + record.receipt.gasUsed, 0n);
    report.gas.perWindowAttribution = Array.from({ length: selected.windows }, (_, index) => {
      const actions = records.filter(record => ['hold', 'publish', 'open'].some(method => record.label === `${method}-${index}`));
      const total = sharedGas + actions.reduce((sum, record) => sum + record.receipt.gasUsed, 0n);
      return { windowIndex: index, policy: 'all shared transactions and all withdrawals plus this window hold/publish/open',
        sharedTransactionHashes: shared.map(record => record.receipt.hash), windowTransactionHashes: actions.map(record => record.receipt.hash),
        conservativeGasUsed: total.toString(), maximumGasUsed: '3000000', passed: total <= 3000000n };
    });
    for (const window of report.gas.perWindowAttribution) assert(window.passed, `window ${window.windowIndex} exceeds the confirmed-receipt 3000000 gas limit`);
    const finalAnchor = await provider.getBlock(anchorNumber);
    if (!finalAnchor || finalAnchor.hash !== anchor.hash) throw new Unverified('confirmation anchor changed during observation; possible reorg');
    passed('the fixed two-confirmation experimental anchor remained canonical during observation; absolute finality is not asserted');
    report.status = 'PASS';
  } catch (error) {
    report.status = error instanceof Unverified ? 'UNVERIFIED' : 'FAIL';
    report.error = error.message;
  } finally { if (provider) await provider.destroy(); }
  return report;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let report;
  try { report = await observe(parseArguments(process.argv.slice(2))); }
  catch (error) { report = { schemaVersion: 1, status: 'FAIL', error: error.message }; }
  fs.mkdirSync(artifacts, { recursive: true });
  fs.writeFileSync(path.join(artifacts, report.case === 'ordinary' ? 'ordinary-settlement-observation.json' : 'settlement-observation.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
  process.exitCode = report.status === 'PASS' ? 0 : report.status === 'UNVERIFIED' ? 2 : 1;
}
