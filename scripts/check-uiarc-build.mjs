import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const appRequire = createRequire(resolve(root, 'apps/app/package.json'))
const arcRequire = createRequire(resolve(root, 'packages/uiarc/package.json'))
const clientRequire = createRequire(resolve(root, 'packages/client/package.json'))
const { compile } = appRequire('tailwindcss')
const variables = await readFile(resolve(dirname(appRequire.resolve('tailwindcss/package.json')), 'theme.css'), 'utf8')
const candidates = ['text-secondary', 'text-muted', 'border-strong', 'border-subtle', 'bg-surface', 'rounded-control', 'text-sm', 'text-base']
const colorRules = {
  'text-secondary': ['color', '--text-secondary'],
  'text-muted': ['color', '--text-muted'],
  'border-strong': ['border-color', '--border-strong'],
  'border-subtle': ['border-color', '--border-subtle'],
  'bg-surface': ['background-color', '--surface'],
}

for (const platform of ['web', 'native']) {
  const file = platform === 'web' ? 'theme.css' : 'native.css'
  const source = (await readFile(resolve(root, 'packages/theme', file), 'utf8')).replace(/^@import[^;]+;/gm, '')
  const sheet = await compile(`${variables}\n@custom-variant light (&:where(:not(.dark)));\n@custom-variant dark (&:where(.dark));\n${source}\n@tailwind utilities;`)
  const css = sheet.build(candidates)
  for (const [candidate, [property, token]] of Object.entries(colorRules)) {
    const rule = new RegExp(`\\.${candidate}\\s*\\{[^}]*${property}:\\s*var\\(${token}\\)`)
    assert.match(css, rule, `${platform}: ${candidate} must compile to the Arc role ${token}`)
  }
  assert.match(css, /\.rounded-control\s*\{[^}]*border-radius:\s*(?:1\.125rem|18px)/, `${platform}: Arc control radius must compile`)
  if (platform === 'native') {
    assert.match(css, /\.text-sm\s*\{[^}]*line-height:\s*var\(--tw-leading,\s*19\.6px\)/, 'Native 14px body text must retain Arc 1.4 line height')
    assert.match(css, /\.text-base\s*\{[^}]*line-height:\s*var\(--tw-leading,\s*22\.4px\)/, 'Native 16px body text must retain Arc 1.4 line height')
  }
}

for (const dependency of ['react', 'react-dom']) {
  assert.equal(arcRequire.resolve(dependency), appRequire.resolve(dependency), `Web and Arc must execute the same ${dependency} instance`)
}
assert.equal(clientRequire.resolve('react'), appRequire.resolve('react'), 'Web client providers must execute the same React instance as their controls')
console.log('Arc compiled utilities, native line heights and Web React identity verified')
