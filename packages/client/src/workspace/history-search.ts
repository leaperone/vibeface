import type { ReadyConnection } from '../lib/node.ts'
import { parseMessageSearch } from './model.ts'
import type { MessageSearchPage, MessageSearchScope } from './types.ts'

type ReadConnection = Pick<ReadyConnection, 'rpc'>

/** Searches all committed visible messages held by this Node in the exact scope. */
export async function searchMessages({ connection, scope, query, cursor, limit }: {
  connection: ReadConnection; scope: MessageSearchScope; query: string; cursor?: string; limit?: number
}): Promise<MessageSearchPage> {
  const params = { scope, query, ...(cursor === undefined ? {} : { cursor }), ...(limit === undefined ? {} : { limit }) }
  const page = parseMessageSearch(await connection.rpc('message_search', params), { scope, query })
  if (limit !== undefined && page.coverage.limit !== limit) throw new Error('搜索结果与请求条数不一致。')
  return page
}
