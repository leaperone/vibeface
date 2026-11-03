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
  'docs/index.html',
  'docs/overview/index.html',
  'docs/quick-start/index.html',
  'docs/onboarding/index.html',
  'docs/user-guide/index.html',
  'docs/comparison/index.html',
  'docs/install/index.html',
  'docs/bot-and-execution/index.html',
  'docs/history-sync/index.html',
  'docs/privacy-security/index.html',
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
for (const [query, url] of [
  ['长期 Bot 聊天', '/docs/overview'],
  ['快速上手', '/docs/quick-start'],
  ['首次配置', '/docs/onboarding'],
  ['取消排队', '/docs/user-guide'],
  ['工作方式', '/docs/comparison'],
  ['协调角色', '/docs/bot-and-execution'],
  ['逐字流式进度', '/docs/history-sync'],
  ['被盗管理设备', '/docs/privacy-security'],
]) {
  const result = await index.searchAsync(query, { index: 'content', limit: 30 })
  const matchesPage = result.some((group) => group.result.some((id) => index.get(id)?.url?.split('#', 1)[0] === url))
  if (!matchesPage) fail(`search index does not match ${query} in ${url}`)
}

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

const docsRoot = resolve('../../packages/docs')
const { pages } = JSON.parse(readFileSync(resolve(docsRoot, 'meta.json'), 'utf8'))
for (const slug of pages) {
  const source = readFileSync(resolve(docsRoot, `${slug}.md`), 'utf8')
  const title = source.match(/^title: (.+)$/m)?.[1]
  const description = source.match(/^description: (.+)$/m)?.[1]
  if (!title || !description) fail(`${slug} is missing public metadata`)
  const page = readFileSync(resolve(root, `docs/${slug}/index.html`), 'utf8')
  if (!page.includes(`<title>${title} · Vibeface 文档</title>`)) fail(`${slug} has stale page title metadata`)
  if (!page.includes(`content="${description}"`)) fail(`${slug} has stale description metadata`)
}

const anchors = new Map(htmlFiles.map((path) => [path, new Set([...readFileSync(path, 'utf8').matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]))]))
for (const path of htmlFiles) {
  for (const match of readFileSync(path, 'utf8').matchAll(/href="([^"]+)"/g)) {
    const url = new URL(match[1].replaceAll('&amp;', '&'), `https://www.vibeface.dev/${path.slice(root.length + 1)}`)
    if (url.origin !== 'https://www.vibeface.dev' || !url.hash) continue
    const target = resolve(root, `.${decodeURIComponent(url.pathname)}`)
    const page = target.endsWith('.html') ? target : resolve(target, 'index.html')
    const ids = anchors.get(page)
    if (ids && !ids.has(decodeURIComponent(url.hash.slice(1)))) fail(`${path.slice(root.length + 1)} points to missing anchor ${match[1]}`)
  }
}

console.log(`checked ${htmlFiles.length} HTML pages, ${required.length} required outputs, public metadata, anchors, and Chinese search terms`)
