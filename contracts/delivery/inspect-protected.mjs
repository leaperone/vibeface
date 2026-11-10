import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { artifact, requireDependency } from './compile.mjs';

const input = fs.readFileSync(0);
assert(input.length <= 65536, 'bounded public chain request');
const request = JSON.parse(input);
const url = new URL(request.rpc_url);
assert(url.protocol === 'http:' && ['127.0.0.1', '[::1]'].includes(url.hostname)
  && url.port && !url.username && !url.password && ['/', ''].includes(url.pathname)
  && !url.search && !url.hash, 'literal loopback RPC only');
assert(path.resolve(request.repository) === path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..'), 'current repository only');
assert(request.transactions.length <= 64 && request.orders.length <= 2, 'bounded task inputs');
assert(request.transactions.every(value => /^0x[0-9a-f]{64}$/.test(value))
  && request.orders.every(value => /^0x[0-9a-f]{64}$/.test(value)), 'public hash inputs');
const { Contract, JsonRpcProvider, Interface, keccak256 } = requireDependency('ethers');
const provider = new JsonRpcProvider(url.href, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 100;
const escrowArtifact = artifact('DeliveryEscrow');
const escrow = new Contract(request.escrow, escrowArtifact.abi, provider);
const token = new Contract(request.asset, artifact('TestToken').abi, provider);
const tokenInterface = new Interface(artifact('TestToken').abi);
const serial = value => JSON.parse(JSON.stringify(value, (_, item) => typeof item === 'bigint' ? item.toString() : item));
try {
  assert.equal(Number((await provider.getNetwork()).chainId), 31337, 'test chain only');
  const latest = await provider.getBlockNumber();
  assert(latest >= 1, 'two confirmation anchor exists');
  const anchor = await provider.getBlock(latest - 1);
  const report = { anchor: { number: anchor.number, hash: anchor.hash, timestamp: anchor.timestamp }, transactions: [], windows: [], balances: {}, runtime_code_hashes: {} };
  for (const hash of request.transactions) {
    const receipt = await provider.getTransactionReceipt(hash);
    assert(receipt && receipt.status === 1 && receipt.blockNumber <= anchor.number, 'task transaction confirmed at anchor');
    const canonical = await provider.getBlock(receipt.blockNumber);
    assert.equal(canonical.hash, receipt.blockHash, 'canonical transaction receipt');
    const tx = await provider.getTransaction(hash);
    assert(tx && tx.hash === hash, 'actual public transaction');
    let parsed;
    if (tx.to?.toLowerCase() === request.escrow.toLowerCase()) parsed = escrow.interface.parseTransaction({ data: tx.data, value: tx.value });
    else if (tx.to?.toLowerCase() === request.asset.toLowerCase()) parsed = tokenInterface.parseTransaction({ data: tx.data, value: tx.value });
    assert(parsed, 'known task contract action');
    const fact = { hash, method: parsed.name, from: tx.from.toLowerCase(), to: tx.to.toLowerCase(), nonce: tx.nonce,
      block_number: receipt.blockNumber, block_hash: receipt.blockHash, gas_used: receipt.gasUsed.toString(), slots: [] };
    if (parsed.name === 'reserveBatch') {
      fact.slots = parsed.args.reservations.flatMap(item => item.specs.map(spec => ({ orderId: item.input.orderId, windowIndex: Number(spec.index) })));
    } else if (['holdBatch', 'revealBatch'].includes(parsed.name)) {
      fact.slots = parsed.args.items.map(item => ({ orderId: item.orderId, windowIndex: Number(item.index) }));
    }
    report.transactions.push(fact);
  }
  for (const address of request.accounts) report.balances[address.toLowerCase()] = (await token.balanceOf(address, { blockTag: anchor.number })).toString();
  if (request.budget_id) {
    assert(/^0x[0-9a-f]{64}$/.test(request.budget_id), 'budget hash');
    const budget = await escrow.getBudget(request.budget_id, { blockTag: anchor.number });
    report.budget = Object.fromEntries(['payer', 'payee', 'providerPeer', 'asset', 'deposit', 'locked', 'earned', 'withdrawn', 'refunded', 'maxClaimUntil', 'state'].map(name => [name, serial(budget[name])]));
  }
  const statement = escrowArtifact.abi.find(item => item.name === 'publicValues').outputs[0];
  assert(statement.components.length === 47, 'canonical 47-word statement');
  const { AbiCoder } = requireDependency('ethers');
  const coder = AbiCoder.defaultAbiCoder();
  for (const orderId of request.orders) {
    const window = await escrow.getWindow(orderId, 0, { blockTag: anchor.number });
    const fact = { orderId, windowIndex: 0, phase: Number(window.phase), public_values: {} };
    for (const relation of [2, 3]) {
      const value = await escrow.publicValues(orderId, 0, relation, { blockTag: anchor.number });
      fact.public_values[relation] = coder.encode([statement], [value]).slice(2);
    }
    if (request.include_capsules) {
      assert(/^0x[0-9a-fA-F]{160}$/.test(window.capsule), 'confirmed public 80-byte capsule');
      fact.capsule = window.capsule.slice(2).toLowerCase();
    }
    report.windows.push(fact);
  }
  for (const [name, address] of Object.entries(request.contracts)) {
    const code = await provider.getCode(address, anchor.number);
    assert(code !== '0x', 'deployed contract code');
    report.runtime_code_hashes[name] = keccak256(code);
  }
  assert.equal((await provider.getBlock(anchor.number))?.hash, anchor.hash, 'canonical anchor changed during inspection');
  const encoded = JSON.stringify(report);
  assert(encoded.length <= 131072, 'bounded public chain response');
  process.stdout.write(encoded + '\n');
} finally {
  provider.destroy();
}
