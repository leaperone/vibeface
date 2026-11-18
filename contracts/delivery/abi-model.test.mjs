import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { deliveryAbi } from './abi-model.mjs';
import { artifact, compile, requireDependency } from './compile.mjs';
import { recordedPreviews } from './observe-settlement.mjs';

const { AbiCoder, Contract, Interface, ZeroHash } = requireDependency('ethers');
const readJson = relative => JSON.parse(fs.readFileSync(new URL(relative, import.meta.url)));
const profile = readJson('../../crates/delivery/proof/profile.json');
compile({ checkTrust: true });
const escrow = artifact('DeliveryEscrow');
const accounting = artifact('AccountingTestEscrow');
const model = deliveryAbi(escrow);

test('configured and observed ABI values match the fixed receipt profile ID', () => {
  const proof = readJson('../../crates/delivery/proof/fixtures/relation-1.json');
  const configured = model.configuredProfile(profile);
  assert.equal(configured.id, `0x${proof.profileId}`);
  assert.deepEqual(deliveryAbi(accounting, { accounting: true }).configuredProfile(profile), configured);
  const observed = AbiCoder.defaultAbiCoder().decode(
    escrow.abi.find(entry => entry.name === 'getProfile').outputs, configured.bytes,
  )[0];
  assert.equal(model.encodeObservedProfile(observed), configured.bytes);
  const otherDomain = `0x${'00'.repeat(32)}`;
  assert.equal(model.encodeObservedProfile([otherDomain, ...observed.slice(1)]), `${otherDomain}${configured.bytes.slice(66)}`);
});

test('strict profile rejects missing and extra keys and lossy byte or integer conversions', () => {
  const { max_windows: _, ...missing } = profile;
  assert.throws(() => model.configuredProfile(missing), /missing proof profile field max_windows/);
  assert.throws(() => model.configuredProfile({ ...profile, domain: '0x' }), /unexpected proof profile field domain/);
  for (const guest_image_id of [profile.guest_image_id.slice(1), Array(32).fill(256), Array(32)]) {
    assert.throws(() => model.configuredProfile({ ...profile, guest_image_id }), /guest_image_id must contain exactly 32 bytes/);
  }
  for (const [field, value, type] of [
    ['crypto_suite', 256, 'uint8'],
    ['max_windows', 65536, 'uint16'],
    ['max_window_bytes', 2 ** 32, 'uint32'],
    ['max_total_bytes', -1, 'uint64'],
    ['max_guest_cycles', '64000000', 'uint64'],
    ['max_prover_memory_bytes', Number.MAX_SAFE_INTEGER + 1, 'uint64'],
  ]) {
    assert.throws(() => model.configuredProfile({ ...profile, [field]: value }), new RegExp(`${field}.*${type}`));
  }
});

test('all four receipt journals roundtrip and alternate encodings are rejected', () => {
  for (const relation of [1, 2, 3, 4]) {
    const proof = readJson(`../../crates/delivery/proof/fixtures/relation-${relation}.json`);
    const journal = `0x${proof.journal}`;
    const statement = model.decodeJournal(journal);
    assert.equal(model.journalBytes, 1504);
    assert.equal(statement.relation, BigInt(relation));
    assert.equal(statement.profileId, `0x${proof.profileId}`);
    assert.equal(model.encodeJournal(statement), journal);
    assert.throws(() => model.decodeJournal(`${journal}00`), /exact delivery journal byte length/);
    const relationPadding = 2 + 32 * 2;
    const paddedRelation = `${journal.slice(0, relationPadding)}01${journal.slice(relationPadding + 2)}`;
    assert.throws(() => model.decodeJournal(paddedRelation), /canonical delivery journal ABI/);
  }
});

test('observer replays decoded historical calls with unchanged calldata and block tag', async () => {
  const bindingProof = readJson('../../crates/delivery/proof/fixtures/relation-1.json');
  const sourceProof = readJson('../../crates/delivery/proof/fixtures/relation-2.json');
  const binding = model.decodeJournal(`0x${bindingProof.journal}`);
  const source = model.decodeJournal(`0x${sourceProof.journal}`);
  const spec = {
    index: source.windowIndex, offset: source.windowOffset, plaintextBytes: source.windowPlaintextBytes,
    ciphertextBytes: source.windowCiphertextBytes, ciphertextHash: source.ciphertextHash,
    capsuleHash: source.capsuleHash, plaintextCommitment: source.plaintextCommitment,
    deliveryFee: source.windowDeliveryFee, reservationFee: source.windowReservationFee,
  };
  const abi = new Interface(escrow.abi);
  const registerData = abi.encodeFunctionData('registerKey', [binding, `0x${bindingProof.seal}`]);
  const reserveData = abi.encodeFunctionData('reserveBatch', [ZeroHash, [{
    input: source, specs: [spec], seals: [`0x${sourceProof.seal}`], walletSignature: '0x1234',
  }]]);
  const registered = abi.parseTransaction({ data: registerData });
  const reserved = abi.parseTransaction({ data: reserveData });
  const calls = [];
  const runner = {
    async call(request) {
      calls.push(request);
      const { name } = abi.parseTransaction(request);
      return abi.encodeFunctionResult(name, name === 'previewBinding' ? [binding] : [[source]]);
    },
  };
  const at = { blockTag: 24 };
  const previews = recordedPreviews(new Contract(source.escrow, escrow.abi, runner), model, at);
  assert.equal(model.encodeJournal(await previews.binding(registered.args[0])), `0x${bindingProof.journal}`);
  const statements = await previews.reservation(reserved.args[1][0]);
  assert.equal(statements.length, 1);
  assert.equal(model.encodeJournal(statements[0]), `0x${sourceProof.journal}`);
  assert.deepEqual(calls, [
    { to: source.escrow, ...at, data: abi.encodeFunctionData('previewBinding', [binding]) },
    { to: source.escrow, ...at, data: abi.encodeFunctionData('preview', [source, [spec]]) },
  ]);
  assert.equal(abi.encodeFunctionData('registerKey', registered.args), registerData);
  assert.equal(abi.encodeFunctionData('reserveBatch', reserved.args), reserveData);
});

test('incompatible tuple artifacts fail during model construction', () => {
  const find = (abi, name) => abi.find(entry => name === 'constructor' ? entry.type === name : entry.name === name);
  for (const [built, name, mutate, layout] of [
    [escrow, 'preview', entry => { entry.inputs[0].components[1].name = 'kind'; }, 'statement'],
    [escrow, 'preview', entry => { entry.outputs[0].components.reverse(); }, 'statement'],
    [escrow, 'previewBinding', entry => { entry.outputs[0].components[42].type = 'uint32'; }, 'statement'],
    [escrow, 'publicValues', entry => { entry.outputs[0].components.pop(); }, 'statement'],
    [escrow, 'constructor', entry => { entry.inputs[0].components[4].type = 'uint32'; }, 'profile'],
    [escrow, 'getProfile', entry => { entry.outputs[0].components[4].name = 'windows'; }, 'profile'],
    [accounting, 'previewRelation', entry => { entry.outputs[0].components[0].type = 'bytes'; }, 'statement'],
  ]) {
    const changed = structuredClone(built);
    mutate(find(changed.abi, name));
    assert.throws(() => deliveryAbi(changed, { accounting: built === accounting }),
      new RegExp(`incompatible ${layout} layout for delivery ABI ${name}`));
  }
  assert.throws(() => deliveryAbi({ abi: escrow.abi.filter(entry => entry.name !== 'preview') }), /exactly one delivery ABI preview/);
});
