import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { open, readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { resolve, dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createHash, createPrivateKey, sign } from 'node:crypto';

const args = process.argv.slice(2);
const argument = (name, fallback) => args.includes(name) ? args[args.indexOf(name) + 1] : fallback;
const modulePath = resolve(argument('--module', 'crates/node/web/generated/vibeface_secure.js'));
const crypto = await import(pathToFileURL(modulePath));
await crypto.default({ module_or_path: await readFile(join(dirname(modulePath), 'vibeface_secure_bg.wasm')) });
const hex = value => Buffer.from(value).toString('hex');
const bytes = value => Uint8Array.from(Buffer.from(value, 'hex'));

function endpoint() {
  let identity = crypto.Identity.generate();
  let handshake;
  let channel;
  return async input => {
    try {
      let result = null;
      switch (input.op) {
        case 'fixture_event':
        case 'fixture_chain': {
          const secret = JSON.parse(await readFile(input.identity_file, 'utf8')).secret_key_hex;
          const key = createPrivateKey({key:Buffer.concat([Buffer.from('302e020100300506032b657004220420','hex'),Buffer.from(secret,'hex')]),format:'der',type:'pkcs8'});
          const signed = event => {
            const canonical_body = JSON.stringify(event);
            const hash = createHash('sha256').update(canonical_body).digest('hex');
            return {hash,canonical_body,signature:sign(null,Buffer.from(`vibeface:event:v1:${hash}`),key).toString('hex')};
          };
          if (input.op === 'fixture_event') { result = signed(input.event); break; }
          assert(Number.isInteger(input.count) && input.count >= 2 && input.count <= 100001, 'fixture chain count must be between 2 and 100001');
          const output = await open(input.output_file, 'wx', 0o600);
          const digest = createHash('sha256');
          let first_hash, previous = null, page = '';
          try {
            for (let index = 0; index < input.count; index++) {
              const event = {...input.event,sequence:index+1,previous,parents:previous ? [previous] : [],
                body:index ? {type:'context_checkpoint',summary:'fixture',covered_heads:[]} : {type:'conversation_created',title:'NET-1 continuation fixture'},
                blobs:index === input.count-1 ? [input.blob] : []};
              const envelope = signed(event);
              first_hash ??= envelope.hash;
              previous = envelope.hash;
              const line = `${JSON.stringify(envelope)}\n`;
              digest.update(line); page += line;
              if ((index+1) % 1024 === 0) { await output.writeFile(page); page = ''; }
            }
            if (page) await output.writeFile(page);
            await output.sync();
          } finally { await output.close(); }
          result = {count:input.count,first_hash,head:previous,envelopes_sha256:digest.digest('hex')};
          break;
        }
        case 'identity':
          if (input.secret_hex) { identity.free(); identity = crypto.Identity.fromSecretHex(input.secret_hex); }
          result = { public_key: identity.publicKeyHex(), node_id: identity.nodeId() }; break;
        case 'recovery_seal': result = hex(crypto.recoverySeal(bytes(input.data), input.password)); break;
        case 'recovery_open': result = hex(crypto.recoveryOpen(bytes(input.data), input.password)); break;
        case 'sign': result = identity.sign(input.domain, bytes(input.data)); break;
        case 'handshake':
          handshake = new crypto.Handshake(input.initiator, identity.secretHex(), input.expected_peer_id, new TextEncoder().encode(input.context)); break;
        case 'write': result = hex(handshake.write(bytes(input.data))); break;
        case 'read': result = hex(handshake.read(bytes(input.data))); break;
        case 'remote': result = handshake.remoteId() ?? null; break;
        case 'finish': channel = handshake.finish(); break;
        case 'seal': result = hex(channel.seal(bytes(input.data))); break;
        case 'open': result = hex(channel.open(bytes(input.data))); break;
        case 'seal_message': result = channel.sealMessage(bytes(input.data)).map(hex); break;
        case 'open_record': { const value = channel.openRecord(bytes(input.data)); result = value === null ? null : hex(value); break; }
        default: throw new Error('unknown probe operation');
      }
      return { ok: true, result };
    } catch (error) { return { ok: false, error: String(error) }; }
  };
}

async function selfTest() {
  const nativePath = argument('--native-probe');
  const children = [];
  const timeout = setTimeout(() => { for (const child of children) child.kill(); process.stderr.write('crypto self-test timed out\n'); process.exit(1); }, 60_000);
  function native() {
    if (!nativePath) return endpoint();
    const child = spawn(resolve(nativePath), [], { stdio: ['pipe', 'pipe', 'inherit'] });
    children.push(child);
    const lines = createInterface({ input: child.stdout });
    const waiting = [];
    lines.on('line', line => waiting.shift()?.resolve(JSON.parse(line)));
    child.on('error', error => { for (const request of waiting.splice(0)) request.reject(error); });
    child.on('exit', code => { for (const request of waiting.splice(0)) request.reject(new Error(`native probe exited ${code}`)); });
    return input => new Promise((resolveRequest, reject) => { waiting.push({ resolve: resolveRequest, reject }); child.stdin.write(`${JSON.stringify(input)}\n`); });
  }
  const call = async (peer, input) => { const response = await peer(input); assert(response.ok, response.error); return response.result; };
  async function pair(wrongIdentity = false) {
    const a = endpoint(); const b = native();
    const ai = await call(a, { op: 'identity' }); const bi = await call(b, { op: 'identity' });
    await call(a, { op: 'handshake', initiator: true, expected_peer_id: wrongIdentity ? 'ab'.repeat(32) : bi.node_id, context: 'vibeface:client:v1' });
    await call(b, { op: 'handshake', initiator: false, expected_peer_id: ai.node_id, context: 'vibeface:client:v1' });
    assert.equal(await call(a, { op: 'remote' }), null);
    assert.equal(await call(b, { op: 'read', data: await call(a, { op: 'write', data: '' }) }), '');
    const message2 = await call(b, { op: 'write', data: '' });
    if (wrongIdentity) { assert.equal((await a({ op: 'read', data: message2 })).ok, false); assert.equal((await a({ op: 'finish' })).ok, false); return; }
    assert.equal(await call(a, { op: 'read', data: message2 }), '');
    assert.equal(await call(b, { op: 'read', data: await call(a, { op: 'write', data: hex(Buffer.from('browser private hello')) }) }), hex(Buffer.from('browser private hello')));
    assert.equal(await call(a, { op: 'remote' }), bi.node_id);
    assert.equal(await call(b, { op: 'remote' }), ai.node_id);
    await call(a, { op: 'finish' }); await call(b, { op: 'finish' });
    const signature = await call(b, { op: 'sign', domain: 'client-challenge', data: '010203' });
    crypto.verify(bi.public_key, 'client-challenge', bytes('010203'), signature);
    return { a, b };
  }
  try {
    const { a, b } = await pair();
    const recovery = hex(Buffer.from(JSON.stringify({ private: 'recovery fixture only' })));
    const password = 'correct horse battery staple';
    const packageFromBrowser = await call(a, { op: 'recovery_seal', data: recovery, password });
    assert.equal(await call(b, { op: 'recovery_open', data: packageFromBrowser, password }), recovery);
    const packageFromNative = await call(b, { op: 'recovery_seal', data: recovery, password });
    assert.equal(await call(a, { op: 'recovery_open', data: packageFromNative, password }), recovery);
    assert.equal((await a({ op: 'recovery_open', data: packageFromNative, password: 'incorrect fixture password' })).ok, false);
    for (const payload of [Buffer.alloc(0), Buffer.alloc(150_000, 0x5a)]) {
      const records = await call(a, { op: 'seal_message', data: hex(payload) });
      for (let i = 0; i < records.length; i++) {
        const value = await call(b, { op: 'open_record', data: records[i] });
        assert.equal(value, i === records.length - 1 ? hex(payload) : null);
      }
    }
    const reply = await call(b, { op: 'seal', data: hex(Buffer.from('native to browser')) });
    assert.equal(await call(a, { op: 'open', data: reply }), hex(Buffer.from('native to browser')));
    assert.equal((await a({ op: 'open', data: reply })).ok, false);
    await pair(true);
    const tamper = await pair();
    const cipher = bytes(await call(tamper.a, { op: 'seal', data: '010203' })); cipher[0] ^= 1;
    assert.equal((await tamper.b({ op: 'open', data: hex(cipher) })).ok, false);
    assert.equal((await tamper.b({ op: 'open', data: await call(tamper.a, { op: 'seal', data: '04' }) })).ok, false);
    process.stdout.write(`${JSON.stringify({ ok: true, peer: nativePath ? 'native-rust' : 'wasm', checks: ['identity-binding', 'private-handshake', 'fragmented-message', 'domain-signature', 'wrong-identity', 'tamper', 'replay', 'fail-closed', 'recovery-native-wasm', 'recovery-wrong-password'] })}\n`);
  } finally { clearTimeout(timeout); for (const child of children) child.stdin.end(); }
}

if (args.includes('--self-test')) await selfTest();
else {
  const run = endpoint();
  for await (const line of createInterface({ input: process.stdin })) {
    let result;
    try { result = await run(JSON.parse(line)); }
    catch { result = { ok: false, error: 'invalid probe JSON' }; }
    process.stdout.write(`${JSON.stringify(result)}\n`);
  }
}
