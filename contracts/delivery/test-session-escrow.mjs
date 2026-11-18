import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { compile, artifact, requireDependency } from './compile.mjs';
import { deployFixtureContract, withLocalEvm } from './local-evm-fixture.mjs';

const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, MaxUint256, ZeroHash, sha256, toUtf8Bytes } = requireDependency('ethers');
const repo = fileURLToPath(new URL('../../', import.meta.url));
const tag = s => sha256(toUtf8Bytes(s));
const tx = async promise => (await promise).wait();
const prover = process.env.VIBEFACE_SESSION_PROVER;
const proofDir = process.env.VIBEFACE_SESSION_PROOF_DIR;
if (prover || proofDir) {
  assert.ok(prover && isAbsolute(prover) && proofDir && isAbsolute(proofDir));
}
const runProver = (args, input) => JSON.parse(execFileSync(prover, args, {
  cwd: repo, input, encoding: 'utf8', stdio: ['pipe', 'pipe', 'inherit'],
}));
const info = prover ? runProver(['session-info']) : null;

compile({ checkTrust: true });
await withLocalEvm({ ganache, BrowserProvider }, async ({ rpc, provider }) => {
  const [payer, relay, keeper] = await Promise.all([0, 1, 2].map(i => provider.getSigner(i)));
  const deploy = (name, ...args) => deployFixtureContract(ContractFactory, artifact(name), payer, ...args);
  const token = await deploy('TestToken', payer.address);
  const relayPeer = tag('session-test-relay');
  const startTime = (await provider.getBlock('latest')).timestamp;
  const deadline = (startTime + (prover ? 24 * 60 * 60 : 300)) * 1000;
  async function budgetFor(name) {
    const image = name === 'SessionEscrow' && info ? `0x${info.imageId}` : tag('accounting-fixture-not-a-proof');
    const escrow = await deploy(name, await token.getAddress(), image);
    const salt = tag('session-budget-test');
    await tx(token.approve(await escrow.getAddress(), MaxUint256));
    const budget = await escrow.openBudget.staticCall(salt, relay.address, relayPeer, 100);
    await tx(escrow.openBudget(salt, relay.address, relayPeer, 100));
    const vector = JSON.parse(execFileSync('cargo', [
      'run', '--locked', '--quiet', '-p', 'vibeface-session-delivery', '--example', 'session-settlement-vector', '--',
      await escrow.deployment(), budget, relayPeer, String(deadline),
      ...(info && name === 'SessionEscrow' ? ['--include-test-claims'] : []),
    ], { cwd: repo, encoding: 'utf8' }));
    const terms = order => [order.delivery, order.context, order.amount, order.deadline];
    return { escrow, budget, vector, terms };
  }
  const { escrow, budget, vector, terms } = await budgetFor('TestSessionEscrow');
  const ids = vector.paid.orders.map(order => order.id);
  const expire = vector.expire.orders[0];
  const reserves = [...vector.paid.orders, expire].map(terms);

  await assert.rejects(escrow.connect(keeper).reserveBatch.staticCall(budget, reserves));
  await assert.rejects(escrow.reserveBatch.staticCall(budget, [[tag('too-expensive'), tag('context'), 101, deadline]]));
  await tx(escrow.reserveBatch(budget, reserves));
  await tx(escrow.reserveBatch(budget, reserves));
  for (const order of [...vector.paid.orders, expire]) {
    assert.equal((await escrow.reservations(budget, order.id)).commitment, order.commitment);
  }
  await assert.rejects(escrow.reserveBatch.staticCall(budget, [[...reserves[0].slice(0, 2), 1n, deadline]]));
  await assert.rejects(escrow.refundExpired.staticCall(budget, [expire.id]));
  await assert.rejects(escrow.settleBatch.staticCall(budget, [ids[0], ids[0]], vector.paid.journalDigest));
  await assert.rejects(tx(escrow.settleBatch(budget, ids, ZeroHash, { gasLimit: 500000 })));
  assert.equal((await escrow.budgets(budget)).credit, 0n);
  assert.equal((await escrow.reservations(budget, ids[0])).consumed, false);
  assert.equal(await token.balanceOf(relay.address), 0n);

  // Closing leaves funded orders live and returns only unallocated funds.
  await tx(escrow.closeBudget(budget));
  assert.equal(await token.balanceOf(await escrow.getAddress()), 9n);
  await tx(escrow.connect(keeper).settleBatch(budget, ids, vector.paid.journalDigest));
  assert.equal((await escrow.budgets(budget)).credit, 5n);
  await assert.rejects(escrow.settleBatch.staticCall(budget, ids, vector.paid.journalDigest));
  await tx(escrow.connect(keeper).withdraw(budget));
  assert.equal(await token.balanceOf(relay.address), 5n);
  await assert.rejects(escrow.withdraw.staticCall(budget));

  const production = await budgetFor('SessionEscrow');
  await tx(production.escrow.reserveBatch(production.budget, production.vector.paid.orders.map(production.terms)));
  await assert.rejects(production.escrow.settleBatch.staticCall(
    production.budget, production.vector.paid.orders.map(order => order.id), production.vector.paid.journalDigest,
  ));

  if (info) {
    const witness = JSON.stringify(production.vector.testClaims);
    const expected = production.vector.paid.journalDigest;
    const execution = runProver(['session-execute'], witness);
    assert.equal(`0x${execution.journalHash}`, expected);
    assert.equal(execution.proved, false);
    console.log(`Session guest execution matched the EVM journal (${execution.cycles} cycles); generating proof.`);
    const path = join(proofDir, 'session-groth16.receipt');
    const checkpoint = join(proofDir, 'session-succinct.receipt');
    const proof = runProver(['session-prove', path, '--succinct-receipt', checkpoint], witness);
    assert.equal(proof.imageId, info.imageId);
    assert.equal(`0x${proof.journalHash}`, expected);
    assert.equal(proof.kind, 'groth16');
    assert.deepEqual(runProver(['session-verify', proof.journal], readFileSync(path)), proof);
    const orderIds = production.vector.paid.orders.map(order => order.id);
    await tx(production.escrow.connect(keeper).settleBatch(production.budget, orderIds, `0x${proof.seal}`));
    await assert.rejects(production.escrow.settleBatch.staticCall(production.budget, orderIds, `0x${proof.seal}`));
    await tx(production.escrow.connect(keeper).withdraw(production.budget));
    assert.equal(await token.balanceOf(relay.address), 10n);
    console.log('Production verifier accepted a genuine session proof and the relay received the TestToken batch payout.');
  }

  await rpc.request({ method: 'evm_setTime', params: [expire.deadline + 1000] });
  await rpc.request({ method: 'evm_mine', params: [] });
  await assert.rejects(escrow.settleBatch.staticCall(budget, [expire.id], vector.expire.journalDigest));
  await tx(escrow.connect(keeper).refundExpired(budget, [expire.id]));
  assert.equal(await token.balanceOf(await escrow.getAddress()), 0n);
  await assert.rejects(escrow.refundExpired.staticCall(budget, [expire.id]));
});

if (!info) console.log('Rust journal and loopback session escrow agree; accounting passed with a test digest, not a ZK proof.');
