import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { compile, artifact, artifacts, requireDependency, schema } from './compile.mjs';

if (!process.argv[2]) throw Error('Usage: node compare-vectors.mjs <public-vectors.json>');
const vectors = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, Contract, AbiCoder, sha256, toUtf8Bytes, hexlify, toBeHex, zeroPadValue } = requireDependency('ethers');
compile();
const rpc = ganache.provider({ logging: { quiet: true }, chain: { chainId: 31337, hardfork: 'shanghai' } });
const provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 10;
const coder = AbiCoder.defaultAbiCoder();
const tag = value => sha256(toUtf8Bytes(value));
const bytes = value => `0x${value}`;
const hex = value => hexlify(Uint8Array.from(value));
const cases = [];
function compare(name, actual, expected) {
  assert.equal(actual.toLowerCase(), expected.toLowerCase(), name);
  cases.push({ name, passed: true }); console.log(`PASS ${name}`);
}
try {
  const p = vectors.profile;
  const profile = { domain: tag('vibeface-delivery-profile-v1'), guestImageId: hex(p.guest_image_id),
    statementSchemaHash: hex(p.statement_schema_hash), cryptoSuite: p.crypto_suite, maxWindows: p.max_windows,
    maxWindowBytes: p.max_window_bytes, maxTotalBytes: p.max_total_bytes, maxGuestCycles: p.max_guest_cycles,
    maxProverMemoryBytes: BigInt(p.max_prover_memory_bytes) };
  const a = artifact('AccountingTestEscrow');
  const fields = artifact('DeliveryEscrow').abi.find(x => x.name === 'preview').inputs[0].components;
  const statementType = `tuple(${fields.map(x => `${x.type} ${x.name}`).join(',')})`;
  const object = value => Object.fromEntries(fields.map(x => [x.name, value[x.name]]));
  const encode = value => coder.encode([statementType], [object(value)]);
  const profileFields = a.abi.find(x => x.type === 'constructor').inputs[0].components;
  const profileType = `tuple(${profileFields.map(x => `${x.type} ${x.name}`).join(',')})`;
  const deployed = await new ContractFactory(a.abi, a.bytecode, await provider.getSigner()).deploy(profile, { gasLimit: 29000000 });
  await deployed.waitForDeployment();
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
  compare('profile canonical ABI bytes', coder.encode([profileType], [onChainProfile]), bytes(vectors.profileAbi));
  compare('profile ID', await escrow.profileId(), bytes(vectors.profileId));
  const source = object(coder.decode([statementType], bytes(vectors.windows[0].statements[0].journal))[0]);
  await rpc.request({ method: 'evm_setAccountCode', params: [source.asset, '0x00'] });
  const specs = vectors.order.unsigned.windows.map(s => ({ index: s.index, offset: s.offset, plaintextBytes: s.plaintext_bytes,
    ciphertextBytes: s.ciphertext_bytes, ciphertextHash: hex(s.ciphertext_hash), capsuleHash: hex(s.capsule_hash),
    plaintextCommitment: hex(s.plaintext_commitment), deliveryFee: BigInt(s.delivery_fee), reservationFee: BigInt(s.reservation_fee) }));
  const keyInput = object(coder.decode([statementType], bytes(vectors.keyJournal))[0]);
  const binding = await escrow.previewBinding(keyInput);
  compare('key_binding journal', encode(binding), bytes(vectors.keyJournal));
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
      compare(`window ${i} ${expected.relation} journal`, encode(actual), bytes(expected.journal));
      compare(`window ${i} ${expected.relation} digest`, sha256(encode(actual)), bytes(expected.journalHash));
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
} finally { await provider.destroy(); await rpc.disconnect(); }
