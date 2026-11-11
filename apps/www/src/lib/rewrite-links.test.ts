import assert from 'node:assert/strict'
import test from 'node:test'
import { documentationLink, rewriteDocumentationLinks } from './rewrite-links.ts'

const options = { repoRoot: '/repo', sourceRef: '0123456789abcdef0123456789abcdef01234567' }
const source = '/repo/packages/docs/quick-start.md'

test('registered documents retain site routes, query strings, anchors and encoded paths', () => {
  assert.equal(documentationLink('node.md', source, options), '/docs/node')
  assert.equal(documentationLink('./%6eode.md?view=full#配置管理-agent', source, options), '/docs/node?view=full#配置管理-agent')
  assert.equal(documentationLink('../docs/install.md#安装桌面应用', source, options), '/docs/install#安装桌面应用')
  assert.equal(documentationLink('design/ci.md#plan', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/packages/docs/design/ci.md#plan')
})

test('every relative source-file type resolves to the exact source revision', () => {
  assert.equal(documentationLink('../../.github/actions/native-desktop/action.yml', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/.github/actions/native-desktop/action.yml')
  assert.equal(documentationLink('../../apps/desktop/src/main.rs?plain=1#L17', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/apps/desktop/src/main.rs?plain=1#L17')
  assert.equal(documentationLink('../../scripts/package-node.sh', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/scripts/package-node.sh')
  assert.equal(documentationLink('../../README.zh-CN.md#从源码构建', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/README.zh-CN.md#从源码构建')
  assert.equal(documentationLink('design/My%20Notes.md#details', source, options), 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/packages/docs/design/My%20Notes.md#details')
})

test('website routes, page anchors and external URLs stay unchanged', () => {
  for (const href of ['/download?channel=nightly#mac', '/docs/node#配置管理-agent', '/icon.svg', '#本页', '?mode=full#本页', 'https://example.com/doc?a=1#part', 'http://localhost:4790', 'mailto:support@example.com', '//example.com/path']) {
    assert.equal(documentationLink(href, source, options), href)
  }
})

test('repository traversal and executable URLs are rejected', () => {
  for (const href of ['../../../private.txt', '%2e%2e/%2e%2e/%2e%2e/private.txt', '../../..%5cprivate.txt', 'javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'data:text/html,attack', 'java\nscript:alert(1)', '/%2e%2e/private']) {
    assert.throws(() => documentationLink(href, source, options))
  }
  assert.throws(() => documentationLink('node.md', '/elsewhere/doc.md', options), /outside the repository/)
})

test('the Markdown plugin rewrites nested direct and reference links', () => {
  const tree = { type: 'root', children: [{ type: 'paragraph', children: [{ type: 'link', url: 'node.md#全网' }] }, { type: 'definition', url: '../../scripts/package-node.sh' }] }
  rewriteDocumentationLinks(options)(tree, { path: source })
  assert.deepEqual(tree, { type: 'root', children: [{ type: 'paragraph', children: [{ type: 'link', url: '/docs/node#全网' }] }, { type: 'definition', url: 'https://github.com/leaperone/vibeface/blob/0123456789abcdef0123456789abcdef01234567/scripts/package-node.sh' }] })
})
