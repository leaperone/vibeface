import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
import { resolve } from 'node:path'
import { Document } from 'flexsearch'

const root = resolve('dist/client')
const required = [
  'index.html',
  '404.html',
  'download/index.html',
  'community/index.html',
  'security/index.html',
  'docs/install/index.html',
  'docs/node/index.html',
  'docs/deployment/index.html',
  'docs/troubleshooting/index.html',
  'docs/releasing/index.html',
  'docs/ci/index.html',
  'search-index.json',
  'robots.txt',
  'sitemap.xml',
]

if (!existsSync(root)) fail(`missing ${root}; run the portal build first`)

for (const relative of required) {
  const path = resolve(root, relative)
  if (!existsSync(path) || !statSync(path).isFile()) fail(`missing generated file: ${relative}`)
}

const htmlFiles = walk(root).filter((path) => path.endsWith('.html'))
const localTargets = new Set(htmlFiles.map((path) => {
  const relative = path.slice(root.length + 1).replace(/index\.html$/, '')
  return `/${relative}`
}))
localTargets.add('/')

for (const path of htmlFiles) {
  const html = readFileSync(path, 'utf8')
  const h1Count = [...html.matchAll(/<h1(?:\s|>)/g)].length
  if (h1Count !== 1) fail(`${path.slice(root.length + 1)} has ${h1Count} h1 elements`)
  for (const match of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const target = match[1]
    if (!target || target.startsWith('#') || /^[a-z][a-z\d+.-]*:/i.test(target) || target.startsWith('//')) continue
    const clean = target.split('#', 1)[0].split('?', 1)[0]
    if (clean.startsWith('/')) {
      const normalized = clean.endsWith('/') ? clean : `${clean}/`
      if (!localTargets.has(normalized) && !existsSync(resolve(root, clean.slice(1)))) fail(`${path.slice(root.length + 1)} points to missing ${target}`)
    }
  }
}

const search = JSON.parse(readFileSync(resolve(root, 'search-index.json'), 'utf8'))
if (search.type !== 'default' || !search.raw) fail('search-index.json is not a default FlexSearch export')
const index = new Document({ tokenize: 'full', document: { id: 'id', index: ['content'], tag: ['tags'], store: true } })
for (const [key, value] of Object.entries(search.raw)) index.import(key, value)
for (const query of ['恢复包', '安全码']) {
  const result = await index.searchAsync(query, { index: 'content', limit: 10 })
  if (!result.some((group) => group.result.length > 0)) fail(`search index does not match ${query}`)
}

console.log(`checked ${htmlFiles.length} HTML pages, ${required.length} required outputs, and Chinese search terms`)

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(directory, entry.name)
    return entry.isDirectory() ? walk(path) : [path]
  })
}

function fail(message) {
  console.error(`static check failed: ${message}`)
  process.exit(1)
}
