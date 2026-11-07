import { LAUNCHER_URL } from '../shared/contract.js'

/**
 * Privileged IPC is reachable from exactly one place: the top frame of the trusted launcher
 * window, loaded from the launcher's own fixed URL. Identity of the WebContents and of the
 * frame is checked together with that URL, because a `file:`-style origin serializes to
 * `null` and an origin alone would also accept a subframe or another window.
 */
export type SenderIdentity = {
  sender: object | null
  senderFrame: object | null
  frameUrl: string | null
  frameParent: object | null
}

export type LauncherIdentity = {
  contents: object | null
  mainFrame: object | null
}

export function isTrustedLauncherCall(sender: SenderIdentity, launcher: LauncherIdentity): boolean {
  if (!launcher.contents || !launcher.mainFrame) return false
  if (sender.sender !== launcher.contents) return false
  if (sender.senderFrame !== launcher.mainFrame) return false
  if (sender.frameParent !== null) return false
  return sender.frameUrl === LAUNCHER_URL
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
