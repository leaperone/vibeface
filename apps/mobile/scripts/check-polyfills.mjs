import assert from 'node:assert/strict'
import { webcrypto, randomUUID } from 'node:crypto'
import { existsSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import vm from 'node:vm'
import ts from 'typescript'

const app = fileURLToPath(new URL('..', import.meta.url))
const packageFile = resolve(app, 'package.json')
const expoRequire = createRequire(createRequire(packageFile).resolve('expo/package.json'))
const { transformSync } = expoRequire('@babel/core')

function bootstrap(existing) {
  const context = vm.createContext({ TextEncoder, ArrayBuffer, DataView, Uint8Array, console, __DEV__: false })
  context.global = context
  if (existing) context.TextDecoder = existing
  const modules = new Map()
  function load(specifier, parent) {
    if (specifier === 'react-native-url-polyfill/auto') return {}
    if (specifier === 'expo-crypto') return {
      CryptoDigestAlgorithm: { SHA256: 'SHA-256' },
      digest: (algorithm, data) => webcrypto.subtle.digest(algorithm, data),
      getRandomValues: data => webcrypto.getRandomValues(data), randomUUID,
    }
    if (specifier === 'expo-router/entry') {
      assert.equal(typeof load('../../packages/client/src/lib/transport.ts', packageFile).createTransport, 'function')
      assert.equal(typeof load('../../packages/client/src/lib/secure.ts', packageFile).secureKit, 'function')
      return {}
    }
    const typescript = resolve(dirname(parent), `${specifier}.ts`)
    const filename = specifier.startsWith('.') && existsSync(typescript) ? typescript : createRequire(parent).resolve(specifier)
    if (modules.has(filename)) return modules.get(filename).exports
    const source = readFileSync(filename, 'utf8')
    const code = filename.endsWith('.ts')
      ? ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText
      : transformSync(source, { filename, babelrc: false, configFile: false, plugins: [expoRequire.resolve('@babel/plugin-transform-flow-strip-types'), expoRequire.resolve('@babel/plugin-transform-modules-commonjs')] }).code
    const module = { exports: {} }
    modules.set(filename, module)
    vm.runInContext(`(function(require, module, exports) {${code}\n})`, context, { filename })(value => load(value, filename), module, module.exports)
    return module.exports
  }
  load(JSON.parse(readFileSync(packageFile, 'utf8')).main, packageFile)
  return context.TextDecoder
}

const Decoder = bootstrap()
const decoder = new Decoder('utf-8', { fatal: true })
const encoder = new TextEncoder()
assert.equal(decoder.fatal, true)
assert.equal(decoder.decode(encoder.encode('中文 😀')), '中文 😀')
assert.equal(decoder.decode(Uint8Array.of(0xef, 0xbb, 0xbf, 0x61)), 'a')
assert.equal(new Decoder('utf-8', { ignoreBOM: true }).decode(Uint8Array.of(0xef, 0xbb, 0xbf, 0x61)), '\ufeffa')
const padded = Uint8Array.of(0xff, 0x61, 0x62, 0xff)
assert.equal(decoder.decode(padded.subarray(1, 3)), 'ab')
assert.equal(decoder.decode(new DataView(padded.buffer, 1, 2)), 'ab')
assert.equal(decoder.decode(encoder.encode('again').buffer), 'again')
for (const bytes of [[0x80], [0xc0, 0xaf], [0xe2, 0x82], [0xed, 0xa0, 0x80], [0xf4, 0x90, 0x80, 0x80]]) {
  assert.throws(() => decoder.decode(Uint8Array.from(bytes)), { name: 'TypeError' })
  assert.equal(decoder.decode(encoder.encode('recovered')), 'recovered')
}
assert.equal(new Decoder().decode(Uint8Array.of(0x80)), '\ufffd')
const stream = new Decoder('utf-8', { fatal: true })
assert.equal(stream.decode(Uint8Array.of(0xe4, 0xb8), { stream: true }), '')
assert.equal(stream.decode(Uint8Array.of(0xad)), '中')
assert.equal(bootstrap(TextDecoder), TextDecoder)
console.log('Mobile bootstrap and strict UTF-8 decoding passed (missing and existing TextDecoder).')
