export function invitationLink(content: string, page: string): string {
  const source = new URL(page)
  if (source.protocol === 'wss:' || source.protocol === 'ws:') source.protocol = source.protocol === 'wss:' ? 'https:' : 'http:'
  const url = new URL('/', source)
  if (url.username || url.password || url.protocol !== 'https:' && !(url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) throw new Error('请用 HTTPS 或本机网页生成连接链接。')
  const bytes = new TextEncoder().encode(content)
  const encoded = btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join('')).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
  url.hash = new URLSearchParams({ invite: encoded }).toString()
  return url.href
}
