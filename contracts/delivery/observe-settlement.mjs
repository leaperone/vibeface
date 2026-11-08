import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { compile, artifact, artifacts, requireDependency, root, schema } from './compile.mjs';
const { AbiCoder, Contract, JsonRpcProvider, getAddress, getBytes, hexlify, recoverAddress, sha256, toUtf8Bytes } = requireDependency('ethers');
const coder = AbiCoder.defaultAbiCoder();
const HASH = /^0x[0-9a-fA-F]{64}$/, ADDRESS = /^0x[0-9a-fA-F]{40}$/;
class Unverified extends Error {}
function keys(value, required, optional = []) {
  assert(value && typeof value === 'object' && !Array.isArray(value), 'expected an object');
  for (const key of required) assert(Object.hasOwn(value, key), `missing ${key}`);
  for (const key of Object.keys(value)) assert([...required, ...optional].includes(key), `unexpected ${key}`);
}
function bareHex(value, bytes, label) {
  assert(typeof value === 'string' && /^[0-9a-f]+$/.test(value) && value.length % 2 === 0, `${label} must be lowercase hex`);
  if (bytes !== undefined) assert.equal(value.length, bytes * 2, label);
  return `0x${value}`;
}
function filename(value) { assert(typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/.test(value), 'proof paths must be relative basenames'); }
export function observationCase(name = 'protected') {
  assert.equal(name, 'protected', 'old Strict/ordinary runtimes were retired; use protected budget evidence or the fixed historical source');
  return { name };
}
export function validateBundle(bundle, caseName = 'protected') {
  observationCase(caseName);
  keys(bundle, ['schemaVersion', 'escrow', 'asset', 'budgetId', 'orders', 'proofs', 'capsules', 'transactions']);
  assert.equal(bundle.schemaVersion, 2, 'protected evidence uses schema 2, never a relabeled old voucher/Strict bundle');
  assert(ADDRESS.test(bundle.escrow) && ADDRESS.test(bundle.asset) && HASH.test(bundle.budgetId));
  assert(Array.isArray(bundle.orders) && bundle.orders.length >= 1 && bundle.orders.length <= 2);
  let windows = 0; const slots = new Set(), orders = new Set();
  for (const order of bundle.orders) {
    keys(order, ['orderId', 'windowCount', 'amount']); assert(HASH.test(order.orderId));
    assert(!orders.has(order.orderId.toLowerCase()), 'duplicate order'); orders.add(order.orderId.toLowerCase());
    assert(Number.isSafeInteger(order.windowCount) && order.windowCount >= 1 && order.windowCount <= 2);
    assert(typeof order.amount === 'string' && /^(0|[1-9][0-9]{0,77})$/.test(order.amount));
    windows += order.windowCount;
    for (let index = 0; index < order.windowCount; index++) slots.add(`${order.orderId.toLowerCase()}:${index}`);
  }
  assert(windows <= 2, 'all protected batches have at most two windows');
  const proofs = new Set(), files = new Set();
  assert(Array.isArray(bundle.proofs));
  for (const proof of bundle.proofs) {
    keys(proof, proof.relation === 1 ? ['relation', 'public', 'receipt'] : ['relation', 'orderId', 'windowIndex', 'public', 'receipt']);
    assert([1, 2, 3, 4].includes(proof.relation));
    const slot = proof.relation === 1 ? 'key' : `${proof.orderId.toLowerCase()}:${proof.windowIndex}`;
    if (slot !== 'key') assert(slots.has(slot), 'proof belongs to another reserved window');
    const id = `${proof.relation}:${slot}`; assert(!proofs.has(id), 'duplicate proof'); proofs.add(id);
    for (const file of [proof.public, proof.receipt]) { filename(file); assert(!files.has(file)); files.add(file); }
  }
  assert(proofs.has('1:key'), 'genuine KeyBinding required');
  for (const slot of slots) for (const relation of [2, 3]) assert(proofs.has(`${relation}:${slot}`), 'every window needs genuine Source/Hold');
  assert(bundle.capsules.length === windows); const seen = new Set();
  for (const item of bundle.capsules) { keys(item, ['orderId', 'windowIndex', 'capsule']);
    const slot = `${item.orderId.toLowerCase()}:${item.windowIndex}`; assert(slots.has(slot) && !seen.has(slot)); seen.add(slot); bareHex(item.capsule, 80, 'capsule'); }
  keys(bundle.transactions, ['registerKey', 'approve', 'openBudget', 'reserveBatch', 'holdBatch', 'revealBatch', 'withdrawBudget'], ['open']);
  const hashes = [bundle.transactions.registerKey, bundle.transactions.approve, bundle.transactions.openBudget];
  for (const method of ['reserveBatch', 'holdBatch', 'revealBatch', 'withdrawBudget']) {
    assert(Array.isArray(bundle.transactions[method]) && bundle.transactions[method].length >= 1 && bundle.transactions[method].length <= 2);
    hashes.push(...bundle.transactions[method]);
  }
  if (bundle.transactions.open) { assert(Array.isArray(bundle.transactions.open) && bundle.transactions.open.length <= windows); hashes.push(...bundle.transactions.open); }
  assert(hashes.every(hash => typeof hash === 'string' && HASH.test(hash))); assert.equal(new Set(hashes.map(hash => hash.toLowerCase())).size, hashes.length, 'duplicate transaction hash');
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
  assert([4, 6].includes(args.length), 'Usage: node observe-settlement.mjs --settlement <protected-bundle.json> --rpc-url <original-loopback-rpc> [--case protected]');
  const values = {};
  for (let i = 0; i < args.length; i += 2) { assert(['--settlement', '--rpc-url', '--case'].includes(args[i]) && values[args[i]] === undefined); values[args[i]] = args[i + 1]; }
  assert(values['--settlement'] && values['--rpc-url']);
  const url = new URL(values['--rpc-url']); assert(url.protocol === 'http:' && ['127.0.0.1', '[::1]'].includes(url.hostname) && !url.username && !url.password && !url.search && !url.hash && url.pathname === '/');
  return { bundleFile: path.resolve(values['--settlement']), rpcUrl: url.href, caseName: observationCase(values['--case']).name };
}
function readRegular(file) { const stat = fs.lstatSync(file); assert(stat.isFile() && stat.size > 0 && stat.size <= 1048576); return fs.readFileSync(file); }
function same(a, b) { assert.equal(a.toLowerCase(), b.toLowerCase()); }
function address(a, b) { assert.equal(getAddress(a), getAddress(b)); }
export async function observe({ bundleFile, rpcUrl, caseName = 'protected' }) {
  const report = { schemaVersion: 2, status: 'UNVERIFIED', case: caseName, scope: 'genuine protected budget on the original local chain; binary receipts require independent host verification', checks: [], transactions: [], proofs: [] };
  let provider;
  try {
    const bundle = validateBundle(JSON.parse(readRegular(bundleFile)), caseName); compile({ checkTrust: true });
    const built = artifact('DeliveryEscrow'), fields = built.abi.find(item => item.name === 'preview').inputs[0].components;
    assert.equal(fields.length, 47); assert.equal(fields[42].name, 'priorityDeadline');
    const statementType = `tuple(${fields.map(field => `${field.type} ${field.name}`).join(',')})`;
    const object = value => Object.fromEntries(fields.map(field => [field.name, value[field.name]]));
    const statementBytes = value => coder.encode([statementType], [object(value)]);
    const canonical = JSON.parse(readRegular(path.resolve(root, '../../crates/delivery/proof/profile.json')));
    const word = value => { assert(Array.isArray(value) && value.length === 32 && value.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255)); return hexlify(Uint8Array.from(value)); };
    const profile = { domain: sha256(toUtf8Bytes('vibeface-delivery-profile-v1')), guestImageId: word(canonical.guest_image_id), statementSchemaHash: word(canonical.statement_schema_hash), cryptoSuite: canonical.crypto_suite,
      maxWindows: canonical.max_windows, maxWindowBytes: canonical.max_window_bytes, maxTotalBytes: canonical.max_total_bytes, maxGuestCycles: canonical.max_guest_cycles, maxProverMemoryBytes: canonical.max_prover_memory_bytes };
    assert.equal(profile.statementSchemaHash, sha256(toUtf8Bytes(schema)));
    const profileFields = built.abi.find(item => item.type === 'constructor').inputs[0].components;
    const profileType = `tuple(${profileFields.map(field => `${field.type} ${field.name}`).join(',')})`;
    const profileBytes = coder.encode([profileType], [profile]), profileId = sha256(profileBytes);
    provider = new JsonRpcProvider(rpcUrl, undefined, { cacheTimeout: -1 }); assert.equal((await provider.getNetwork()).chainId, 31337n);
    const tip = await provider.getBlockNumber(), anchorNumber = tip - 1;
    if (anchorNumber < 0) throw new Unverified('two experimental confirmation blocks are required');
    const anchor = await provider.getBlock(anchorNumber); if (!anchor) throw new Unverified('canonical anchor unavailable');
    const at = { blockTag: anchorNumber }, trust = JSON.parse(readRegular(path.join(root, 'trust.json')));
    const escrow = new Contract(bundle.escrow, built.abi, provider), token = new Contract(bundle.asset, artifact('TestToken').abi, provider);
    const trusted = async (name, target) => checkRuntime(trust.contracts[name], await provider.getCode(target, anchorNumber));
    await trusted('DeliveryEscrow', bundle.escrow); await trusted('TestToken', bundle.asset);
    assert.equal(await escrow.profileId(at), profileId); assert.equal(coder.encode([profileType], [await escrow.getProfile(at)]), profileBytes);
    const adapterAddress = await escrow.adapter(at), adapter = new Contract(adapterAddress, artifact('RiscZeroAdapter').abi, provider);
    await trusted('RiscZeroAdapter', adapterAddress); assert.equal(await adapter.imageId(at), profile.guestImageId);
    const verifierAddress = await adapter.verifier(at); await trusted('RiscZeroGroth16Verifier', verifierAddress);
    const verifier = new Contract(verifierAddress, artifact('RiscZeroGroth16Verifier').abi, provider), control = getBytes(await adapter.CONTROL_ROOT(at));
    assert.equal(await verifier.CONTROL_ROOT_0(at), hexlify(control.slice(0, 16).reverse())); assert.equal(await verifier.CONTROL_ROOT_1(at), hexlify(control.slice(16).reverse()));
    assert.equal(await verifier.BN254_CONTROL_ID(at), await adapter.BN254_CONTROL_ID(at)); assert.equal(await verifier.SELECTOR(at), await adapter.SELECTOR(at));
    const budget = await escrow.getBudget(bundle.budgetId, at); address(budget.asset, bundle.asset); assert.equal(budget.assetDecimals, 6n);
    const proofs = new Map(), proofDirectory = path.dirname(bundleFile);
    for (const entry of bundle.proofs) {
      const metadata = JSON.parse(readRegular(path.join(proofDirectory, entry.public)));
      keys(metadata, ['mode', 'kind', 'imageId', 'profileId', 'relation', 'journal', 'journalHash', 'seal']);
      assert.equal(metadata.mode, 'prove'); assert.equal(metadata.kind, 'groth16'); assert.equal(metadata.relation, entry.relation);
      same(bareHex(metadata.imageId, 32, 'image'), profile.guestImageId); same(bareHex(metadata.profileId, 32, 'profile'), profileId);
      const journal = bareHex(metadata.journal, 1504, 'journal'), seal = bareHex(metadata.seal, 260, 'seal'), digest = bareHex(metadata.journalHash, 32, 'journal hash');
      assert.equal(sha256(journal), digest); const statement = coder.decode([statementType], journal)[0]; assert.equal(statementBytes(statement), journal);
      assert.equal(statement.relation, BigInt(entry.relation)); assert.equal(statement.chainId, 31337n); address(statement.escrow, bundle.escrow);
      same(statement.domain, sha256(toUtf8Bytes('vibeface-delivery-statement-v1'))); same(statement.profileId, profileId); same(statement.guestImageId, profile.guestImageId);
      if (entry.relation === 1) assert.equal(statementBytes(await escrow.previewBinding(object(statement), at)), journal);
      else { same(statement.orderId, entry.orderId); assert.equal(statement.windowIndex, BigInt(entry.windowIndex)); assert.equal(statement.acceptance, 0n);
        assert.equal(statementBytes(await escrow.publicValues(entry.orderId, entry.windowIndex, entry.relation, at)), journal); }
      assert.equal(await adapter.verify(seal, digest, at), true, 'genuine public seal rejected');
      const slot = entry.relation === 1 ? 'key' : `${entry.orderId.toLowerCase()}:${entry.windowIndex}`;
      proofs.set(`${entry.relation}:${slot}`, { journal, seal, statement });
      report.proofs.push({ relation: entry.relation, orderId: entry.orderId ?? null, windowIndex: entry.windowIndex ?? null, journalHash: digest, receiptSha256: sha256(readRegular(path.join(proofDirectory, entry.receipt))) });
    }
    const key = proofs.get('1:key'); assert.equal(key.statement.bindingAction, 1n);
    const orders = new Map(); let total = 0n; const slots = new Map();
    for (const expected of bundle.orders) {
      const order = await escrow.getOrder(expected.orderId, at); same(await escrow.orderBudget(expected.orderId, at), bundle.budgetId);
      address(order.terms.payer, budget.payer); address(order.terms.providerWallet, budget.payee); address(order.terms.asset, bundle.asset); same(order.terms.providerPeer, budget.providerPeer);
      same(order.terms.keyBindingHash, key.statement.keyBindingHash); assert.equal(order.terms.acceptance, 0n); assert.equal(order.windows.length, expected.windowCount); assert.equal(order.terms.escrowAmount, BigInt(expected.amount));
      assert.equal(order.earned, order.terms.escrowAmount); assert.equal(order.withdrawn, 0n); assert.equal(order.refunded, 0n);
      total += order.earned; orders.set(expected.orderId.toLowerCase(), order);
      for (let index = 0; index < expected.windowCount; index++) { const fact = order.windows[index], slot = `${expected.orderId.toLowerCase()}:${index}`;
        assert.equal(fact.phase, 3n); const capsule = bundle.capsules.find(item => `${item.orderId.toLowerCase()}:${item.windowIndex}` === slot);
        same(fact.capsule, bareHex(capsule.capsule, 80, 'capsule')); same(sha256(fact.capsule), fact.spec.capsuleHash); slots.set(slot, fact); }
    }
    assert.equal(budget.deposit, total); assert.equal(budget.locked, 0n); assert.equal(budget.earned, total); assert.equal(budget.withdrawn, total); assert.equal(budget.refunded, 0n);
    const records = [], blocks = new Map();
    const block = async number => { if (!blocks.has(number)) blocks.set(number, await provider.getBlock(number)); const b = blocks.get(number); if (!b) throw new Unverified('receipt block unavailable'); return b; };
    const transaction = async (hash, contract, method) => {
      const receipt = await provider.getTransactionReceipt(hash); if (!receipt || receipt.blockNumber > anchorNumber) throw new Unverified(`${method} receipt is missing or unconfirmed`);
      assert.equal(receipt.status, 1); const included = await block(receipt.blockNumber); if (included.hash !== receipt.blockHash) throw new Unverified(`${method} receipt is not canonical`);
      const tx = await provider.getTransaction(hash); if (!tx || tx.blockHash !== receipt.blockHash) throw new Unverified(`${method} transaction inclusion unavailable`);
      assert.equal(tx.chainId, 31337n); assert.equal(tx.value, 0n); address(tx.to, contract.target);
      const parsed = contract.interface.parseTransaction({ data: tx.data }); assert.equal(parsed.name, method); same(contract.interface.encodeFunctionData(parsed.fragment, parsed.args), tx.data);
      const record = { receipt, tx, parsed, timestamp: BigInt(included.timestamp), block: included }; records.push(record);
      report.transactions.push({ hash, method, from: tx.from, blockNumber: receipt.blockNumber, blockHash: receipt.blockHash, gasUsed: receipt.gasUsed.toString(), executionGasFeeWei: (receipt.gasUsed * receipt.gasPrice).toString() });
      return record;
    };
    const events = (record, name) => record.receipt.logs.filter(log => log.address.toLowerCase() === bundle.escrow.toLowerCase()).map(log => escrow.interface.parseLog(log)).filter(log => log?.name === name);
    const registered = await transaction(bundle.transactions.registerKey, escrow, 'registerKey'); same(registered.parsed.args[1], key.seal);
    assert.equal(statementBytes(await escrow.previewBinding(registered.parsed.args[0], at)), key.journal);
    const approval = await transaction(bundle.transactions.approve, token, 'approve'); address(approval.tx.from, budget.payer); address(approval.parsed.args[0], bundle.escrow); assert.equal(approval.parsed.args[1], budget.deposit);
    const opened = await transaction(bundle.transactions.openBudget, escrow, 'openBudget'); address(opened.tx.from, budget.payer); address(opened.parsed.args[1], budget.payee); same(opened.parsed.args[2], budget.providerPeer); address(opened.parsed.args[3], bundle.asset); assert.equal(opened.parsed.args[4], budget.deposit);
    assert.equal(events(opened, 'BudgetOpened').length, 1); same(events(opened, 'BudgetOpened')[0].args[0], bundle.budgetId);
    const covered = new Map(['reserveBatch', 'holdBatch', 'revealBatch'].map(method => [method, new Set()]));
    const batchSizes = {};
    for (const method of ['reserveBatch', 'holdBatch', 'revealBatch']) {
      batchSizes[method] = [];
      for (const hash of bundle.transactions[method]) {
        const record = await transaction(hash, escrow, method); same(record.parsed.args[0], bundle.budgetId);
        let count = 0;
        if (method === 'reserveBatch') {
          address(record.tx.from, budget.payer);
          for (const r of record.parsed.args[1]) {
            const statements = await escrow.preview(r.input, r.specs, at), orderId = statements[0].orderId.toLowerCase(), order = orders.get(orderId); assert(order);
            assert.equal(r.seals.length, statements.length); count += statements.length;
            for (let index = 0; index < statements.length; index++) { const slot = `${orderId}:${index}`; assert(slots.has(slot) && !covered.get(method).has(slot)); covered.get(method).add(slot);
              const source = proofs.get(`2:${slot}`); assert.equal(statementBytes(statements[index]), source.journal); same(r.seals[index], source.seal); }
            address(recoverAddress(await escrow.quoteWalletDigest(order.terms.quoteDigest, budget.providerPeer, at), r.walletSignature), budget.payee);
            assert(record.timestamp <= order.terms.quoteExpiresAt);
          }
          assert.equal(events(record, 'BudgetReserved').length, record.parsed.args[1].length);
        } else {
          count = record.parsed.args[1].length;
          for (const item of record.parsed.args[1]) {
            const slot = `${item.orderId.toLowerCase()}:${item.index}`; assert(slots.has(slot) && !covered.get(method).has(slot)); covered.get(method).add(slot);
            const order = orders.get(item.orderId.toLowerCase());
            if (method === 'holdBatch') { same(item.seal, proofs.get(`3:${slot}`).seal); assert(record.timestamp <= order.terms.holdDeadline); }
            else { same(item.capsule, slots.get(slot).capsule); assert(record.timestamp <= order.terms.keyDeadline);
              if (order.terms.wholeOrderBonus > 0n) assert(record.timestamp <= order.terms.priorityDeadline, 'full bonus amount requires timely completion'); }
          }
          assert.equal(events(record, method === 'holdBatch' ? 'WindowUpdated' : 'BudgetEarned').length, count);
          if (method === 'revealBatch') assert(record.receipt.gasUsed <= 800000n, 'finite offline reveal bound exceeded');
        }
        assert(count >= 1 && count <= 2); batchSizes[method].push(count);
      }
      assert.equal(covered.get(method).size, slots.size, `${method} omitted a selected allocation`);
    }
    if (slots.size === 2) for (const method of ['reserveBatch', 'holdBatch', 'revealBatch']) assert(batchSizes[method].includes(2), `${method} did not actually batch both slots`);
    let withdrawn = 0n; const withdrawals = [];
    for (const hash of bundle.transactions.withdrawBudget) { const record = await transaction(hash, escrow, 'withdrawBudget'); same(record.parsed.args[0], bundle.budgetId);
      const paid = events(record, 'BudgetWithdrawn'); assert.equal(paid.length, 1); address(paid[0].args[1], budget.payee); withdrawn += paid[0].args[2]; withdrawals.push([record, paid[0].args[2]]); }
    assert.equal(withdrawn, total);
    for (const hash of bundle.transactions.open ?? []) { const record = await transaction(hash, escrow, 'open'); const slot = `${record.parsed.args[0].toLowerCase()}:${record.parsed.args[1]}`;
      same(record.parsed.args[2], proofs.get(`4:${slot}`).seal); }
    const accounts = [budget.payer, budget.payee, bundle.escrow].map(getAddress);
    const movement = async (record, from, to, amount) => {
      if (record.block.transactions.length !== 1) throw new Unverified('historical balance attribution requires isolated token movement blocks');
      for (const account of accounts) { const delta = (await token.balanceOf(account, { blockTag: record.receipt.blockNumber })) - (await token.balanceOf(account, { blockTag: record.receipt.blockNumber - 1 }));
        assert.equal(delta, account === getAddress(from) ? -amount : account === getAddress(to) ? amount : 0n); }
    };
    await movement(opened, budget.payer, bundle.escrow, budget.deposit); for (const [record, amount] of withdrawals) await movement(record, bundle.escrow, budget.payee, amount);
    for (const account of accounts) { const delta = (await token.balanceOf(account, at)) - (await token.balanceOf(account, { blockTag: opened.receipt.blockNumber - 1 }));
      assert.equal(delta, account === getAddress(budget.payer) ? -total : account === getAddress(budget.payee) ? total : 0n); }
    if ((await provider.getBlock(anchorNumber))?.hash !== anchor.hash) throw new Unverified('canonical anchor changed during observation');
    report.status = 'PASS'; report.budgetId = bundle.budgetId; report.orderIds = [...orders.keys()]; report.imageId = profile.guestImageId; report.profileId = profileId;
    report.money = { deposit: budget.deposit.toString(), earned: budget.earned.toString(), withdrawn: budget.withdrawn.toString(), refunded: budget.refunded.toString(), payer: budget.payer, payee: budget.payee };
    report.batchSizes = batchSizes; report.confirmationPolicy = { confirmations: 2, anchorNumber, anchorHash: anchor.hash, absoluteFinality: false };
    report.gas = { operations: report.transactions, totalGasUsed: records.reduce((n, r) => n + r.receipt.gasUsed, 0n).toString(), totalExecutionFeeWei: records.reduce((n, r) => n + r.receipt.gasUsed * r.receipt.gasPrice, 0n).toString(),
      scope: 'listed register/approve/open/reserve/hold/reveal/withdraw and optional Open; deployments, proving and unlisted recovery attempts are separate; proofs and 80-byte storage remain per-window' };
    report.checks = ['pinned production runtimes and exact canonical profile', 'genuine Source/Hold seals and contract-constructed journals', 'actual finite multi-slot calldata and canonical receipts', 'fixed-payee cumulative withdrawal and historical token conservation'];
  } catch (error) { report.status = error instanceof Unverified ? 'UNVERIFIED' : 'FAIL'; report.error = error.message; }
  finally { if (provider) provider.destroy(); }
  return report;
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let report; try { report = await observe(parseArguments(process.argv.slice(2))); } catch (error) { report = { schemaVersion: 2, status: 'FAIL', error: error.message }; }
  fs.mkdirSync(artifacts, { recursive: true }); fs.writeFileSync(path.join(artifacts, 'protected-settlement-observation.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2)); process.exitCode = report.status === 'PASS' ? 0 : report.status === 'UNVERIFIED' ? 2 : 1;
}
