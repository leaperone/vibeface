import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { resolve } from 'node:path';

const port = Number(process.argv[2]);
assert(Number.isInteger(port) && port > 0 && port < 65536, 'Expected a TCP port');
const binary = process.env.VIBEFACE_E2E_BINARY ?? 'vibeface';
const root = resolve('.artifacts/e2e');
await mkdir(root, { recursive: true });
const dataDir = await mkdtemp(`${root}/node-`);
const reservation = createServer();
await new Promise((done, fail) => {
  reservation.once('error', fail);
  reservation.listen(0, '127.0.0.1', done);
});
const address = reservation.address();
assert(address && typeof address === 'object');
const localPort = address.port;
await new Promise((done, fail) => reservation.close(error => error ? fail(error) : done()));

const init = spawnSync(binary, ['init', '--data-dir', dataDir,
  '--listen', `127.0.0.1:${port}`, '--local-listen', `127.0.0.1:${localPort}`], { encoding: 'utf8' });
if (init.error) throw init.error;
assert.equal(init.status, 0, init.stderr);
const configPath = `${dataDir}/config.json`;
const config = JSON.parse(await readFile(configPath, 'utf8'));
config.manager = null;
config.providers.execution = false;
config.triage.scan_codex_sessions = false;
await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`);

const node = spawn(binary, ['serve', '--data-dir', dataDir], { stdio: 'inherit' });
process.once('SIGTERM', () => node.kill('SIGTERM'));
process.once('SIGINT', () => node.kill('SIGINT'));
node.once('error', error => { console.error(error.message); process.exitCode = 1; });
node.once('exit', code => { process.exitCode = code ?? 1; });
