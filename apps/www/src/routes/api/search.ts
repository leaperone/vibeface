import { createFileRoute } from '@tanstack/react-router'
import { flexsearchFromSource } from 'fumadocs-core/search/flexsearch'
import { source } from '@/lib/source'

const search = flexsearchFromSource(source)

export const Route = createFileRoute('/api/search')({
  server: {
    handlers: {
      GET: async () => Response.json(await search.export()),
    },
  },
})
