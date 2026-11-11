import { dirname, isAbsolute, relative, resolve, sep } from 'node:path'
import { PUBLIC_DOCS } from './public-docs.ts'

type LinkOptions = { repoRoot: string; sourceRef: string }
type MarkdownNode = { type?: string; url?: string; depth?: number; children?: MarkdownNode[] }

/** Resolve repository links before the prerender crawler treats them as website routes. */
export function documentationLink(href: string, sourcePath: string, options: LinkOptions): string {
  if (/[\u0000-\u0020\u007f\\]/.test(href)) throw new Error(`Invalid documentation link: ${href}`)
  const scheme = /^([a-z][a-z\d+.-]*):/i.exec(href)?.[1]?.toLowerCase()
  if (scheme) {
    if (!['http', 'https', 'mailto', 'tel', 'vibeface'].includes(scheme)) throw new Error(`Unsafe documentation URL scheme: ${scheme}`)
    return href
  }
  if (!href || href.startsWith('#') || href.startsWith('?') || href.startsWith('//')) return href
  const suffixAt = href.search(/[?#]/)
  const pathname = suffixAt < 0 ? href : href.slice(0, suffixAt)
  const suffix = suffixAt < 0 ? '' : href.slice(suffixAt)
  const decoded = decodeURIComponent(pathname)
  if (/[\u0000-\u001f\u007f\\]/.test(decoded)) throw new Error(`Invalid documentation path: ${href}`)
  if (href.startsWith('/')) {
    if (decoded.split('/').some(part => part === '.' || part === '..')) throw new Error(`Unsafe site path: ${href}`)
    return href
  }
  const root = resolve(options.repoRoot)
  const source = resolve(sourcePath)
  const sourceRelative = relative(root, source)
  if (isAbsolute(sourceRelative) || sourceRelative === '..' || sourceRelative.startsWith(`..${sep}`)) throw new Error('Documentation source is outside the repository')
  const target = resolve(dirname(source), decoded)
  const repoPath = relative(root, target)
  if (isAbsolute(repoPath) || repoPath === '..' || repoPath.startsWith(`..${sep}`)) throw new Error(`Documentation link escapes the repository: ${href}`)
  const doc = PUBLIC_DOCS.find(doc => target === resolve(root, 'packages/docs', doc.file))
  if (doc) return `/docs/${doc.slug}${suffix}`
  if (!/^[a-zA-Z0-9._-]+$/.test(options.sourceRef)) throw new Error('Invalid documentation source ref')
  const encoded = repoPath.split(sep).map(part => encodeURIComponent(part)).join('/')
  return `https://github.com/leaperone/vibeface/blob/${options.sourceRef}/${encoded}${suffix}`
}

export function rewriteDocumentationLinks(options: LinkOptions) {
  return (tree: MarkdownNode, file: { path?: string }) => {
    const visit = (node: MarkdownNode) => {
      if ((node.type === 'link' || node.type === 'definition') && node.url !== undefined) {
        if (!file.path) throw new Error('Documentation links need their source file path')
        node.url = documentationLink(node.url, file.path, options)
      }
      for (const child of node.children ?? []) visit(child)
    }
    visit(tree)
  }
}

export function removeLeadingHeading() {
  return (tree: MarkdownNode) => {
    const first = tree.children?.[0]
    if (first?.type === 'heading' && first.depth === 1) tree.children?.shift()
  }
}
