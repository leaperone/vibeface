import { defineConfig } from 'fumadocs-mdx/config'
import { removeLeadingHeading, rewriteDocumentationLinks } from './src/lib/rewrite-links'

export default defineConfig({
  mdxOptions: {
    remarkPlugins: [rewriteDocumentationLinks, removeLeadingHeading],
  },
})
