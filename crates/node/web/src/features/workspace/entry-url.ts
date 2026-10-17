export function conversationHref(currentHref: string, conversationId: string) {
  const url = new URL(currentHref)
  url.hash = ""
  url.search = ""
  url.searchParams.set("conversation", conversationId)
  return `${url.pathname}${url.search}`
}
