import { execFileSync } from 'node:child_process'
import { resolve } from 'node:path'
import { defineConfig } from 'fumadocs-mdx/config'
import { removeLeadingHeading, rewriteDocumentationLinks } from './src/lib/rewrite-links'

const repoRoot = resolve(process.cwd(), '../..')
const sourceRef = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: repoRoot, encoding: 'utf8' }).trim()

export default defineConfig({
  mdxOptions: {
    remarkPlugins: [[rewriteDocumentationLinks, { repoRoot, sourceRef }], removeLeadingHeading],
  },
})
