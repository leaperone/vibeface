import { dirname, relative, resolve } from 'node:path'
import { PUBLIC_DOCS } from './public-docs'

const publicDocs = new Map<string, string>(PUBLIC_DOCS.map((doc) => [doc.file, doc.slug]))

type LinkNode = { type: string; url?: string }
type RootNode = { type?: string; children?: Array<LinkNode | RootNode> }

function walk(node: RootNode | LinkNode, visit: (node: LinkNode) => void) {
  if (node.type === 'link') visit(node as LinkNode)
  for (const child of 'children' in node && node.children ? node.children : []) walk(child, visit)
}

export function rewriteDocumentationLinks() {
  return (tree: RootNode, file: { path?: string }) => {
    walk(tree, (node) => {
      const href = node.url
      if (!href || href.startsWith('#') || /^[a-z][a-z\d+.-]*:/i.test(href) || href.startsWith('/')) return
      const [target, hash = ''] = href.split('#', 2)
      if (!target.endsWith('.md') && !target.endsWith('.mdx')) return

      const sourcePath = file.path ?? ''
      const resolvedPath = resolve(dirname(sourcePath), target)
      const basename = resolvedPath.slice(resolvedPath.lastIndexOf('/') + 1)
      const slug = publicDocs.get(basename)
      if (slug && resolvedPath.includes('/docs/')) {
        node.url = `/docs/${slug}${hash ? `#${hash}` : ''}`
        return
      }

      const repoRoot = resolve(process.cwd(), '../..')
      const repoPath = relative(repoRoot, resolvedPath)
      node.url = `https://github.com/leaperone/vibeface/blob/main/${repoPath}${hash ? `#${hash}` : ''}`
    })
  }
}

export function removeLeadingHeading() {
  return (tree: RootNode) => {
    const first = tree.children?.[0]
    if (first && 'type' in first && first.type === 'heading' && 'depth' in first && first.depth === 1) {
      tree.children?.shift()
    }
  }
}
