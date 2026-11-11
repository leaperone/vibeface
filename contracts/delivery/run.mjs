import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { compile, artifact, artifacts, requireDependency, schema } from './compile.mjs';

const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, Wallet, AbiCoder, sha256, toUtf8Bytes, hexlify, ZeroAddress, ZeroHash, MaxUint256 } = requireDependency('ethers');
compile();
const rpc = ganache.provider({ logging: { quiet: true }, wallet: { deterministic: true },
  chain: { chainId: 31337, hardfork: 'shanghai' }, miner: { timestampIncrement: 0 } });
const provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 10;
const payer = await provider.getSigner(0), keeper = await provider.getSigner(2);
const payerAddress = await payer.getAddress(), keeperAddress = await keeper.getAddress();
const accounts = Object.values(rpc.getInitialAccounts());
const wallet = new Wallet(accounts[1].secretKey);
const tag = value => sha256(toUtf8Bytes(value));
const coder = AbiCoder.defaultAbiCoder();
const profile = {
  domain: tag('vibeface-delivery-profile-v1'), guestImageId: tag('accounting-test-image-not-a-delivery-proof'),
  statementSchemaHash: sha256(toUtf8Bytes(schema)), cryptoSuite: 1, maxWindows: 2, maxWindowBytes: 65536,
  maxTotalBytes: 131072, maxGuestCycles: 64000000, maxProverMemoryBytes: 17179869184n,
};
const tx = async promise => (await promise).wait();
async function deploy(name, args = []) {
  const a = artifact(name);
  const c = await new ContractFactory(a.abi, a.bytecode, payer).deploy(...args, { gasLimit: 29000000 });
  await c.waitForDeployment();
  return c;
}
const escrow = await deploy('AccountingTestEscrow', [profile]);
const escrowAddress = await escrow.getAddress();
const token = await deploy('TestToken', [payerAddress]);
const tokenAddress = await token.getAddress();
await tx(token.approve(escrowAddress, MaxUint256));
const fields = artifact('DeliveryEscrow').abi.find(x => x.name === 'preview').inputs[0].components;
assert.equal(fields.length, 47);
assert.equal(fields[42].name, 'priorityDeadline');
const statementType = `tuple(${fields.map(x => `${x.type} ${x.name}`).join(',')})`;
const emptyStatement = () => Object.fromEntries(fields.map(x => [x.name, x.type === 'address' ? ZeroAddress : x.type === 'bytes32' ? ZeroHash : 0n]));
const object = value => Object.fromEntries(fields.map(x => [x.name, value[x.name]]));
const encoded = value => coder.encode([statementType], [object(value)]);
const seal = value => sha256(encoded(value));
const now = async () => BigInt((await provider.getBlock('latest')).timestamp);
const time = async value => {
  await rpc.request({ method: 'evm_setTime', params: [Number(value) * 1000] });
  await rpc.request({ method: 'evm_mine', params: [] });
};
const start = await now();
const baseKey = { receiverPeer: tag('receiver-peer'), receiverPublic: tag('receiver-x25519'), receiverKeyId: tag('key-one'),
  receiverKeySerial: 1n, keyValidFrom: start - 10n, keyValidUntil: start + 1000000n, bindingAction: 1n };
