import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';

export const root = path.dirname(fileURLToPath(import.meta.url));
export const artifacts = path.resolve(root, '../../.artifacts/vdp/contracts');
export const requireDependency = createRequire(path.join(artifacts, 'package.json'));
export const abiFile = process.env.DELIVERY_ABI_FILE || path.resolve(root, '../../crates/delivery/abi/Statement.sol');
export const schema = fs.readFileSync(abiFile, 'utf8');
const solc = requireDependency('solc');

export function compile({ writeTrust = false, checkTrust = false } = {}) {
  if (writeTrust && checkTrust) throw Error('--write-trust and --check-trust are mutually exclusive');
  const sources = { 'crates/delivery/abi/Statement.sol': { content: schema } };
  for (const name of ['DeliveryEscrow.sol', 'TestAdapters.sol', 'RiscZeroAdapter.sol']) {
    sources[`contracts/delivery/${name}`] = { content: fs.readFileSync(path.join(root, name), 'utf8') };
  }
  const pins = JSON.parse(fs.readFileSync(path.join(root, 'verifier-sources.json')));
  for (const pin of pins) {
    const content = fs.readFileSync(path.join(root, pin.path));
    if (createHash('sha256').update(content).digest('hex') !== pin.sha256) throw Error(`Vendor checksum mismatch: ${pin.path}`);
    sources[pin.path.slice('vendor/'.length)] = { content: content.toString('utf8') };
  }
  const output = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources, settings: {
    optimizer: { enabled: true, runs: 200 }, viaIR: true, evmVersion: 'shanghai',
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object', 'evm.deployedBytecode.object', 'evm.deployedBytecode.immutableReferences'] } },
  } })));
  const errors = (output.errors || []).filter(x => x.severity === 'error');
  if (errors.length) throw Error(errors.map(x => x.formattedMessage).join('\n'));
  for (const warning of (output.errors || []).filter(x => x.severity !== 'error')) console.warn(warning.formattedMessage);
  fs.mkdirSync(path.join(artifacts, 'build'), { recursive: true });
  const trust = { schemaVersion: 1, compiler: '0.8.30', compilerBuild: solc.version(),
    settings: { optimizer: { enabled: true, runs: 200 }, viaIR: true, evmVersion: 'shanghai' },
    statementSchemaHash: `0x${createHash('sha256').update(schema).digest('hex')}`, contracts: {} };
  for (const contracts of Object.values(output.contracts)) for (const [name, contract] of Object.entries(contracts)) {
    if (!contract.evm.bytecode.object) continue;
    const bytes = contract.evm.deployedBytecode.object.length / 2;
    if (bytes > 24576) throw Error(`${name} exceeds EIP-170: ${bytes} bytes`);
    fs.writeFileSync(path.join(artifacts, 'build', `${name}.json`), JSON.stringify({
      abi: contract.abi, bytecode: `0x${contract.evm.bytecode.object}`, deployedBytes: bytes,
    }, null, 2));
    if (['DeliveryEscrow', 'RiscZeroAdapter', 'RiscZeroGroth16Verifier', 'TestToken'].includes(name)) {
      trust.contracts[name] = { runtime: `0x${contract.evm.deployedBytecode.object}`,
        immutable_ranges: Object.values(contract.evm.deployedBytecode.immutableReferences || {}).flat().sort((a, b) => a.start - b.start),
        immutable_groups: Object.values(contract.evm.deployedBytecode.immutableReferences || {}).map(group => group.toSorted((a, b) => a.start - b.start)) };
    }
  }
  const trustJson = `${JSON.stringify(trust, null, 2)}\n`;
  if (writeTrust) fs.writeFileSync(path.join(root, 'trust.json'), trustJson);
  if (checkTrust && fs.readFileSync(path.join(root, 'trust.json'), 'utf8') !== trustJson) {
    throw Error('Tracked trust.json differs from current compiler output. Review changes and explicitly regenerate --write-trust.');
  }
  return output;
}

export function artifact(name) { return JSON.parse(fs.readFileSync(path.join(artifacts, 'build', `${name}.json`))); }
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  compile({ writeTrust: process.argv.includes('--write-trust'), checkTrust: process.argv.includes('--check-trust') });
  console.log('Compiled delivery settlement and checksum-pinned official Groth16 verifier.');
}
