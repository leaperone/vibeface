import assert from 'node:assert/strict';
import { createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const args = process.argv.slice(2);
const modulePath = resolve(args.includes('--module') ? args[args.indexOf('--module') + 1] : 'apps/app/generated/vibeface_secure.js');
const crypto = await import(pathToFileURL(modulePath));
await crypto.default({ module_or_path: await readFile(join(dirname(modulePath), 'vibeface_secure_bg.wasm')) });

// Public, deterministic test key. It is not a production identity or credential.
const privateKey = createPrivateKey({
  key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.alloc(32, 42)]),
  format: 'der', type: 'pkcs8',
});
const issuer = createPublicKey(privateKey).export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex');
const now_ms = 1_700_000_000_000;
const payload = JSON.stringify({
  version: 1, issuer, issued_at_ms: now_ms - 1000, expires_at_ms: now_ms + 60_000,
  entries: [{ gateway_url: 'https://gateway.fixture.test', relay_url: 'https://relay.fixture.test' }],
});
const bytes = new TextEncoder().encode(payload);
const signature = sign(null, Buffer.from(crypto.signingMessage('bootstrap-catalog', bytes)), privateKey).toString('hex');
assert.equal(signature.length, 128);
crypto.verify(issuer, 'bootstrap-catalog', bytes, signature);
const output = resolve('tests/fixtures/bootstrap-catalog.json');
await writeFile(output, JSON.stringify({ note: 'Public test vector generated with the real WebAssembly signing-message format.', issuer, now_ms, catalog: { payload, signature } }, null, 2) + '\n');
console.log('Generated and verified the public bootstrap catalogue vector');
