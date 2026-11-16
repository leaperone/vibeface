import assert from 'node:assert/strict'
import { Buffer } from 'node:buffer'
import { registerHooks } from 'node:module'
import test from 'node:test'
import { base64ToBytes, bytesToBase64, bytesToHex, sha256Hex } from './byte-codec.ts'
import { recoveryCode, requestKey } from './secure.ts'

test('base64 matches literal empty, text, Unicode bytes and binary vectors', () => {
  const vectors: [number[], string][] = [
    [[], ''], [[102], 'Zg=='], [[102, 111], 'Zm8='], [[102, 111, 111], 'Zm9v'],
    [[102, 111, 111, 98], 'Zm9vYg=='], [[102, 111, 111, 98, 97], 'Zm9vYmE='],
    [[102, 111, 111, 98, 97, 114], 'Zm9vYmFy'],
    [[0, 1, 127, 128, 254, 255], 'AAF/gP7/'],
    [[228, 184, 173, 240, 159, 153, 130], '5Lit8J+Zgg=='],
  ]
  for (const [values, encoded] of vectors) {
    const bytes = new Uint8Array(values)
    assert.equal(bytesToBase64(bytes), encoded)
    assert.deepEqual(base64ToBytes(encoded), bytes)
    assert.deepEqual(bytes, new Uint8Array(values), 'encoding does not clear the caller buffer')
  }
  const containing = new Uint8Array([99, 0, 255, 99])
  assert.equal(bytesToBase64(containing.subarray(1, 3)), 'AP8=')
  assert.deepEqual(containing, new Uint8Array([99, 0, 255, 99]))
})

test('base64 retains runtime permissiveness and propagates malformed input errors', () => {
  for (const encoded of ['Zg', ' Zg==\n', 'Zh==']) assert.deepEqual(base64ToBytes(encoded), new Uint8Array([102]))
  for (const encoded of ['A', 'Zg===', '*', '_w==', '中']) assert.throws(() => base64ToBytes(encoded), { name: 'InvalidCharacterError' })
})

test('base64 block edges and multi-megabyte attachments agree with an independent encoder', () => {
  for (const size of [24_575, 24_576, 24_577, 32_767, 32_768, 32_769, 1_048_575, 1_048_576, 1_048_577, 4_194_305]) {
    const bytes = Uint8Array.from({ length: size }, (_, index) => index % 256)
    const expected = Buffer.from(bytes).toString('base64')
    assert.equal(bytesToBase64(bytes), expected, `encoded ${size} bytes`)
    assert.deepEqual(base64ToBytes(expected), bytes, `decoded ${size} bytes`)
  }
})

test('hex and SHA-256 preserve known byte-exact vectors and subarray bounds', async () => {
  assert.equal(bytesToHex(new Uint8Array()), '')
  assert.equal(bytesToHex(new Uint8Array([0, 1, 15, 16, 128, 254, 255])), '00010f1080feff')
  assert.equal(await sha256Hex(new Uint8Array()), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855')
  const bytes = new Uint8Array([255, 97, 98, 99, 255])
  assert.equal(await sha256Hex(bytes.subarray(1, 4)), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')
  assert.deepEqual(bytes, new Uint8Array([255, 97, 98, 99, 255]), 'hashing does not clear the caller buffer')
})

test('request keys concatenate binary payload and UTF-8 signer bytes; recovery codes hash key bytes', async () => {
  assert.equal(await requestKey(new Uint8Array([0, 255, 128]), '中🙂'), '011fb90fb81f30edf1022734b38727e3ed129e6337cd05b4865339054a93c20a')
  assert.equal(await recoveryCode('00'.repeat(32)), '66687AAD')
  assert.equal(await recoveryCode('ff'.repeat(32)), 'AF961376')
  assert.equal(await recoveryCode('000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f'), '630DCD29')
})

test('the byte codec imports and runs without React or a platform adapter', async () => {
  const hooks = registerHooks({
    resolve(specifier, context, nextResolve) {
      assert.ok(!/^react(?:\/|$)/.test(specifier), `unexpected React dependency: ${specifier}`)
      return nextResolve(specifier, context)
    },
  })
  try {
    const path = './byte-codec.ts?without-react'
    const codec: typeof import('./byte-codec.ts') = await import(path)
    assert.equal(codec.bytesToBase64(new Uint8Array([0, 255])), 'AP8=')
    assert.equal(await codec.sha256Hex(new Uint8Array()), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855')
  } finally { hooks.deregister() }
})
