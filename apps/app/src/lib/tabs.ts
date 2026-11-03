/**
 * One tab per origin leads (holds the connection); the others follow its broadcasts.
 * Closing or crashing the leader releases the Web Lock and the next tab takes over.
 */
export function joinTabs<T>(name: string, handlers: { lead(): void | (() => void); message(value: T): void }) {
  const channel = new BroadcastChannel(name)
  channel.onmessage = (event: MessageEvent<T>) => handlers.message(event.data)
  const stop = new AbortController()
  navigator.locks.request(`${name}:leader`, { signal: stop.signal }, () => new Promise<void>(release => {
    const resign = handlers.lead()
    stop.signal.addEventListener('abort', () => { resign?.(); release() }, { once: true })
  })).catch(() => {}) // ponytail: an abort before the lock is granted rejects; nothing to undo then
  return {
    post(value: T) { channel.postMessage(value) },
    close() { stop.abort(); channel.close() },
  }
}
