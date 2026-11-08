import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { compile, artifact, artifacts, requireDependency } from './compile.mjs';

const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, Wallet, TypedDataEncoder, Signature, keccak256, toUtf8Bytes, AbiCoder } = requireDependency('ethers');
compile({ checkTrust: true });
const rpc = ganache.provider({ logging: { quiet: true }, wallet: { deterministic: true },
  chain: { chainId: 31337, hardfork: 'shanghai' }, miner: { timestampIncrement: 0 } });
const provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 10;
const payer = await provider.getSigner(0), payee = await provider.getSigner(1), keeper = await provider.getSigner(2);
const payerAddress = await payer.getAddress(), payeeAddress = await payee.getAddress();
const signer = new Wallet(Object.values(rpc.getInitialAccounts())[0].secretKey);
const tag = text => keccak256(toUtf8Bytes(text));
const tx = async promise => (await promise).wait();
const reject = async action => assert.rejects(async () => { const result = await action(); if (result?.wait) await result.wait(); });
const types = { CumulativeVoucher: [ { name: 'channelId', type: 'bytes32' }, { name: 'sequence', type: 'uint64' },
  { name: 'totalAmount', type: 'uint256' }, { name: 'receiptCommitment', type: 'bytes32' } ] };
const results = [];
async function deploy(name, args) {
  const built = artifact(name);
  const contract = await new ContractFactory(built.abi, built.bytecode, payer).deploy(...args);
  return contract.waitForDeployment();
}
try {
  const token = await deploy('TestToken', [payerAddress]);
  const channel = await deploy('CumulativeChannel', [await token.getAddress()]);
  const address = await channel.getAddress();
  const domain = { name: 'VibefaceCumulativeChannel', version: '1', chainId: 31337, verifyingContract: address };
  const voucher = (id, sequence, totalAmount) => ({ channelId: id, sequence, totalAmount, receiptCommitment: tag(`synthetic-receipt-${sequence}`) });
  const sign = value => signer.signTypedData(domain, types, value);
  const redeem = async (value, signature = null) => tx(channel.connect(keeper).redeemChannel(value.channelId, value.sequence,
    value.totalAmount, value.receiptCommitment, signature ?? await sign(value)));
  await tx(token.approve(address, 1000n));
  const salt = tag('payer-owned-unique-salt');
  const id = await channel.channelId(payerAddress, salt);
  assert.equal(id, keccak256(AbiCoder.defaultAbiCoder().encode(['uint256', 'address', 'address', 'bytes32'], [31337, address, payerAddress, salt])));
  // Another sender cannot occupy the payer's channel ID, even with the same public salt.
  await tx(token.transfer(await keeper.getAddress(), 1n));
  await tx(token.connect(keeper).approve(address, 1n));
  await tx(channel.connect(keeper).openChannel(salt, payeeAddress, tag('other-peer'), 1n));
  assert.notEqual(await channel.channelId(await keeper.getAddress(), salt), id);
  await tx(channel.openChannel(salt, payeeAddress, tag('provider-peer'), 100n));
  await reject(() => channel.openChannel(salt, payeeAddress, tag('provider-peer'), 100n));
  assert.equal((await channel.getChannel(id)).budget, 100n);
  results.push('payer-scoped unique ID, independent locked deposit and no ID reuse');

  const charges = [voucher(id, 1, 7n), voucher(id, 2, 18n), voucher(id, 3, 31n)];
  for (const value of charges) assert.equal(await channel.voucherDigest(value.channelId, value.sequence, value.totalAmount,
    value.receiptCommitment), TypedDataEncoder.hash(domain, types, value));
  const vectorDomain = { ...domain, verifyingContract: '0x3333333333333333333333333333333333333333' };
  assert.equal(TypedDataEncoder.hash(vectorDomain, types, { channelId: '0x' + '66'.repeat(32), sequence: 1,
    totalAmount: 7, receiptCommitment: '0x' + '77'.repeat(32) }), '0xd00a013edebccda322a7bf6b666c2122e510f5bd4b805f6ce53d71dac8713f54');
  results.push('Solidity EIP-712 digest and shared Rust vector');
  const before = await token.balanceOf(payeeAddress);
  await redeem(charges[0]);
  assert.equal(await token.balanceOf(payeeAddress) - before, 7n);
  await reject(() => redeem(charges[0]));
  await reject(() => redeem(voucher(id, 1, 8n)));
  await reject(() => redeem(voucher(id, 4, 101n)));
  await reject(() => redeem(voucher(id, 2, 7n)));
  await reject(async () => redeem(charges[1], await signer.signTypedData({ ...domain, chainId: 31338 }, types, charges[1])));
  await reject(async () => redeem(charges[1], await signer.signTypedData({ ...domain, verifyingContract: await token.getAddress() }, types, charges[1])));
  await reject(() => redeem(charges[1], '0x' + '00'.repeat(65)));
  const signed = Signature.from(await sign(charges[1]));
  const curveOrder = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141n;
  const highS = signed.r + (curveOrder - BigInt(signed.s)).toString(16).padStart(64, '0') + (signed.v === 27 ? '1c' : '1b');
  await reject(() => redeem(charges[1], highS));
  const secondSalt = tag('second-payee-budget');
  const secondId = await channel.channelId(payerAddress, secondSalt);
  await tx(channel.openChannel(secondSalt, await keeper.getAddress(), tag('second-provider'), 50n));
  await reject(async () => redeem({ ...charges[1], channelId: secondId }, await sign(charges[1])));
  assert.equal((await channel.getChannel(secondId)).redeemed, 0n);
  await redeem(charges[2]);
  assert.equal(await token.balanceOf(payeeAddress) - before, 31n);
  await reject(() => redeem(charges[1]));
  assert.equal((await channel.getChannel(id)).redeemed, 31n);
  results.push('several off-chain charges, partial/skipped redemption, duplicate/stale/overbudget/domain/malformed-signature/cross-channel rejection');

  await reject(() => channel.connect(keeper).closeChannel(id));
  await tx(channel.closeChannel(id));
  await reject(() => channel.refundChannel(id));
  await redeem(voucher(id, 4, 40n));
  await reject(() => channel.closeChannel(id));
  const closesAt = (await channel.getChannel(id)).closesAt;
  await rpc.request({ method: 'evm_setTime', params: [Number(closesAt) * 1000] });
  await rpc.request({ method: 'evm_mine', params: [] });
  await reject(() => redeem(voucher(id, 5, 41n)));
  const payerBeforeRefund = await token.balanceOf(payerAddress);
  await tx(channel.connect(keeper).refundChannel(id));
  assert.equal(await token.balanceOf(payerAddress) - payerBeforeRefund, 60n);
  assert.equal((await channel.getChannel(id)).state, 2n);
  await reject(() => channel.refundChannel(id));
  await reject(() => channel.openChannel(salt, payeeAddress, tag('provider-peer'), 100n));
  await reject(() => redeem(voucher(id, 5, 41n)));
  assert.equal(await token.balanceOf(address), 51n, 'other channel deposits remain independently locked');
  results.push('payer-only close, challenge-window claim, exact cutoff, untrusted-keeper refund and terminal states');

  for (const mode of [2, 3, 4, 6, 7]) {
    const badSalt = tag(`bad-deposit-${mode}`);
    await tx(token.configure(mode));
    await reject(() => channel.openChannel(badSalt, payeeAddress, tag('provider-peer'), 10n));
    await tx(token.configure(0));
    await reject(async () => channel.getChannel(await channel.channelId(payerAddress, badSalt)));
  }
  const attackSalt = tag('reentrancy-deposit');
  const attackId = await channel.channelId(payerAddress, attackSalt);
  const nested = voucher(secondId, 1, 1n);
  await tx(token.attack(address, channel.interface.encodeFunctionData('redeemChannel', [secondId, nested.sequence,
    nested.totalAmount, nested.receiptCommitment, await sign(nested)])));
  await tx(token.configure(5));
  await tx(channel.openChannel(attackSalt, payeeAddress, tag('provider-peer'), 10n));
  assert.equal(await token.reentrySucceeded(), false);
  await tx(token.configure(0));
  await redeem(nested);
  for (const mode of [2, 3, 4, 6, 7]) {
    await tx(token.configure(mode));
    await reject(() => redeem(voucher(attackId, 1, 3n)));
    assert.equal((await channel.getChannel(attackId)).redeemed, 0n, 'failed outgoing transfer rolls back cumulative claim');
  }
  await tx(token.configure(1));
  await redeem(voucher(attackId, 1, 10n));
  assert.equal((await channel.getChannel(attackId)).redeemed, 10n);
  results.push('false-return/frozen/fee/extra-debit/precision-changing tokens roll back; reentry blocked; exact no-return token works; full redemption');
  fs.writeFileSync(path.join(artifacts, 'channel-results.json'), JSON.stringify({ passed: true,
    scope: 'synthetic local settlement, no Node delivery proof or ZK fairness', results }, null, 2) + '\n');
  for (const result of results) console.log(`PASS ${result}`);
} finally {
  provider.destroy();
  await rpc.disconnect();
}
