import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { isIP } from 'node:net';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';
import { createHash, randomUUID } from 'node:crypto';

const { values } = parseArgs({ options: {
  listen: { type: 'string', default: '127.0.0.1:0' },
  output: { type: 'string' },
  help: { type: 'boolean', default: false },
}, strict: true, allowPositionals: false });
if (values.help) {
  console.log('Usage: node contracts/delivery/deploy-local.mjs --listen 127.0.0.1:0 --output /absolute/path/deployment.json');
  process.exit(0);
}
assert(values.output && path.isAbsolute(values.output), '--output must be an absolute path for a new public deployment file');
const match = /^(?:\[([a-fA-F0-9:]+)\]|([0-9.]+)):(0|[1-9][0-9]{0,4})$/.exec(values.listen);
assert(match, '--listen must be a literal loopback IP and port');
const host = match[1] || match[2];
const port = Number(match[3]);
const family = isIP(host);
assert(port <= 65535 && (family === 4 && host.startsWith('127.')
  || family === 6 && new URL(`http://[${host}]/`).hostname === '[::1]'), '--listen must be a literal loopback IP and port');

const output = path.resolve(values.output);
const statisticsFile = `${output}.rpc.json`;
assert(!fs.existsSync(output) && !fs.existsSync(statisticsFile), 'deployment output already exists; use a fresh task path');
fs.mkdirSync(path.dirname(output), { recursive: true, mode: 0o700 });
const outputFd = fs.openSync(output, 'wx', 0o600);
let statisticsReserved = false;
let server;
let listening = false;
let provider;
let interval;
let stopping;
let ready = false;
let baseline;
const counts = Object.create(null);
let otherLoggedMethods = 0;
const startedAt = new Date().toISOString();

function atomicPublicJson(file, value) {
  const temporary = `${file}.${randomUUID()}.tmp`;
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
    fs.renameSync(temporary, file);
  } finally {
    if (fs.existsSync(temporary)) fs.unlinkSync(temporary);
  }
}

function statistics(state) {
  return {
    schema_version: 1, state, chain_id: 31337, started_at: startedAt,
    baseline_block: baseline || null,
    rpc_count_source: 'ganache method logger, including fixture and client requests',
    rpc_counts: { ...counts }, other_logged_methods: otherLoggedMethods,
  };
}

async function stop(state = 'stopped') {
  if (stopping) return stopping;
  stopping = (async () => {
    if (interval) clearInterval(interval);
    try {
      if (server && listening) await server.close();
    } catch (error) {
      console.error(error.message);
      process.exitCode = 1;
    } finally {
      if (provider) provider.destroy();
      try { if (statisticsReserved) atomicPublicJson(statisticsFile, statistics(state)); }
      catch (error) { console.error(error.message); process.exitCode = 1; }
      if (!ready) {
        fs.closeSync(outputFd);
        fs.unlinkSync(output);
        if (statisticsReserved) fs.unlinkSync(statisticsFile);
      }
    }
  })();
  return stopping;
}

