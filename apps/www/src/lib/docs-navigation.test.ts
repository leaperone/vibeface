import assert from 'node:assert/strict'
import test from 'node:test'
import { searchPath } from 'fumadocs-core/breadcrumb'
import { flattenTree } from 'fumadocs-core/page-tree'
import type * as PageTree from 'fumadocs-core/page-tree'
import { groupDocsNavigation } from './docs-navigation.ts'
import { PUBLIC_DOCS, PUBLIC_DOC_GROUPS } from './public-docs.ts'

const expectedSlugs = [
  'overview', 'quick-start', 'onboarding', 'user-guide', 'cli', 'comparison',
  'install', 'ios', 'mobile-push', 'bot-and-execution', 'history-sync', 'privacy-security',
  'node', 'deployment', 'troubleshooting', 'releasing', 'ci',
]

function sourceTree(): PageTree.Root {
  return {
    name: 'Vibeface 文档',
    $id: 'public-docs',
    children: PUBLIC_DOCS.map(({ slug, file }) => ({
      type: 'page', name: `Title: ${slug}`, url: `/docs/${slug}`, $ref: file,
    })),
  }
}

test('four native collapsible groups retain all 17 exact page URLs once and preserve source nodes', () => {
  const source = sourceTree()
  const originalPages = [...source.children]
  Object.freeze(source.children)
  source.children.forEach(Object.freeze)
  const tree = groupDocsNavigation(source)
  assert.equal(tree.$id, source.$id)
  assert.deepEqual(tree.children.map(node => node.name), ['开始使用', 'Bot 与任务', '设备与安全', '部署与开发'])
  const pages = flattenTree(tree.children)
  assert.equal(pages.length, 17)
  assert.deepEqual(pages.map(page => page.url).sort(), expectedSlugs.map(slug => `/docs/${slug}`).sort())
  assert.equal(new Set(pages.map(page => page.url)).size, 17)
  assert.deepEqual(source.children, originalPages)
  for (const page of pages) assert.equal(page, originalPages.find(node => node.type === 'page' && node.url === page.url))
  for (const node of tree.children) {
    assert.equal(node.type, 'folder')
    if (node.type !== 'folder') throw new Error('Expected a native folder')
    assert.equal(node.collapsible, true)
    assert.equal(node.defaultOpen, false)
    assert.equal(node.index, undefined)
  }
})

test('Fumadocs detects every current page and its group, allowing automatic active-group expansion', () => {
  const tree = groupDocsNavigation(sourceTree())
  for (const { title, pages } of PUBLIC_DOC_GROUPS) {
    for (const slug of pages) {
      const path = searchPath(tree.children, `/docs/${slug}`)
      assert.ok(path, `Current page ${slug} must be found`)
      assert.equal(path.length, 2)
      assert.equal(path[0].type, 'folder')
      assert.equal(path[0].name, title)
      const page = path[1]
      assert.equal(page.type, 'page')
      if (page.type === 'page') assert.equal(page.url, `/docs/${slug}`)
    }
  }
})

test('navigation refuses missing, duplicated or unregistered source pages instead of hiding them', () => {
  const missing = sourceTree()
  missing.children.shift()
  assert.throws(() => groupDocsNavigation(missing), /Missing documentation page/)
  const duplicated = sourceTree()
  duplicated.children.push(duplicated.children[0])
  assert.throws(() => groupDocsNavigation(duplicated), /Duplicate documentation page/)
  const extra = sourceTree()
  extra.children.push({ type: 'page', name: 'Private', url: '/docs/private' })
  assert.throws(() => groupDocsNavigation(extra), /Unregistered documentation pages/)
})

test('registry validation rejects the old shape, invalid groups, invalid slugs and duplicate pages', async () => {
  const { readPublicDocGroups, readPublicDocs } = await import(new URL('../../scripts/generate-public-docs.mjs', import.meta.url).href)
  assert.deepEqual(readPublicDocs(), PUBLIC_DOCS)
  assert.deepEqual(readPublicDocGroups().map(({ title, pages }: { title: string; pages: { slug: string }[] }) => ({ title, pages: pages.map(page => page.slug) })), PUBLIC_DOC_GROUPS)
  for (const registry of [null, [], ['overview'], {}, { groups: [] }, { groups: [null] }, { groups: [{ title: '', pages: ['overview'] }] }, { groups: [{ title: 'Docs', pages: [] }] }]) {
    assert.throws(() => readPublicDocGroups(registry))
  }
  assert.throws(() => readPublicDocGroups({ groups: [{ title: 'Docs', pages: ['../private'] }] }), /Invalid public document slug/)
  assert.throws(() => readPublicDocGroups({ groups: [{ title: 'Docs', pages: ['overview', 'overview'] }] }), /Duplicate public document slug/)
  assert.throws(() => readPublicDocGroups({ groups: [{ title: 'One', pages: ['overview'] }, { title: 'Two', pages: ['overview'] }] }), /Duplicate public document slug/)
  assert.throws(() => readPublicDocGroups({ groups: [{ title: 'Docs', pages: ['overview'] }, { title: 'Docs', pages: ['quick-start'] }] }), /Duplicate public document group/)
  assert.throws(() => readPublicDocGroups({ groups: [{ title: 'Docs', pages: ['missing-document'] }] }))
})
