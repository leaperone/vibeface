import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { compile, artifact, artifacts, requireDependency, root, schema } from './compile.mjs';

const canonicalProfile = JSON.parse(fs.readFileSync(path.resolve(root, '../../crates/delivery/proof/profile.json'), 'utf8'));
const directory = process.argv[2] ? path.resolve(process.argv[2]) : path.resolve(root, '../../crates/delivery/proof/fixtures');
const read = name => {
  const file = path.join(directory, name);
  assert(fs.lstatSync(file).isFile(), `Fixture must be a regular file: ${name}`);
  return fs.readFileSync(file);
};
const manifest = JSON.parse(read('manifest.json'));
assert.equal(manifest.schemaVersion, 1, 'fixture manifest schema');
const hex = value => { assert.match(value, /^[0-9a-f]+$/); assert.equal(value.length % 2, 0); return `0x${value}`; };
for (const name of ['imageId', 'schemaHash', 'elfSha256']) { hex(manifest[name]); assert.equal(manifest[name].length, 64, name); }
assert.equal(manifest.proofs?.length, 4, 'all four relations required');
const publicProofs = manifest.proofs.map((entry, index) => {
  assert.equal(entry.receipt, `relation-${index + 1}.receipt`);
  assert.equal(entry.public, `relation-${index + 1}.json`);
  assert(read(entry.receipt).length > 0, 'binary receipt required; host separately verifies receipt-to-metadata binding');
  const proof = JSON.parse(read(entry.public));
  assert.equal(proof.mode, 'prove'); assert.equal(proof.kind, 'groth16'); assert.equal(proof.relation, index + 1);
  assert.equal(proof.imageId, manifest.imageId);
  assert.equal(proof.profileId.length, 64);
  for (const name of ['imageId', 'profileId', 'journal', 'journalHash', 'seal']) hex(proof[name]);
  return proof;
});
const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, AbiCoder, sha256, toUtf8Bytes, getBytes, hexlify, ZeroAddress } = requireDependency('ethers');
const profileKeys = ['guest_image_id', 'statement_schema_hash', 'crypto_suite', 'max_windows', 'max_window_bytes',
  'max_total_bytes', 'max_guest_cycles', 'max_prover_memory_bytes'];
