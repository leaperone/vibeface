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
  statementSchemaHash: sha256(toUtf8Bytes(schema)), cryptoSuite: 1, maxWindows: 16, maxWindowBytes: 65536,
  maxTotalBytes: 1048576, maxGuestCycles: 64000000, maxProverMemoryBytes: 17179869184n,
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
async function fund(o) {
  await tx(escrow.fund(o.input, o.specs, o.seals, o.signature));
  ids.push(o.id);
  return o;
}
const funded = async (...args) => fund(await make(...args));
const proof = async (o, i, relation) => seal(await escrow.publicValues(o.id, i, relation));
const hold = async (o, i = 0) => tx(escrow.connect(keeper).hold(o.id, i, await proof(o, i, 3)));
const publish = async (o, i = 0) => tx(escrow.connect(keeper).publish(o.id, i, o.capsules[i]));
const open = async (o, i = 0) => tx(escrow.connect(keeper).open(o.id, i, await proof(o, i, 4)));
const order = o => escrow.getOrder(o.id);
const window = (o, i = 0) => escrow.getWindow(o.id, i);
const reject = async action => assert.rejects(async () => { const result = await action(); if (result?.wait) await result.wait(); });
async function invariant() {
  let expectedBalance = 0n;
  for (const id of ids) {
    const o = await escrow.getOrder(id);
    assert(o.withdrawn <= o.earned, 'withdrawn <= earned');
    assert(o.earned + o.refunded <= o.terms.escrowAmount, 'earned + refunded <= funded');
    expectedBalance += o.terms.escrowAmount - o.withdrawn - o.refunded;
  }
  assert.equal(await token.balanceOf(escrowAddress), expectedBalance, 'escrow token conservation across every order');
}
const results = [];
async function check(name, run) {
  const snapshot = await rpc.request({ method: 'evm_snapshot', params: [] });
  ids = [];
  try {
    await run();
    await invariant();
    results.push({ name, passed: true });
    console.log(`PASS ${name}`);
  } finally { await rpc.request({ method: 'evm_revert', params: [snapshot] }); ids = []; }
}
try {
  await check('47 ABI slots and 1504 bytes; priority deadline remains slot 42', async () => {
    const o = await make(2);
    assert.equal((encoded(o.statements[0]).length - 2) / 2, 1504);
    assert.equal(o.statements[0].windowIndex, 0n);
    assert.equal(o.statements[1].windowIndex, 1n);
    assert.notEqual(seal(o.statements[0]), seal(o.statements[1]), 'window statements cannot alias');
  });
  await check('independent canonical order preterms manifest quote and wallet hash vectors', async () => {
    const o = await make(2);
    const source = o.statements[0];
    const hashAbi = (types, values) => sha256(coder.encode(types, values));
    assert.equal(source.orderId, hashAbi(['bytes32', 'uint256', 'address', 'address', 'uint256'],
      [tag('vibeface-delivery-order-v1'), 31337n, escrowAddress, payerAddress, o.input.payerOrderNonce]));
    const preTerms = { ...source, domain: tag('vibeface-delivery-preterms-v1'), relation: 0n,
      preTermsHash: ZeroHash, manifestRoot: ZeroHash, quoteDigest: ZeroHash, providerQuoteNonce: ZeroHash,
      windowIndex: 0n, windowOffset: 0n, windowPlaintextBytes: 0n, windowCiphertextBytes: 0n,
      ciphertextHash: ZeroHash, capsuleHash: ZeroHash, plaintextCommitment: ZeroHash, windowDeliveryFee: 0n, windowReservationFee: 0n };
    assert.equal(source.preTermsHash, seal(preTerms));
    const leaves = o.specs.map(s => hashAbi(['bytes32', 'uint16', 'uint64', 'uint32', 'uint32', 'bytes32', 'bytes32', 'bytes32', 'uint256', 'uint256'],
      [tag('vibeface-delivery-window-v1'), s.index, s.offset, s.plaintextBytes, s.ciphertextBytes, s.ciphertextHash, s.capsuleHash,
        s.plaintextCommitment, s.deliveryFee, s.reservationFee]));
    assert.equal(source.manifestRoot, hashAbi(['bytes32', 'bytes32', 'bytes32[]'],
      [tag('vibeface-delivery-manifest-v1'), source.preTermsHash, leaves]));
    assert.equal(source.quoteDigest, hashAbi(['bytes32', 'bytes32', 'bytes32', 'bytes32'],
      [tag('vibeface-delivery-quote-v1'), source.preTermsHash, source.manifestRoot, o.input.providerQuoteNonce]));
    const key = object(await escrow.previewBinding({ ...emptyStatement(), relation: 1n, ...baseKey }));
    assert.equal(key.keyBindingHash, hashAbi(['bytes32', 'uint256', 'address', 'bytes32', 'bytes32', 'bytes32', 'uint64', 'uint64', 'uint64', 'uint32', 'uint8'],
      [tag('vibeface-delivery-key-v1'), 31337n, escrowAddress, key.receiverPeer, key.receiverPublic, key.receiverKeyId,
        key.receiverKeySerial, key.keyValidFrom, key.keyValidUntil, 1, 1]));
    const vector = { kind: 'canonical ABI/hash vector with synthetic identities; not a valid cryptographic delivery witness',
      profile, profileId: await escrow.profileId(), chainId: '31337', escrow: escrowAddress, input: o.input, windows: o.specs,
      keyStatement: encoded(key), keyDigest: seal(key), sourceStatements: o.statements.map(encoded), sourceDigests: o.statements.map(seal),
      preTermsHash: source.preTermsHash, windowHashes: leaves, manifestRoot: source.manifestRoot, quoteDigest: source.quoteDigest,
      walletDigest: await escrow.quoteWalletDigest(source.quoteDigest, source.providerPeer), walletSignature: o.signature };
    fs.writeFileSync(path.join(artifacts, 'solidity-vectors.json'), `${JSON.stringify(vector, (_, value) => typeof value === 'bigint' ? value.toString() : value, 2)}\n`);
  });
  await check('derived statement fields are rebuilt; phase must be source', async () => {
    const a = await make();
    const b = (await escrow.preview({ ...a.input, domain: tag('forged'), chainId: 7n, escrow: keeperAddress,
      profileId: tag('forged'), guestImageId: tag('forged'), orderId: tag('forged'), preTermsHash: tag('forged'),
      manifestRoot: tag('forged'), quoteDigest: tag('forged'), windowDeliveryFee: 999n }, a.specs))[0];
    assert.equal(seal(b), seal(a.statements[0]));
    await reject(() => escrow.preview({ ...a.input, relation: 3n }, a.specs));
  });
  await check('current key is registered from exact canonical key proof', async () => {
    const k = await escrow.keys(baseKey.receiverPeer);
    assert.equal(k.serial, 1n); assert.equal(k.active, true);
    await reject(() => register({ receiverKeySerial: 2n }, ZeroHash));
    assert.equal((await escrow.keys(baseKey.receiverPeer)).serial, 1n);
  });
  await check('key registration replay and decreasing serial are rejected', async () => {
    await reject(() => register());
    await register({ receiverKeySerial: 2n });
    await reject(() => register());
  });
  await check('future and expired key registration rejected', async () => {
    await reject(() => register({ receiverKeySerial: 2n, keyValidFrom: start + 1n }));
    await reject(() => register({ receiverKeySerial: 2n, keyValidFrom: start - 100n, keyValidUntil: start - 1n }));
  });
  await check('active and revoked key fields cannot be confused', async () => {
    await reject(() => register({ receiverKeySerial: 2n, receiverPublic: ZeroHash }));
    await reject(() => register({ receiverKeySerial: 2n, bindingAction: 2n }));
  });
  await check('fund conserves exact paid amount and records independent fees', async () => {
    const o = await funded(); const s = await order(o);
    assert.equal(s.terms.escrowAmount, 17n); assert.equal(s.earned, 0n); assert.equal(s.refunded, 0n);
    assert.equal((await window(o)).spec.reservationFee, 2n);
  });
  await check('different payer cannot spend allowance or front-run funding', async () => {
    const o = await make(); await reject(() => escrow.connect(keeper).fund(o.input, o.specs, o.seals, o.signature));
  });
  await check('duplicate funding never charges again', async () => {
    const o = await funded(); const balance = await token.balanceOf(payerAddress);
    await reject(() => escrow.fund(o.input, o.specs, o.seals, o.signature));
    assert.equal(await token.balanceOf(payerAddress), balance);
  });
  await check('wrong provider wallet quote signature rejected', async () => {
    const o = await make(); const wrong = await new Wallet(accounts[2].secretKey).signMessage('not EIP712');
    await reject(() => escrow.fund(o.input, o.specs, o.seals, wrong));
  });
  await check('quote wallet signature binds provider PeerID and escrow', async () => {
    const o = await make();
    const signature = await wallet.signTypedData({ name: 'VibefaceDelivery', version: '1', chainId: 31337, verifyingContract: keeperAddress },
      { DeliveryQuote: [{ name: 'quoteDigest', type: 'bytes32' }, { name: 'providerPeer', type: 'bytes32' }] },
      { quoteDigest: o.statements[0].quoteDigest, providerPeer: o.input.providerPeer });
    await reject(() => escrow.fund(o.input, o.specs, o.seals, signature));
    const changed = (await escrow.preview({ ...o.input, providerPeer: tag('another-provider') }, o.specs)).map(object);
    await reject(() => escrow.fund({ ...o.input, providerPeer: tag('another-provider') }, o.specs, changed.map(seal), o.signature));
  });
  await check('all per-window source seals are mandatory', async () => {
    const o = await make(2);
    await reject(() => escrow.fund(o.input, o.specs, [o.seals[0]], o.signature));
    await reject(() => escrow.fund(o.input, o.specs, [o.seals[0], o.seals[0]], o.signature));
  });
  await check('manifest coverage rejects gaps duplicates and wrong totals', async () => {
    const o = await make(2);
    await reject(() => escrow.preview(o.input, [o.specs[0], { ...o.specs[1], offset: 65n }]));
    await reject(() => escrow.preview(o.input, [o.specs[0], { ...o.specs[1], index: 0 }]));
    await reject(() => escrow.preview({ ...o.input, totalPlaintextBytes: 129n }, o.specs));
    await reject(() => escrow.preview({ ...o.input, escrowAmount: 28n }, o.specs));
  });
  await check('window limits and exact ciphertext overhead enforced', async () => {
    const o = await make();
    await reject(() => escrow.preview(o.input, [{ ...o.specs[0], plaintextBytes: 65537, ciphertextBytes: 65605 }]));
    await reject(() => escrow.preview(o.input, [{ ...o.specs[0], ciphertextBytes: 133 }]));
    await reject(() => escrow.preview(o.input, [{ ...o.specs[0], ciphertextHash: ZeroHash }]));
    await reject(() => make(17));
  });
  await check('one 64 KiB and two contiguous 64 KiB accounting windows fit profile', async () => {
    const a = await funded(1, {}, { plaintextBytes: 65536n, ciphertextBytes: 65604n });
    const b = await funded(2, {}, { plaintextBytes: 65536n, ciphertextBytes: 65604n });
    assert.equal((await order(a)).terms.totalPlaintextBytes, 65536n);
    assert.equal((await window(b, 1)).spec.offset, 65536n);
  });
  await check('deadline ordering and strictly later priority deadline enforced', async () => {
    const o = await make();
    await reject(() => escrow.preview({ ...o.input, priorityDeadline: o.input.quoteExpiresAt }, o.specs));
    await reject(() => escrow.preview({ ...o.input, priorityDeadline: o.input.keyDeadline + 1n }, o.specs));
    await reject(() => escrow.preview({ ...o.input, holdDeadline: o.input.keyDeadline }, o.specs));
    await reject(() => escrow.preview({ ...o.input, retainUntil: o.input.keyValidUntil + 1n }, o.specs));
  });
  await check('fund accepts quote expiry boundary and rejects next second', async () => {
    const a = await make(); await time(a.input.quoteExpiresAt); await fund(a);
    const b = await make(); await time(b.input.quoteExpiresAt + 1n);
    await reject(() => escrow.fund(b.input, b.specs, b.seals, b.signature));
  });
  await check('rotation invalidates old unheld key for funding', async () => {
    const o = await make(); await register({ receiverKeySerial: 2n, receiverKeyId: tag('two') });
    await reject(() => escrow.fund(o.input, o.specs, o.seals, o.signature));
  });
  await check('revoked key cannot fund or hold, unheld windows refund', async () => {
    const o = await funded();
    await register({ receiverKeySerial: 2n, receiverPublic: ZeroHash, receiverKeyId: ZeroHash, bindingAction: 2n });
    await reject(() => hold(o));
    await tx(escrow.connect(keeper).refund(o.id, 0));
    assert.equal((await order(o)).refunded, 17n);
  });
  await check('fund and hold require exact current binding hash', async () => {
    const o = await make(1, { receiverKeyId: tag('unregistered-id') });
    await reject(() => escrow.fund(o.input, o.specs, o.seals, o.signature));
  });
  await check('hold accepts deadline boundary and blocks subsequent hold replay', async () => {
    const o = await funded(); await time(o.input.holdDeadline); await hold(o);
    await reject(() => hold(o)); assert.equal((await window(o)).phase, 1n);
  });
  await check('hold one second late cannot lock funds', async () => {
    const o = await funded(); await time(o.input.holdDeadline + 1n); await reject(() => hold(o));
    await tx(escrow.refund(o.id, 0)); assert.equal((await window(o)).phase, 4n);
  });
  await check('hold proof cannot be replayed for another window or relation', async () => {
    const o = await funded(2); const first = await proof(o, 0, 3);
    await reject(() => escrow.hold(o.id, 1, first));
    await reject(() => escrow.hold(o.id, 0, o.seals[0]));
  });
  await check('normal recovery earns fees and bonus only with full capsule', async () => {
    const o = await funded(); await hold(o);
    await reject(() => escrow.publish(o.id, 0, '0x00'));
    await reject(() => escrow.publish(o.id, 0, hexlify(new Uint8Array(80).fill(9))));
    await publish(o); const s = await order(o);
    assert.equal(s.earned, 17n); assert.equal(s.bonusEarned, true);
    assert.equal((await window(o)).capsule, o.capsules[0]);
  });
  await check('publish without hold and duplicate publish cannot earn', async () => {
    const o = await funded(); await reject(() => publish(o)); await hold(o); await publish(o);
    await reject(() => publish(o)); assert.equal((await order(o)).earned, 17n);
  });
  await check('publish at key deadline earns fees but no late priority bonus', async () => {
    const o = await funded(); await hold(o); await time(o.input.keyDeadline); await publish(o);
    assert.equal((await order(o)).earned, 12n);
    await tx(escrow.refundBonus(o.id)); assert.equal((await order(o)).refunded, 5n);
  });
  await check('publish late cannot earn; held refund waits for hard deadline', async () => {
    const o = await funded(); await hold(o); await reject(() => escrow.refund(o.id, 0));
    await time(o.input.keyDeadline + 1n); await reject(() => publish(o)); await tx(escrow.refund(o.id, 0));
    assert.equal((await order(o)).refunded, 17n); assert.equal((await window(o)).capsule, '0x');
  });
  await check('rotation mixed-window race preserves held debt and refunds only unheld', async () => {
    const o = await funded(2); await hold(o, 0);
    await register({ receiverKeySerial: 2n, receiverKeyId: tag('two') });
    await reject(() => hold(o, 1)); await reject(() => escrow.refund(o.id, 0));
    await tx(escrow.refund(o.id, 1)); await publish(o, 0);
    const s = await order(o); assert.equal(s.earned, 12n); assert.equal(s.refunded, 17n);
    assert.equal(s.bonusEarned, false); assert.equal(s.bonusRefunded, true);
  });
  await check('normal cancellation requires payer and no held or earned windows', async () => {
    const a = await funded(2); await reject(() => escrow.connect(keeper).cancel(a.id));
    await tx(escrow.cancel(a.id)); assert.equal((await order(a)).refunded, 29n);
    const b = await funded(2); await hold(b); await reject(() => escrow.cancel(b.id));
    await publish(b); await reject(() => escrow.cancel(b.id));
  });
  await check('refunded windows are terminal against late hold publish and open', async () => {
    const o = await funded(); await tx(escrow.cancel(o.id));
    await reject(() => hold(o)); await reject(() => publish(o)); await reject(() => open(o));
    await reject(() => escrow.refund(o.id, 0)); await reject(() => escrow.cancel(o.id));
  });
  await check('strict publish stores capsule and locks cancellation without earning', async () => {
    const o = await funded(1, { acceptance: 1n }); await hold(o); await publish(o);
    assert.equal((await window(o)).phase, 2n); assert.equal((await order(o)).earned, 0n);
    await reject(() => escrow.cancel(o.id)); await reject(() => escrow.refund(o.id, 0));
  });
  await check('strict open at hard deadline earns fee without late bonus', async () => {
    const o = await funded(1, { acceptance: 1n }); await hold(o); await publish(o);
    await time(o.input.openDeadline); await open(o);
    assert.equal((await order(o)).earned, 12n); assert.equal((await window(o)).decryptionProven, true);
    await reject(() => open(o));
  });
  await check('strict open late rejected before refund and after terminal refund', async () => {
    const o = await funded(1, { acceptance: 1n }); await hold(o); await publish(o);
    await time(o.input.openDeadline + 1n); await reject(() => open(o));
    await tx(escrow.refund(o.id, 0)); await reject(() => open(o));
    assert.equal((await order(o)).earned, 0n); assert.equal((await window(o)).capsule, o.capsules[0]);
  });
  await check('ordinary open may strengthen evidence after deadline without new income', async () => {
    const o = await funded(); await hold(o); await publish(o);
    await time(o.input.openDeadline + 1n); await open(o);
    assert.equal((await order(o)).earned, 17n); assert.equal((await window(o)).decryptionProven, true);
  });
  await check('bonus is earned once only after every window finishes by priority deadline', async () => {
    const o = await funded(2); await hold(o, 0); await hold(o, 1); await publish(o, 0);
    assert.equal((await order(o)).earned, 12n); assert.equal((await order(o)).bonusEarned, false);
    await time(o.input.priorityDeadline); await publish(o, 1);
    assert.equal((await order(o)).earned, 29n); assert.equal((await order(o)).bonusEarned, true);
    await reject(() => escrow.refundBonus(o.id));
  });
  await check('bonus refunds independently after priority while held fees remain protected', async () => {
    const o = await funded(2); await hold(o, 0); await hold(o, 1);
    await reject(() => escrow.refundBonus(o.id));
    await time(o.input.priorityDeadline + 1n); await tx(escrow.refundBonus(o.id));
    assert.equal((await order(o)).refunded, 5n); await reject(() => escrow.refund(o.id, 0));
    await publish(o, 0); await publish(o, 1); assert.equal((await order(o)).earned, 24n);
    await reject(() => escrow.refundBonus(o.id));
  });
  await check('strict completion uses independent priority deadline', async () => {
    const o = await funded(1, { acceptance: 1n }); await hold(o); await publish(o);
    await time(o.input.priorityDeadline + 1n); await open(o);
    assert.equal((await order(o)).earned, 12n); await tx(escrow.refundBonus(o.id));
    assert.equal((await order(o)).refunded, 5n);
  });
  await check('partial refund cannot confiscate other earned or held fees', async () => {
    const o = await funded(3); await hold(o, 0); await hold(o, 1); await publish(o, 0);
    await time(o.input.holdDeadline + 1n); await tx(escrow.refund(o.id, 2));
    const s = await order(o); assert.equal(s.earned, 12n); assert.equal(s.refunded, 17n);
    assert.equal((await window(o, 1)).phase, 1n); await publish(o, 1); assert.equal((await order(o)).earned, 24n);
  });
  await check('keeper withdrawal pays only fixed provider and never duplicates income', async () => {
    const o = await funded(); await hold(o); await publish(o);
    const before = await token.balanceOf(wallet.address);
    await tx(escrow.connect(keeper).withdraw(o.id));
    assert.equal(await token.balanceOf(wallet.address), before + 17n);
    assert.equal(await token.balanceOf(keeperAddress), 0n); await reject(() => escrow.withdraw(o.id));
  });
  await check('withdrawal and partial refund remain separate across windows', async () => {
    const o = await funded(2); await hold(o); await publish(o); await tx(escrow.withdraw(o.id));
    await time(o.input.holdDeadline + 1n); await tx(escrow.refund(o.id, 1));
    const s = await order(o); assert.equal(s.earned, 12n); assert.equal(s.withdrawn, 12n); assert.equal(s.refunded, 17n);
    assert.equal((await window(o)).capsule, o.capsules[0]);
  });
  await check('non-returning ERC20 works for funding withdrawal and refund', async () => {
    await tx(token.configure(1)); const a = await funded(); await hold(a); await publish(a); await tx(escrow.withdraw(a.id));
    const b = await funded(); await tx(escrow.cancel(b.id));
    assert.equal((await order(a)).withdrawn, 17n); assert.equal((await order(b)).refunded, 17n);
  });
  for (const [mode, label] of [[2, 'false-returning'], [3, 'frozen'], [4, 'recipient-fee'], [6, 'sender-tax']]) {
    await check(`${label} ERC20 cannot short-fund escrow`, async () => {
      const o = await make(); await tx(token.configure(mode));
      await reject(() => escrow.fund(o.input, o.specs, o.seals, o.signature));
      await reject(() => order(o)); assert.equal(await token.balanceOf(escrowAddress), 0n);
    });
    await check(`${label} withdrawal failure preserves debt and retained capsule`, async () => {
      const o = await funded(); await hold(o); await publish(o); await tx(token.configure(mode));
      await reject(() => escrow.withdraw(o.id)); assert.equal((await order(o)).withdrawn, 0n);
      assert.equal((await order(o)).earned, 17n); assert.equal((await window(o)).capsule, o.capsules[0]);
      await tx(token.configure(0)); await tx(escrow.withdraw(o.id));
    });
    await check(`${label} refund failure does not terminate window or erase budget`, async () => {
      const o = await funded(); await tx(token.configure(mode)); await reject(() => escrow.cancel(o.id));
      assert.equal((await order(o)).refunded, 0n); assert.equal((await window(o)).phase, 0n);
      await tx(token.configure(0)); await tx(escrow.cancel(o.id));
    });
  }
  await check('asset precision mutation cannot silently settle different units', async () => {
    const o = await funded(); await hold(o); await publish(o); await tx(token.setDecimals(18));
    await reject(() => escrow.withdraw(o.id)); assert.equal((await order(o)).withdrawn, 0n);
    await tx(token.setDecimals(6)); await tx(escrow.withdraw(o.id));
  });
  await check('asset cannot mutate precision inside incoming transfer', async () => {
    const o = await make(); await tx(token.configure(7));
    await reject(() => escrow.fund(o.input, o.specs, o.seals, o.signature));
    assert.equal(await token.decimals(), 6n); await reject(() => order(o));
  });
  await check('asset cannot mutate precision inside outgoing transfer', async () => {
    const o = await funded(); await hold(o); await publish(o); await tx(token.configure(7));
    await reject(() => escrow.withdraw(o.id)); assert.equal((await order(o)).withdrawn, 0n);
    assert.equal(await token.decimals(), 6n); await tx(token.configure(0)); await tx(escrow.withdraw(o.id));
  });
  await check('ERC20 callback cannot reenter withdrawal', async () => {
    const o = await funded(); await hold(o); await publish(o);
    await tx(token.attack(escrowAddress, escrow.interface.encodeFunctionData('withdraw', [o.id]))); await tx(token.configure(5));
    await tx(escrow.withdraw(o.id)); assert.equal(await token.reentrySucceeded(), false);
    assert.equal((await order(o)).withdrawn, 17n);
  });
  await check('ERC20 callback cannot reenter funding or key mutation', async () => {
    const o = await make(); const input = { ...emptyStatement(), relation: 1n, ...baseKey, receiverKeySerial: 2n };
    const b = await escrow.previewBinding(input);
    await tx(token.attack(escrowAddress, escrow.interface.encodeFunctionData('registerKey', [input, seal(b)])));
    await tx(token.configure(5)); await fund(o);
    assert.equal(await token.reentrySucceeded(), false); assert.equal((await escrow.keys(baseKey.receiverPeer)).serial, 1n);
  });
  await check('production escrow rejects digest-only accounting seal', async () => {
    const production = await deploy('DeliveryEscrow', [profile]);
    const input = { ...emptyStatement(), relation: 1n, ...baseKey };
    const canonical = await production.previewBinding(input);
    await reject(() => production.registerKey(input, seal(canonical)));
    assert.equal((await production.keys(baseKey.receiverPeer)).serial, 0n);
  });
  const result = { kind: 'deterministic accounting verifier only; not new delivery ZK acceptance', passed: results.length,
    statementWords: 47, statementBytes: 1504, schemaHash: profile.statementSchemaHash,
    productionRuntimeBytes: artifact('DeliveryEscrow').deployedBytes, cases: results };
  fs.writeFileSync(path.join(artifacts, 'accounting-results.json'), `${JSON.stringify(result, null, 2)}\n`);
  console.log(JSON.stringify({ passed: results.length, kind: result.kind }));
} finally {
  await provider.destroy(); await rpc.disconnect();
}
