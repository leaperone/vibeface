import type { ReadyConnection } from '../lib/node.ts'
import { parseMessageSearch, parseWorkspaceHistory } from './model.ts'
import type { MessageSearchPage, MessageSearchScope, WorkspaceFilter, WorkspaceHistory } from './types.ts'

type ReadConnection = Pick<ReadyConnection, 'rpc'>

/** Read the newest page or continue toward older events using this exact filter. */
export async function readWorkspaceHistoryPage({ connection, filter = {}, cursor }: {
  connection: ReadConnection; filter?: WorkspaceFilter; cursor?: string
}): Promise<WorkspaceHistory> {
  const params = { ...filter, ...(cursor === undefined ? {} : { cursor }) }
  const page = parseWorkspaceHistory(await connection.rpc('workspace_history', params))
  const scope = filter.task_id ? 'task' : filter.channel_id ? 'channel' : 'workspace'
  if (page.coverage.scope !== scope || filter.limit !== undefined && page.coverage.limit !== filter.limit) throw new Error('历史结果与请求范围不一致。')
  return page
}

/** Searches all committed visible messages held by this Node in the exact scope. */
export async function searchMessages({ connection, scope, query, cursor, limit }: {
  connection: ReadConnection; scope: MessageSearchScope; query: string; cursor?: string; limit?: number
}): Promise<MessageSearchPage> {
  const params = { scope, query, ...(cursor === undefined ? {} : { cursor }), ...(limit === undefined ? {} : { limit }) }
  const page = parseMessageSearch(await connection.rpc('message_search', params), { scope, query })
  if (limit !== undefined && page.coverage.limit !== limit) throw new Error('搜索结果与请求条数不一致。')
  return page
}