try {
  fs.closeSync(fs.openSync(statisticsFile, 'wx', 0o600));
  statisticsReserved = true;
  const root = path.dirname(fileURLToPath(import.meta.url));
  const profile = JSON.parse(fs.readFileSync(path.resolve(root, '../../proofs/delivery/profile.json'), 'utf8'));
  assert.deepEqual(Object.keys(profile).sort(), ['crypto_suite', 'guest_image_id', 'max_guest_cycles', 'max_prover_memory_bytes',
    'max_total_bytes', 'max_window_bytes', 'max_windows', 'statement_schema_hash'].sort(), 'invalid product proof profile');
  for (const field of ['guest_image_id', 'statement_schema_hash']) {
    assert(Array.isArray(profile[field]) && profile[field].length === 32 && profile[field].every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255), 'invalid product profile digest');
  }
  assert(profile.guest_image_id.some(byte => byte !== 0), 'zero product guest image');
  assert(profile.crypto_suite === 1 && profile.max_windows === 2 && profile.max_window_bytes === 65536
    && profile.max_total_bytes === 131072 && profile.max_guest_cycles === 64000000
    && profile.max_prover_memory_bytes === 17179869184, 'unsupported local delivery profile');

  const { compile, artifact, requireDependency, schema } = await import('./compile.mjs');
  assert.equal(Buffer.from(profile.statement_schema_hash).toString('hex'), createHash('sha256').update(schema).digest('hex'), 'product profile and statement schema differ');
  compile({ checkTrust: true });
  const ganache = requireDependency('ganache');
  const { AbiCoder, BrowserProvider, Contract, ContractFactory, ParamType, hexlify, keccak256, sha256, toUtf8Bytes } = requireDependency('ethers');
  server = ganache.server({
    chain: { chainId: 31337, networkId: 31337, hardfork: 'shanghai' },
    wallet: { totalAccounts: 4, defaultBalance: 1000 },
    miner: { blockTime: 1, blockGasLimit: 30_000_000 },
    logging: { quiet: false, logger: { log(message) {
      if (typeof message !== 'string' || !/^(?:eth|net|web3|evm|debug|personal|txpool|miner|rpc)_[A-Za-z0-9_]{1,64}$/.test(message)) return;
      if (Object.hasOwn(counts, message) || Object.keys(counts).length < 256) counts[message] = (counts[message] || 0) + 1;
      else otherLoggedMethods++;
    } } },
  });
  await server.listen(port, host);
  listening = true;
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => { void stop(); });
  const actualPort = server.address().port;
  const rpcUrl = `http://${family === 6 ? `[${host}]` : host}:${actualPort}`;
  provider = new BrowserProvider(server.provider, undefined, { cacheTimeout: -1 });
  provider.pollingInterval = 100;
  const signer = await provider.getSigner(0);
  const funder = (await signer.getAddress()).toLowerCase();
  const chainId = Number((await provider.getNetwork()).chainId);
  assert.equal(chainId, 31337);
  const publicProfile = {
    domain: sha256(toUtf8Bytes('vibeface-delivery-profile-v1')),
    guestImageId: hexlify(Uint8Array.from(profile.guest_image_id)),
    statementSchemaHash: hexlify(Uint8Array.from(profile.statement_schema_hash)),
    cryptoSuite: profile.crypto_suite, maxWindows: profile.max_windows,
    maxWindowBytes: profile.max_window_bytes, maxTotalBytes: profile.max_total_bytes,
    maxGuestCycles: profile.max_guest_cycles, maxProverMemoryBytes: profile.max_prover_memory_bytes,
  };
  async function deploy(name, args) {
    const built = artifact(name);
    const contract = await new ContractFactory(built.abi, built.bytecode, signer).deploy(...args, { gasLimit: 29_000_000 });
    await contract.waitForDeployment();
    const receipt = await contract.deploymentTransaction().wait(2);
    assert(receipt && receipt.status === 1, `${name} deployment did not confirm`);
    return { contract, receipt };
  }
  const asset = await deploy('TestToken', [funder]);
  const escrow = await deploy('DeliveryEscrow', [publicProfile]);
  const assetAddress = (await asset.contract.getAddress()).toLowerCase();
  const escrowAddress = (await escrow.contract.getAddress()).toLowerCase();
  const adapterAddress = (await escrow.contract.adapter()).toLowerCase();
  const adapterAbi = artifact('RiscZeroAdapter').abi;
  const adapter = new Contract(adapterAddress, adapterAbi, provider);
  const verifierAddress = (await adapter.verifier()).toLowerCase();
  const tuple = ParamType.from(artifact('DeliveryEscrow').abi.find(item => item.type === 'constructor').inputs[0]);
  const profileId = sha256(AbiCoder.defaultAbiCoder().encode([tuple], [publicProfile]));
  assert.equal(await escrow.contract.profileId(), profileId);
  assert.equal(await adapter.imageId(), publicProfile.guestImageId);
  assert.equal(await asset.contract.decimals(), 6n);
  assert.equal(await asset.contract.mode(), 0n);

  const codeHashes = {};
  for (const [name, address] of Object.entries({ escrow: escrowAddress, asset: assetAddress, adapter: adapterAddress, verifier: verifierAddress })) {
    const code = await provider.getCode(address);
    assert(code !== '0x', `${name} runtime is missing`);
    codeHashes[name] = keccak256(code);
  }
  const block = await provider.getBlock('latest');
  baseline = { number: block.number, hash: block.hash, timestamp: block.timestamp };
  const receiptValue = receipt => ({ hash: receipt.hash, block_number: receipt.blockNumber, block_hash: receipt.blockHash, gas_used: receipt.gasUsed.toString() });
  const deployment = {
    schema_version: 1, rpc_url: rpcUrl, chain_id: 31337, escrow: escrowAddress, asset: assetAddress,
    profile, confirmations: 2, funder, profile_id: profileId, adapter: adapterAddress, verifier: verifierAddress,
    runtime_code_hashes: codeHashes, baseline_block: baseline,
    baseline_balances: {
      native: { [funder]: (await provider.getBalance(funder, block.number)).toString() },
      asset: { [funder]: (await asset.contract.balanceOf(funder, { blockTag: block.number })).toString(),
        [escrowAddress]: (await asset.contract.balanceOf(escrowAddress, { blockTag: block.number })).toString() },
    },
    deployment_transactions: { asset: receiptValue(asset.receipt), escrow: receiptValue(escrow.receipt) },
    rpc_counts: { ...counts }, statistics_file: statisticsFile,
  };
  assert.equal(deployment.baseline_balances.asset[funder], '1000000000000');
  assert.equal(deployment.baseline_balances.asset[escrowAddress], '0');
  assert(!stopping, 'deployment was stopped before readiness');
  fs.writeFileSync(outputFd, `${JSON.stringify(deployment, null, 2)}\n`);
  fs.fsyncSync(outputFd);
  fs.closeSync(outputFd);
  ready = true;
  atomicPublicJson(statisticsFile, statistics('ready'));
  interval = setInterval(() => {
    try { atomicPublicJson(statisticsFile, statistics('ready')); }
    catch (error) { console.error(error.message); process.exitCode = 1; void stop('failed'); }
  }, 1000);
  interval.unref();
  console.log(`READY ${output}`);
} catch (error) {
  if (!stopping) {
    console.error(error.shortMessage || error.message);
    process.exitCode = 1;
  }
  await stop('failed');
}