async function register(overrides = {}, proofOverride) {
  const input = { ...emptyStatement(), relation: 1n, ...baseKey, ...overrides };
  const b = object(await escrow.previewBinding(input));
  await tx(escrow.registerKey(input, proofOverride ?? seal(b)));
  return b;
}
await register();
let nonce = 0n;
let ids = [];
async function make(count = 1, overrides = {}, specOverrides = {}) {
  const timestamp = await now();
  const capsules = Array.from({ length: count }, (_, i) => hexlify(new Uint8Array(80).fill(i + 1)));
  let offset = 0n;
  const specs = capsules.map((capsule, index) => {
    const s = { index, offset, plaintextBytes: 64n, ciphertextBytes: 132n,
      ciphertextHash: tag(`C-${index}`), capsuleHash: sha256(capsule), plaintextCommitment: tag(`plain-${index}`),
      deliveryFee: 10n, reservationFee: 2n, ...specOverrides };
    offset += BigInt(s.plaintextBytes);
    return s;
  });
  const input = { ...emptyStatement(), relation: 2n, ...baseKey, sourcePeer: tag('source-peer'), providerPeer: tag('provider-peer'),
    payer: payerAddress, providerWallet: wallet.address, asset: tokenAddress, totalPlaintextBytes: offset, windowCount: count,
    wholeOrderBonus: 5n, escrowAmount: BigInt(count) * 12n + 5n, quoteExpiresAt: timestamp + 10000n,
    holdDeadline: timestamp + 20000n, keyDeadline: timestamp + 30000n, openDeadline: timestamp + 40000n,
    retainUntil: timestamp + 50000n, priorityDeadline: timestamp + 25000n, payerOrderNonce: ++nonce,
    providerQuoteNonce: tag(`quote-${nonce}`), ...overrides };
  const statements = (await escrow.preview(input, specs)).map(object);
  const signature = await wallet.signTypedData({ name: 'VibefaceDelivery', version: '1', chainId: 31337, verifyingContract: escrowAddress },
    { DeliveryQuote: [{ name: 'quoteDigest', type: 'bytes32' }, { name: 'providerPeer', type: 'bytes32' }] },
    { quoteDigest: statements[0].quoteDigest, providerPeer: input.providerPeer });
  return { id: statements[0].orderId, input, specs, capsules, statements, seals: statements.map(seal), signature };
}
let budgets = [];
async function budget(deposit = 100n, overrides = {}) {
  const salt = tag(`budget-${++nonce}`);
  const roles = { payee: wallet.address, providerPeer: tag('provider-peer'), asset: tokenAddress, ...overrides };
  const id = sha256(coder.encode(['bytes32', 'uint256', 'address', 'address', 'bytes32'],
    [tag('vibeface-delivery-budget-v1'), 31337, escrowAddress, payerAddress, salt]));
  await tx(escrow.openBudget(salt, roles.payee, roles.providerPeer, roles.asset, deposit));
  budgets.push(id);
  return { id, deposit };
}
const reservation = o => ({ input: o.input, specs: o.specs, seals: o.seals, walletSignature: o.signature });
const ref = (o, index = 0) => ({ orderId: o.id, index });
async function reserve(b, orders) {
  const receipt = await tx(escrow.reserveBatch(b.id, orders.map(reservation)));
  for (const o of orders) if (!ids.includes(o.id)) ids.push(o.id);
  return receipt;
}
const proof = async (o, i, relation) => seal(await escrow.publicValues(o.id, i, relation));
async function holds(b, refs) {
  return tx(escrow.connect(keeper).holdBatch(b.id, await Promise.all(refs.map(async ([o, index = 0]) =>
    ({ ...ref(o, index), seal: await proof(o, index, 3) })))));
}
const reveals = (b, refs) => tx(escrow.connect(keeper).revealBatch(b.id, refs.map(([o, index = 0]) => ({ ...ref(o, index), capsule: o.capsules[index] }))));
const state = b => escrow.getBudget(b.id);
const window = (o, index = 0) => escrow.getWindow(o.id, index);
const reject = async action => assert.rejects(async () => { const result = await action(); if (result?.wait) await result.wait(); });
async function invariant() {
  let expectedBalance = 0n;
  for (const id of budgets) {
    const b = await escrow.getBudget(id);
    assert(b.withdrawn <= b.earned, 'withdrawn <= earned');
    assert(b.locked + b.earned + b.refunded <= b.deposit, 'free + locked + earned + refunded = deposit');
    expectedBalance += b.deposit - b.withdrawn - b.refunded;
  }
  for (const id of ids) {
    const o = await escrow.getOrder(id);
    assert(o.earned + o.refunded <= o.terms.escrowAmount, 'order earnings and refunds cannot exceed exact allocation');
    assert.equal(o.withdrawn, 0n, 'budget withdrawals are accounted at budget level');
  }
  assert.equal(await token.balanceOf(escrowAddress), expectedBalance, 'escrow token conservation across every budget');
}
const results = [];
async function check(name, run) {
  const snapshot = await rpc.request({ method: 'evm_snapshot', params: [] });
  ids = []; budgets = [];
  try { await run(); await invariant(); results.push({ name, passed: true }); console.log(`PASS ${name}`); }
  finally { await rpc.request({ method: 'evm_revert', params: [snapshot] }); ids = []; budgets = []; }
}
try {
  await check('canonical source statement remains 47 words; strict is explicitly unsupported', async () => {
    const o = await make(2);
    assert.equal((encoded(o.statements[0]).length - 2) / 2, 1504);
    assert.equal(fields[42].name, 'priorityDeadline');
    assert.notEqual(seal(o.statements[0]), seal(o.statements[1]));
    await reject(() => make(1, { acceptance: 1n }));
    for (const name of ['fund', 'hold', 'publish', 'withdraw', 'refund', 'cancel', 'refundBonus'])
      assert(!artifact('DeliveryEscrow').abi.some(x => x.name === name), `${name} must not bypass the protected budget`);
  });
  await check('two distinct orders actually reserve, hold and reveal in three finite batches, then one cumulative withdrawal', async () => {
    const b = await budget(); const a = await make(), c = await make();
    const payerBefore = await token.balanceOf(payerAddress), payeeBefore = await token.balanceOf(wallet.address);
    const allocation = await reserve(b, [a, c]);
    assert.equal(allocation.logs.filter(log => { try { return escrow.interface.parseLog(log)?.name === 'BudgetReserved'; } catch { return false; } }).length, 2);
    assert.equal((await state(b)).locked, 34n);
    await holds(b, [[a], [c]]); await reveals(b, [[a], [c]]);
    assert.equal((await state(b)).earned, 34n); assert.equal((await state(b)).locked, 0n);
    assert.equal(await token.balanceOf(wallet.address), payeeBefore, 'reveal must not transfer tokens');
    await tx(escrow.connect(keeper).withdrawBudget(b.id));
    assert.equal((await state(b)).withdrawn, 34n); assert.equal(await token.balanceOf(wallet.address), payeeBefore + 34n);
    await reject(() => escrow.connect(keeper).withdrawBudget(b.id));
    await tx(escrow.closeBudget(b.id)); await time(a.input.keyDeadline + 1n);
    await tx(escrow.connect(keeper).refundBudget(b.id, [ref(a), ref(c)]));
    assert.equal((await state(b)).refunded, 66n); assert.equal((await state(b)).state, 3n);
    assert.equal(await token.balanceOf(payerAddress), payerBefore + 66n);
  });
  await check('whole-order bonus is earned exactly once; earned duplicates do not roll back another capsule', async () => {
    const b = await budget(); const o = await make(2);
    await reserve(b, [o]); await holds(b, [[o, 0], [o, 1]]);
    await reveals(b, [[o, 0]]); assert.equal((await state(b)).earned, 12n);
    await tx(escrow.closeBudget(b.id));
    await reveals(b, [[o, 0], [o, 1]]); assert.equal((await state(b)).earned, 29n);
    await reveals(b, [[o, 1], [o, 0]]); assert.equal((await state(b)).earned, 29n);
    const statement = await escrow.publicValues(o.id, 0, 4);
    await tx(escrow.connect(keeper).open(o.id, 0, seal(statement)));
    assert.equal((await window(o)).decryptionProven, true);
    await reject(() => escrow.open(o.id, 0, seal(statement)));
  });
  await check('close is payer-only, blocks new allocation and preserves historical keys and all live rights', async () => {
    const b = await budget(); const o = await make(); await reserve(b, [o]);
    await reject(() => escrow.connect(keeper).closeBudget(b.id));
    await tx(escrow.closeBudget(b.id)); await reject(async () => reserve(b, [await make()]));
    await register({ receiverKeySerial: 2n, receiverPublic: ZeroHash, receiverKeyId: ZeroHash, bindingAction: 2n });
    await reject(() => escrow.refundBudget(b.id, [ref(o)]));
    await holds(b, [[o]]); await reveals(b, [[o]]);
    assert.equal((await state(b)).earned, 17n);
    await tx(escrow.connect(keeper).withdrawBudget(b.id)); assert.equal((await state(b)).withdrawn, 17n);
  });
  await check('overspend and reassignment reject atomically; exact reservations retry without a second allocation', async () => {
    const b = await budget(20n), a = await make(), c = await make();
    await reject(() => reserve(b, [a, c])); assert.equal((await state(b)).locked, 0n);
    await reserve(b, [a]); await reserve(b, [a]); assert.equal((await state(b)).locked, 17n);
    await reject(() => reserve(b, [c]));
    const other = await budget(); await reject(() => reserve(other, [a]));
    await reject(() => escrow.reserveBatch(b.id, [{ ...reservation(a), walletSignature: `0x${'00'.repeat(65)}` }]));
    await reject(() => escrow.reserveBatch(other.id, [reservation(c), reservation(c), reservation(c)]));
    await tx(escrow.reserveBatch(other.id, [reservation(c), reservation(c)]));
  });
  await check('source proof, wallet quote, payer, payee, provider and deployment domains are bound before service', async () => {
    const b = await budget(); const good = await make();
    await reject(() => escrow.reserveBatch(b.id, [{ ...reservation(good), seals: [tag('wrong-proof')] }]));
    const wrongPayee = await make(1, { providerWallet: keeperAddress });
    await reject(() => reserve(b, [wrongPayee]));
    const wrongPeer = await make(1, { providerPeer: tag('other-provider') }); await reject(() => reserve(b, [wrongPeer]));
    const wrongPayer = await make(1, { payer: keeperAddress }); await reject(() => reserve(b, [wrongPayer]));
    const alien = await deploy('AccountingTestEscrow', [profile]);
    const wrongSignature = await wallet.signTypedData({ name: 'VibefaceDelivery', version: '1', chainId: 31337, verifyingContract: await alien.getAddress() },
      { DeliveryQuote: [{ name: 'quoteDigest', type: 'bytes32' }, { name: 'providerPeer', type: 'bytes32' }] },
      { quoteDigest: good.statements[0].quoteDigest, providerPeer: good.input.providerPeer });
    await reject(() => escrow.reserveBatch(b.id, [{ ...reservation(good), walletSignature: wrongSignature }]));
    await reserve(b, [good]); assert.equal((await state(b)).locked, 17n);
  });
  await check('no Hold, altered E, wrong budget, duplicate and malformed windows cannot earn', async () => {
    const b = await budget(); const a = await make(), c = await make(); await reserve(b, [a, c]);
    await reject(() => reveals(b, [[a]]));
    await reject(() => escrow.holdBatch(b.id, [{ ...ref(a), seal: tag('wrong-hold') }]));
    await holds(b, [[a], [c]]);
    await reject(() => escrow.revealBatch(b.id, [{ ...ref(a), capsule: a.capsules[0] }, { ...ref(c), capsule: `0x${'ff'.repeat(80)}` }]));
    assert.equal((await state(b)).earned, 0n); assert.equal((await window(a)).capsule, '0x');
    const other = await budget(); await reject(() => reveals(other, [[a]]));
    await reject(() => escrow.revealBatch(b.id, [{ ...ref(a), index: 2, capsule: a.capsules[0] }]));
    await reject(() => escrow.revealBatch(b.id, [{ ...ref(a), capsule: '0x01' }]));
    await reveals(b, [[a]]); await reveals(b, [[a], [c]]); assert.equal((await state(b)).earned, 34n);
  });
  await check('late completion earns only base fees and releases the whole-order bonus after the fixed deadline', async () => {
    const b = await budget(); const o = await make(2); await reserve(b, [o]); await holds(b, [[o, 0], [o, 1]]);
    await time(o.input.priorityDeadline + 1n); await reveals(b, [[o, 0], [o, 1]]);
    assert.equal((await state(b)).earned, 24n); assert.equal((await state(b)).locked, 5n);
    await tx(escrow.closeBudget(b.id)); await reject(() => escrow.refundBudget(b.id, [ref(o)]));
    await time(o.input.keyDeadline + 1n); await tx(escrow.refundBudget(b.id, [ref(o)]));
    assert.equal((await state(b)).locked, 0n); assert.equal((await state(b)).refunded, 76n);
    assert.equal((await escrow.getOrder(o.id)).refunded, 5n); await tx(escrow.connect(keeper).withdrawBudget(b.id));
  });
  await check('unrevealed allocations refund only after the immutable maximum claim deadline', async () => {
    const b = await budget(); const a = await make(), c = await make(); await reserve(b, [a, c]);
    await holds(b, [[a]]); await tx(escrow.closeBudget(b.id));
    await time(a.input.keyDeadline); await reject(() => escrow.refundBudget(b.id, [ref(a), ref(c)]));
    await time(a.input.keyDeadline + 1n); await tx(escrow.connect(keeper).refundBudget(b.id, [ref(a), ref(c)]));
    assert.equal((await state(b)).earned, 0n); assert.equal((await state(b)).refunded, 100n);
    await reject(() => reveals(b, [[a]])); await reject(() => escrow.refundBudget(b.id, [ref(a)]));
  });
  await check('token withdrawal failure preserves earned claims; closing never refunds an earned fee', async () => {
    const b = await budget(20n); const o = await make(); await reserve(b, [o]); await holds(b, [[o]]); await reveals(b, [[o]]);
    await tx(token.configure(3)); await reject(() => escrow.withdrawBudget(b.id));
    assert.equal((await state(b)).earned, 17n); assert.equal((await state(b)).withdrawn, 0n);
    await tx(token.configure(0)); await tx(escrow.closeBudget(b.id)); await time(o.input.keyDeadline + 1n);
    await tx(escrow.refundBudget(b.id, [ref(o)])); assert.equal((await state(b)).refunded, 3n);
    await tx(escrow.connect(keeper).withdrawBudget(b.id)); assert.equal((await state(b)).withdrawn, 17n);
  });
  for (const mode of [2, 3, 4, 6, 7]) await check(`non-exact token mode ${mode} cannot deposit or corrupt budget accounting`, async () => {
    await tx(token.configure(mode)); await reject(() => budget()); await tx(token.configure(0));
    assert.equal(await token.balanceOf(escrowAddress), 0n);
  });
  await check('decimal drift and reentrancy cannot steal cumulative earned funds', async () => {
    const b = await budget(); const o = await make(); await reserve(b, [o]); await holds(b, [[o]]); await reveals(b, [[o]]);
    await tx(token.setDecimals(18)); await reject(() => escrow.withdrawBudget(b.id)); await tx(token.setDecimals(6));
    await tx(token.attack(escrowAddress, escrow.interface.encodeFunctionData('withdrawBudget', [b.id]))); await tx(token.configure(5));
    await tx(escrow.connect(keeper).withdrawBudget(b.id)); assert.equal(await token.reentrySucceeded(), false); await tx(token.configure(0));
    assert.equal((await state(b)).withdrawn, 17n);
  });
  await check('exact spent budgets close without a zero-value refund and non-returning transfers remain supported', async () => {
    await tx(token.configure(1)); const b = await budget(17n); const o = await make();
    await tx(token.setDecimals(18)); await reject(() => reserve(b, [o])); await tx(token.setDecimals(6));
    await reserve(b, [o]); await holds(b, [[o]]); await tx(escrow.closeBudget(b.id));
    assert.equal((await state(b)).state, 2n); await reveals(b, [[o]]); assert.equal((await state(b)).state, 3n);
    await tx(escrow.connect(keeper).withdrawBudget(b.id)); assert.equal((await state(b)).withdrawn, 17n);
    await tx(token.configure(0));
  });
  const production = await deploy('DeliveryEscrow', [profile]);
  assert((await provider.getCode(await production.getAddress())).length > 2);
  const report = { verifier: 'synthetic accounting digest only; not a ZK proof', maxBatchWindows: 2,
    productionRuntimeBytes: artifact('DeliveryEscrow').deployedBytes, cases: results };
  fs.mkdirSync(artifacts, { recursive: true });
  fs.writeFileSync(path.join(artifacts, 'settlement-report.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(`Protected budget accounting: ${results.length} cases; production runtime ${report.productionRuntimeBytes} bytes.`);
} finally { provider.destroy(); await rpc.disconnect(); }
