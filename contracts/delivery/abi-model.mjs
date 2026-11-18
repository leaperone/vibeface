import assert from 'node:assert/strict';
import { requireDependency } from './compile.mjs';

const { AbiCoder, ParamType, getBytes, hexlify, sha256, toUtf8Bytes } = requireDependency('ethers');
const coder = AbiCoder.defaultAbiCoder();
const profileDomain = sha256(toUtf8Bytes('vibeface-delivery-profile-v1'));
const profileFields = [
  ['guest_image_id', 'guestImageId', 'bytes32'],
  ['statement_schema_hash', 'statementSchemaHash', 'bytes32'],
  ['crypto_suite', 'cryptoSuite', 'uint8'],
  ['max_windows', 'maxWindows', 'uint16'],
  ['max_window_bytes', 'maxWindowBytes', 'uint32'],
  ['max_total_bytes', 'maxTotalBytes', 'uint64'],
  ['max_guest_cycles', 'maxGuestCycles', 'uint64'],
  ['max_prover_memory_bytes', 'maxProverMemoryBytes', 'uint64'],
];
const profileLayout = ['bytes32 domain', ...profileFields.map(([, name, type]) => `${type} ${name}`)].join(',');
const profileKeys = new Set(profileFields.map(([key]) => key));
// SHA-256 of the ordered Solidity types and names ("bytes32 domain,uint8 relation,...") in the v1 journal.
const statementLayoutHash = '0xc4ce95b77714ef7d2507545294056669f1c06c1ee6eb5abca2fd2f7ecc279dc0';

function parseProfile(input) {
  assert(input && typeof input === 'object' && !Array.isArray(input), 'expected a proof profile object');
  for (const key of Object.keys(input)) assert(profileKeys.has(key), `unexpected proof profile field ${key}`);
  const value = { domain: profileDomain };
  for (const [key, name, type] of profileFields) {
    assert(Object.hasOwn(input, key), `missing proof profile field ${key}`);
    const field = input[key];
    if (type === 'bytes32') {
      assert(Array.isArray(field) && field.length === 32 && Array.from(field).every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255),
        `proof profile ${key} must contain exactly 32 bytes`);
      value[name] = hexlify(Uint8Array.from(field));
    } else {
      const bits = BigInt(type.slice(4));
      assert(Number.isSafeInteger(field) && field > 0 && BigInt(field) < (1n << bits),
        `proof profile ${key} must be a positive safe JSON integer in ${type} range`);
      value[name] = field;
    }
  }
  return value;
}

export function deliveryAbi(artifact, { accounting = false } = {}) {
  assert(Array.isArray(artifact?.abi), 'delivery artifact ABI is missing');
  function fragment(name) {
    const matches = artifact.abi.filter(entry => name === 'constructor'
      ? entry.type === 'constructor'
      : entry.type === 'function' && entry.name === name);
    assert.equal(matches.length, 1, `expected exactly one delivery ABI ${name}`);
    return matches[0];
  }
  function tuple(name, part, index, { array = false, profile = false } = {}) {
    const label = `delivery ABI ${name} ${part}[${index}]`;
    const parameter = fragment(name)[part]?.[index];
    let type;
    try {
      type = ParamType.from(parameter);
    } catch (cause) {
      throw new Error(`invalid ${label}`, { cause });
    }
    if (array) {
      assert(type.isArray() && type.arrayLength === -1, `expected a dynamic tuple array for ${label}`);
      type = type.arrayChildren;
    }
    assert(type.isTuple(), `expected a tuple for ${label}`);
    const layout = type.components.map(field => `${field.type} ${field.name}`).join(',');
    if (profile) assert.equal(layout, profileLayout, `incompatible profile layout for ${label}`);
    else assert.equal(sha256(toUtf8Bytes(layout)), statementLayoutHash, `incompatible statement layout for ${label}`);
    return type;
  }

  const statementType = tuple('preview', 'inputs', 0);
  tuple('preview', 'outputs', 0, { array: true });
  tuple('previewBinding', 'inputs', 0);
  tuple('previewBinding', 'outputs', 0);
  tuple('publicValues', 'outputs', 0);
  if (accounting) {
    tuple('previewRelation', 'inputs', 0);
    tuple('previewRelation', 'outputs', 0);
  }
  const configuredProfileType = tuple('constructor', 'inputs', 0, { profile: true });
  const observedProfileType = tuple('getProfile', 'outputs', 0, { profile: true });
  const journalFields = statementType.components;
  const journalBytes = journalFields.length * 32;
  const statementObject = value => Object.fromEntries(journalFields.map(({ name }) => [name, value[name]]));
  const encodeJournal = value => coder.encode([statementType], [value]);

  return {
    statementType,
    journalBytes,
    statementObject,
    encodeJournal,
    decodeJournal(value) {
      const bytes = getBytes(value);
      assert.equal(bytes.length, journalBytes, 'exact delivery journal byte length required');
      const statement = coder.decode([statementType], bytes)[0];
      assert.equal(encodeJournal(statement), hexlify(bytes), 'canonical delivery journal ABI required');
      return statementObject(statement);
    },
    configuredProfile(input) {
      const value = parseProfile(input);
      const bytes = coder.encode([configuredProfileType], [value]);
      return { value, bytes, id: sha256(bytes) };
    },
    encodeObservedProfile(value) {
      return coder.encode([observedProfileType], [value]);
    },
  };
}
