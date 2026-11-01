import { loader } from 'fumadocs-core/source'
import { defineDocs } from 'fumadocs-mdx/macro'

export const docs = defineDocs({
  dir: '../../docs',
  docs: {
    async: true,
    files: [
      'overview.md',
      'quick-start.md',
      'onboarding.md',
      'user-guide.md',
      'comparison.md',
      'install.md',
      'bot-and-execution.md',
      'history-sync.md',
      'privacy-security.md',
      'node.md',
      'deployment.md',
      'troubleshooting.md',
      'releasing.md',
      'ci.md',
    ],
    postprocess: { includeProcessedMarkdown: true },
  },
  meta: { files: ['meta.json'] },
})

export const source = loader({
  source: docs.toFumadocsSource(),
  baseUrl: '/docs',
})
