const ENTRY_ERROR = "填写 HTTP 或 HTTPS 的入口地址，不包含账号、路径或密钥。"

export function validEntry(value: string) {
  const url = new URL(value)
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    (url.pathname !== "/" && url.pathname !== "")
  ) {
    throw new Error(ENTRY_ERROR)
  }
  return url.origin
}

export function entryHref(origin: string, conversationId: string, heads: readonly string[]) {
  const url = new URL(validEntry(origin))
  if (conversationId) {
    url.searchParams.set("conversation", conversationId)
    const basis = JSON.stringify(heads)
    if (basis.length < 6000) url.searchParams.set("basis", basis)
  }
  return url.href
}

export function conversationHref(currentHref: string, conversationId: string) {
  const url = new URL(currentHref)
  url.username = ""
  url.password = ""
  url.hash = ""
  for (const key of ["password", "csrf", "cookie"]) url.searchParams.delete(key)
  url.searchParams.set("conversation", conversationId)
  url.searchParams.delete("basis")
  return `${url.origin}${url.pathname}${url.search}`
}
