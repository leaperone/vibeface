import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { compile, artifact, artifacts, requireDependency, root } from './compile.mjs';

const ganache = requireDependency('ganache');
const { BrowserProvider, ContractFactory, sha256, getBytes, hexlify } = requireDependency('ethers');
compile();
const rpc = ganache.provider({ logging: { quiet: true }, chain: { chainId: 31337, hardfork: 'shanghai' } });
const provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 10;
try {
  const signer = await provider.getSigner();
  const source = fs.readFileSync(path.join(root, 'vendor/risc0/contracts/test/TestReceiptV3_0.sol'), 'utf8');
  const field = name => `0x${source.match(new RegExp(`${name}\\s*=\\s*hex"([0-9a-f]+)"`))[1]}`;
  const seal = field('SEAL'), journal = field('JOURNAL'), imageId = field('IMAGE_ID');
  const a = artifact('RiscZeroAdapter');
  const adapter = await new ContractFactory(a.abi, a.bytecode, signer).deploy(imageId);
  await adapter.waitForDeployment();
  const mutate = (value, at = 0) => { const b = getBytes(value); b[at] ^= 1; return hexlify(b); };
  const cases = [];
  async function check(name, expected) { assert.equal(expected, true, name); cases.push({ name, passed: true }); console.log(`PASS ${name}`); }
  await check('official published Groth16 fixture verified on EVM', await adapter.verify(seal, sha256(journal)));
  await check('corrupted Groth16 seal rejected', !await adapter.verify(mutate(seal, 5), sha256(journal)));
  await check('modified journal rejected', !await adapter.verify(seal, sha256(mutate(journal))));
  await check('wrong proof selector rejected', !await adapter.verify(mutate(seal), sha256(journal)));
  const wrongImage = await new ContractFactory(a.abi, a.bytecode, signer).deploy(mutate(imageId));
  await wrongImage.waitForDeployment();
  await check('adapter pins guest image and rejects a valid seal for another image', !await wrongImage.verify(seal, sha256(journal)));
  const receipt = await (await signer.sendTransaction(await adapter.verify.populateTransaction(seal, sha256(journal)))).wait();
  const result = { kind: 'real EVM verification of official precomputed fixture; not a new delivery proof', passed: cases.length,
    version: 'risc0-ethereum/v3.0.1', selector: await adapter.SELECTOR(), imageId,
    controlRoot: await adapter.CONTROL_ROOT(), bn254ControlId: await adapter.BN254_CONTROL_ID(),
    gas: receipt.gasUsed.toString(), cases };
  fs.writeFileSync(path.join(artifacts, 'official-verifier-results.json'), `${JSON.stringify(result, null, 2)}\n`);
  console.log(JSON.stringify(result));
} finally { await provider.destroy(); await rpc.disconnect(); }
