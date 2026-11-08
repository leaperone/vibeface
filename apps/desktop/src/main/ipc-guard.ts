import { CLIENT_HOST, CLIENT_SCHEME } from '../shared/client-contract.js'

/**
 * Privileged node IPC is reachable only from the top frame of the trusted first-party client
 * window at the fixed CLIENT origin. WebContents identity is checked together with frame URL,
 * because origin checks alone would accept subframes or another window.
 */
export type SenderIdentity = {
  sender: object | null
  senderFrame: object | null
  frameUrl: string | null
  frameParent: object | null
}

export type ClientIdentity = {
  contents: object | null
  mainFrame: object | null
}

function sameClientOrigin(raw: string | null): boolean {
  if (!raw) return false
  try {
    const url = new URL(raw)
    return url.protocol === `${CLIENT_SCHEME}:` && url.host === CLIENT_HOST && !url.username && !url.password && !url.port
  } catch {
    return false
  }
}

export function isTrustedClientCall(sender: SenderIdentity, client: ClientIdentity): boolean {
  if (!client.contents || !client.mainFrame) return false
  if (sender.sender !== client.contents) return false
  if (sender.senderFrame !== client.mainFrame) return false
  if (sender.frameParent !== null) return false
  return sameClientOrigin(sender.frameUrl)
}

/** Reject anything that is not a plain string of a sane length before it reaches Core. */
export function readString(value: unknown, what: string, maxLength: number): string {
  if (typeof value !== 'string') throw new TypeError(`${what} must be text`)
  const trimmed = value.trim()
  if (!trimmed || trimmed.length > maxLength) throw new TypeError(`${what} has an unusable length`)
  return trimmed
}

export function readBoolean(value: unknown, what: string): boolean {
  if (typeof value !== 'boolean') throw new TypeError(`${what} must be true or false`)
  return value
}

export function readNoArguments(args: readonly unknown[], channel: string): void {
  if (args.length > 0) throw new TypeError(`${channel} takes no arguments`)
}

/** Only a user-initiated web link leaves the app; shell and file schemes never do. */
export function readExternalUrl(value: unknown): string {
  const raw = readString(value, '链接', 2048)
  let url: URL
  try {
    url = new URL(raw)
  } catch {
    throw new TypeError('链接无法解析')
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('只允许打开 http(s) 链接')
  }
  return url.toString()
}
