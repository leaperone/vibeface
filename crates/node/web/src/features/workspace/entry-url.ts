import type { IdentityScope } from '../../lib/scope'

export function conversationHref(currentHref: string, conversationId: string, scope: IdentityScope, resourceId: string) {
  const url = new URL(currentHref)
  url.hash = ""
  url.search = ""
  url.searchParams.set("conversation", conversationId)
  url.searchParams.set("account", scope.account_id)
  url.searchParams.set("space", scope.space_id)
  url.searchParams.set("device", scope.device_id)
  url.searchParams.set("project", resourceId)
  return `${url.pathname}${url.search}`
}
