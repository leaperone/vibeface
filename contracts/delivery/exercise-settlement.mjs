import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
import { observe, parseArguments, validateBundle, observationCase } from './observe-settlement.mjs';

function selfTest() {
  const hash = n => `0x${n.toString(16).padStart(64, '0')}`;
  const orders = [1, 2].map(n => ({ orderId: hash(n), windowCount: 1, amount: '5' }));
  const bundle = { schemaVersion: 2, escrow: `0x${'11'.repeat(20)}`, asset: `0x${'22'.repeat(20)}`, budgetId: hash(3), orders,
    proofs: [{ relation: 1, public: 'key.json', receipt: 'key.receipt' }, ...orders.flatMap((order, i) => [2, 3].map(relation =>
      ({ relation, orderId: order.orderId, windowIndex: 0, public: `order-${i}-relation-${relation}.json`, receipt: `order-${i}-relation-${relation}.receipt` })))],
    capsules: orders.map(order => ({ orderId: order.orderId, windowIndex: 0, capsule: '33'.repeat(80) })),
    transactions: { registerKey: hash(10), approve: hash(11), openBudget: hash(12), reserveBatch: [hash(13)], holdBatch: [hash(14)], revealBatch: [hash(15)], withdrawBudget: [hash(16)] } };
  assert.equal(validateBundle(bundle), bundle);
  const mutate = fn => { const changed = structuredClone(bundle); fn(changed); assert.throws(() => validateBundle(changed)); };
  mutate(value => { value.schemaVersion = 1; });
  mutate(value => { value.orders[1].orderId = value.orders[0].orderId; });
  mutate(value => { value.orders[0].windowCount = 2; });
  mutate(value => { value.proofs.pop(); });
  mutate(value => { value.proofs[1].windowIndex = 1; });
  mutate(value => { value.proofs[1].orderId = hash(999); });
  mutate(value => { value.proofs[0].receipt = '../private.receipt'; });
  mutate(value => { value.transactions.revealBatch[0] = value.transactions.holdBatch[0]; });
  mutate(value => { value.capsules[0].capsule = '33'.repeat(79); });
  mutate(value => { value.capsules[1] = value.capsules[0]; });
  mutate(value => { value.transactions.raw = 'unbroadcast'; });
  mutate(value => { value.transactions.deployments = [hash(17), hash(18)]; });
  assert.throws(() => observationCase('strict'));
  assert.throws(() => observationCase('ordinary'));
  assert.equal(parseArguments(['--settlement', '/fixture/public.json', '--rpc-url', 'http://127.0.0.1:8545']).caseName, 'protected');
  assert.throws(() => parseArguments(['--settlement', '/fixture/public.json', '--rpc-url', 'https://example.com']));
  console.log('PASS protected evidence boundaries: two distinct slots, exact proof coverage, relative public paths, unique transactions and no private raw export; no chain, Node or proof ran');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv.length === 3 && process.argv[2] === '--self-test') selfTest();
  else {
    try {
      assert(!process.argv.includes('--deployment'), 'the old Strict snapshot/mutation protocol is retired; use run.mjs for protected money regressions and this command with a protected public bundle for read-only genuine verification');
      const result = await observe(parseArguments(process.argv.slice(2)));
      console.log(JSON.stringify(result, null, 2)); process.exitCode = result.status === 'PASS' ? 0 : result.status === 'UNVERIFIED' ? 2 : 1;
    } catch (error) { console.log(JSON.stringify({ schemaVersion: 2, status: 'FAIL', error: error.message })); process.exitCode = 1; }
  }
}