assert.deepEqual(Object.keys(canonicalProfile).sort(), profileKeys.toSorted(), 'exact product profile fields');
const word = value => {
  assert(Array.isArray(value) && value.length === 32 && value.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255), 'profile Word must contain exactly 32 bytes');
  return hexlify(Uint8Array.from(value));
};
for (const name of profileKeys.slice(2)) assert(Number.isSafeInteger(canonicalProfile[name]) && canonicalProfile[name] > 0, `profile ${name}`);
const profile = {
  domain: sha256(toUtf8Bytes('vibeface-delivery-profile-v1')), guestImageId: word(canonicalProfile.guest_image_id),
  statementSchemaHash: word(canonicalProfile.statement_schema_hash), cryptoSuite: canonicalProfile.crypto_suite,
  maxWindows: canonicalProfile.max_windows, maxWindowBytes: canonicalProfile.max_window_bytes,
  maxTotalBytes: canonicalProfile.max_total_bytes, maxGuestCycles: canonicalProfile.max_guest_cycles,
  maxProverMemoryBytes: canonicalProfile.max_prover_memory_bytes,
};
assert.equal(hex(manifest.imageId), profile.guestImageId, 'fixture image must match fixed product profile');
assert.equal(hex(manifest.schemaHash), profile.statementSchemaHash, 'fixture schema must match fixed product profile');
assert.equal(sha256(toUtf8Bytes(schema)), profile.statementSchemaHash, 'product profile must match exact canonical ABI source');
compile();
const fields = artifact('DeliveryEscrow').abi.find(x => x.name === 'preview').inputs[0].components;
const statementType = `tuple(${fields.map(x => `${x.type} ${x.name}`).join(',')})`;
const coder = AbiCoder.defaultAbiCoder();
const profileFields = artifact('DeliveryEscrow').abi.find(x => x.type === 'constructor').inputs[0].components;
const profileType = `tuple(${profileFields.map(x => `${x.type} ${x.name}`).join(',')})`;
const expectedProfileId = sha256(coder.encode([profileType], [profile]));
let common;
let ordinary;
for (const proof of publicProofs) {
  assert.equal(getBytes(hex(proof.journal)).length, 1504, 'exact 47-word journal');
  const statement = coder.decode([statementType], hex(proof.journal))[0];
  assert.equal(coder.encode([statementType], [statement]), hex(proof.journal), 'canonical ABI roundtrip');
  assert.equal(statement.domain, sha256(toUtf8Bytes('vibeface-delivery-statement-v1')));
  assert.equal(statement.relation, BigInt(proof.relation));
  assert.equal(statement.chainId, 31337n, 'local fixture chain');
  assert.notEqual(statement.escrow, ZeroAddress);
  assert.equal(hex(proof.profileId), expectedProfileId, 'proof metadata must match fixed product profile ID');
  assert.equal(statement.guestImageId, profile.guestImageId, 'journal must match fixed product image');
  assert.equal(statement.profileId, expectedProfileId, 'journal must match fixed product profile ID');
  if (proof.relation !== 1) {
    assert.equal(statement.cryptoSuite, BigInt(profile.cryptoSuite));
    assert(statement.windowCount > 0n && statement.windowCount <= BigInt(profile.maxWindows), 'journal window count within product profile');
    assert(statement.windowPlaintextBytes <= BigInt(profile.maxWindowBytes), 'journal window bytes within product profile');
    assert(statement.totalPlaintextBytes <= BigInt(profile.maxTotalBytes), 'journal total bytes within product profile');
  }
  assert.equal(sha256(hex(proof.journal)), hex(proof.journalHash));
  const identity = [statement.escrow, statement.profileId, statement.receiverPeer, statement.receiverPublic,
    statement.receiverKeyId, statement.receiverKeySerial.toString(), statement.keyBindingHash].join(',');
  if (common) assert.equal(identity, common, 'all relation fixtures share deployment/profile/receiving key');
  common = identity;
  if (proof.relation !== 1) {
    const journal = getBytes(hex(proof.journal)); journal.fill(0, 32, 64);
    if (ordinary) assert.equal(hexlify(journal), ordinary, 'source/hold/open fixtures share exact order and window');
    ordinary = hexlify(journal);
  }
}
const rpc = ganache.provider({ logging: { quiet: true }, chain: { chainId: 31337, hardfork: 'shanghai' } });
const provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 10;
try {
  const signer = await provider.getSigner();
  const a = artifact('RiscZeroAdapter');
  const deploy = async image => {
    const adapter = await new ContractFactory(a.abi, a.bytecode, signer).deploy(image);
    await adapter.waitForDeployment(); return adapter;
  };
  const mutate = (value, index = 0) => { const bytes = getBytes(value); assert(bytes.length > index); bytes[index] ^= 1; return hexlify(bytes); };
  const adapter = await deploy(profile.guestImageId);
  const wrongImage = await deploy(mutate(profile.guestImageId));
  const cases = [];
  async function check(name, value) { assert.equal(value, true, name); cases.push({ name, passed: true }); console.log(`PASS ${name}`); }
  for (const proof of publicProofs) {
    const seal = hex(proof.seal), journalDigest = hex(proof.journalHash);
    await check(`relation ${proof.relation} current delivery image genuine Groth16 seal`, await adapter.verify(seal, journalDigest));
    await check(`relation ${proof.relation} mutated seal rejected`, !await adapter.verify(mutate(seal, 5), journalDigest));
    await check(`relation ${proof.relation} mutated journal rejected`, !await adapter.verify(seal, sha256(mutate(hex(proof.journal)))));
    await check(`relation ${proof.relation} wrong image rejected`, !await wrongImage.verify(seal, journalDigest));
    await check(`relation ${proof.relation} wrong selector rejected`, !await adapter.verify(mutate(seal), journalDigest));
  }
  const result = { kind: 'genuine current delivery image seals verified by production adapter on local EVM; not escrow funding acceptance',
    imageId: profile.guestImageId.slice(2), profileId: expectedProfileId, passed: cases.length, cases };
  fs.writeFileSync(path.join(artifacts, 'delivery-proof-results.json'), `${JSON.stringify(result, null, 2)}\n`);
  console.log(JSON.stringify({ passed: result.passed, imageId: result.imageId, kind: result.kind }));
} finally { await provider.destroy(); await rpc.disconnect(); }
