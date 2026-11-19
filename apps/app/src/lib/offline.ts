/**
 * Registers the app shell, checks for new workers while the page is visible, and
 * reloads only when the page has no editable content that could be lost.
 */
const UPDATE_INTERVAL_MS = 15 * 60 * 1000

function hasEditableContent(): boolean {
  const active = document.activeElement
  if (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement || active instanceof HTMLSelectElement) {
    return true
  }
  if (active instanceof HTMLElement && active.isContentEditable) return true
  return Array.from(document.querySelectorAll('input, textarea, [contenteditable="true"]')).some(element => {
    if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) return element.value.length > 0
    return element instanceof HTMLElement && Boolean(element.textContent?.trim().length)
  })
}

export function registerShell(): void | (() => void) {
  if (!globalThis.isSecureContext || !('serviceWorker' in navigator)) return

  let registration: ServiceWorkerRegistration | undefined
  let disposed = false
  const checkForUpdate = () => {
    if (!disposed) registration?.update().catch(() => {})
  }
  const onControllerChange = () => {
    if (!navigator.serviceWorker.controller || hasEditableContent()) return
    window.location.reload()
  }
  const onVisibilityChange = () => {
    if (document.visibilityState === 'visible') checkForUpdate()
  }

  navigator.serviceWorker.addEventListener('controllerchange', onControllerChange)
  window.addEventListener('online', checkForUpdate)
  document.addEventListener('visibilitychange', onVisibilityChange)
  const timer = window.setInterval(checkForUpdate, UPDATE_INTERVAL_MS)

  navigator.serviceWorker.register('/sw.js', { updateViaCache: 'none' })
    .then(value => {
      registration = value
      checkForUpdate()
      return navigator.serviceWorker.ready
    })
    .then(value => value.active?.postMessage({
      keep: performance.getEntriesByType('resource').map(entry => entry.name),
    }))
    .catch(() => {})

  return () => {
    disposed = true
    window.clearInterval(timer)
    window.removeEventListener('online', checkForUpdate)
    document.removeEventListener('visibilitychange', onVisibilityChange)
    navigator.serviceWorker.removeEventListener('controllerchange', onControllerChange)
  }
}
