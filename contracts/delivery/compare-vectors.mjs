import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { deliveryAbi } from './abi-model.mjs';
import { compile, artifact, artifacts, requireDependency, schema } from './compile.mjs';
import { deployFixtureContract, withLocalEvm } from './local-evm-fixture.mjs';

if (!process.argv[2]) throw Error('Usage: node compare-vectors.mjs <public-vectors.json>');
const vectors = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const { BrowserProvider, ContractFactory, Contract, AbiCoder, sha256, toUtf8Bytes, hexlify, toBeHex, zeroPadValue } = requireDependency('ethers');
compile();
const a = artifact('AccountingTestEscrow');
const model = deliveryAbi(a, { accounting: true });
const configured = model.configuredProfile(vectors.profile);
const bytes = value => `0x${value}`;
assert.equal(configured.bytes, bytes(vectors.profileAbi).toLowerCase(), 'configured profile ABI bytes must match the Rust vector');
assert.equal(configured.id, bytes(vectors.profileId).toLowerCase(), 'configured profile ID must match the Rust vector');
const ganache = requireDependency('ganache');
await withLocalEvm({ ganache, BrowserProvider }, async ({ rpc, provider }) => {
  const coder = AbiCoder.defaultAbiCoder();
  const tag = value => sha256(toUtf8Bytes(value));
  const hex = value => hexlify(Uint8Array.from(value));
  const cases = [];
  function compare(name, actual, expected) {
    assert.equal(actual.toLowerCase(), expected.toLowerCase(), name);
    cases.push({ name, passed: true }); console.log(`PASS ${name}`);
  }
  const deployed = await deployFixtureContract(ContractFactory, a, await provider.getSigner(), configured.value, { gasLimit: 29000000 });
  const deployment = await deployed.getAddress();
  const fixedEscrow = hex(vectors.order.unsigned.terms.binding.escrow);
  // Only this disposable EVM transplants code/storage to reproduce Rust's fixed address vector.
  await rpc.request({ method: 'evm_setAccountCode', params: [fixedEscrow, await provider.getCode(deployment)] });
  for (let slot = 0; slot < 8; slot++) {
    const value = await provider.getStorage(deployment, slot);
    await rpc.request({ method: 'evm_setAccountStorageAt', params: [fixedEscrow, zeroPadValue(toBeHex(slot), 32), zeroPadValue(value === '0x' ? '0x00' : value, 32)] });
  }
  const escrow = new Contract(fixedEscrow, a.abi, provider);
  const onChainProfile = await escrow.getProfile();
  compare('schema exact source bytes', sha256(toUtf8Bytes(schema)), bytes(vectors.schemaHash));
  compare('profile canonical ABI bytes', model.encodeObservedProfile(onChainProfile), bytes(vectors.profileAbi));
  compare('profile ID', await escrow.profileId(), bytes(vectors.profileId));
  const source = model.decodeJournal(bytes(vectors.windows[0].statements[0].journal));
  await rpc.request({ method: 'evm_setAccountCode', params: [source.asset, '0x00'] });
  const specs = vectors.order.unsigned.windows.map(s => ({ index: s.index, offset: s.offset, plaintextBytes: s.plaintext_bytes,
    ciphertextBytes: s.ciphertext_bytes, ciphertextHash: hex(s.ciphertext_hash), capsuleHash: hex(s.capsule_hash),
    plaintextCommitment: hex(s.plaintext_commitment), deliveryFee: BigInt(s.delivery_fee), reservationFee: BigInt(s.reservation_fee) }));
  const keyInput = model.decodeJournal(bytes(vectors.keyJournal));
  const binding = await escrow.previewBinding(keyInput);
  compare('key_binding journal', model.encodeJournal(binding), bytes(vectors.keyJournal));
  compare('key binding payload hash', binding.keyBindingHash, sha256(bytes(vectors.keyBindingPayload)));
  const statements = await escrow.preview(source, specs);
  compare('order ID', statements[0].orderId, bytes(vectors.orderId));
  compare('PreTerms hash', statements[0].preTermsHash, bytes(vectors.preTermsHash));
  compare('manifest canonical parameter ABI bytes', coder.encode(['bytes32', 'bytes32', 'bytes32[]'],
    [tag('vibeface-delivery-manifest-v1'), statements[0].preTermsHash, vectors.windows.map(window => bytes(window.leafHash))]), bytes(vectors.manifestAbi));
  compare('full manifest root', statements[0].manifestRoot, bytes(vectors.manifestRoot));
  compare('quote digest', statements[0].quoteDigest, bytes(vectors.quoteDigest));
  compare('quote signing payload', statements[0].quoteDigest, bytes(vectors.quotePayload));
  compare('source signing payload', coder.encode(['bytes32', 'bytes32', 'bytes32', 'bytes32'],
    [statements[0].orderId, statements[0].preTermsHash, statements[0].manifestRoot, statements[0].quoteDigest]), bytes(vectors.sourcePayload));
  for (let i = 0; i < vectors.windows.length; ++i) {
    for (const expected of vectors.windows[i].statements) {
      const relation = expected.relation;
      assert([2, 3, 4].includes(relation), `unknown vector relation: ${relation}`);
      const actual = await escrow.previewRelation(source, specs, i, relation);
      const journal = model.encodeJournal(actual);
      compare(`window ${i} ${expected.relation} journal`, journal, bytes(expected.journal));
      compare(`window ${i} ${expected.relation} digest`, sha256(journal), bytes(expected.journalHash));
    }
    compare(`window ${i} leaf hash`, sha256(coder.encode(
      ['bytes32', 'uint16', 'uint64', 'uint32', 'uint32', 'bytes32', 'bytes32', 'bytes32', 'uint256', 'uint256'],
      [tag('vibeface-delivery-window-v1'), specs[i].index, specs[i].offset, specs[i].plaintextBytes, specs[i].ciphertextBytes,
        specs[i].ciphertextHash, specs[i].capsuleHash, specs[i].plaintextCommitment, specs[i].deliveryFee, specs[i].reservationFee])), bytes(vectors.windows[i].leafHash));
  }
  fs.writeFileSync(path.join(artifacts, 'cross-language-results.json'), `${JSON.stringify({
    kind: 'actual Solidity encoder on local EVM compared byte-for-byte against Rust public vectors; no proof acceptance claim',
    passed: cases.length, vectorFile: process.argv[2], cases,
  }, null, 2)}\n`);
  console.log(JSON.stringify({ passed: cases.length }));
});
