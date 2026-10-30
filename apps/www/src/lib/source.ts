import { loader } from 'fumadocs-core/source'
import { defineDocs } from 'fumadocs-mdx/macro'

export const docs = defineDocs({
  dir: '../../docs',
  docs: {
    async: true,
    files: [
      'install.md',
      'node.md',
      'deployment.md',
      'troubleshooting.md',
      'releasing.md',
      'ci.md',
    ],
    postprocess: { includeProcessedMarkdown: true },
  },
})

export const source = loader({
  source: docs.toFumadocsSource(),
  baseUrl: '/docs',
})
