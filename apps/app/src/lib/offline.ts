/** Registers the app-shell service worker and hands it what this page already loaded. */
export function registerShell() {
  if (!globalThis.isSecureContext || !('serviceWorker' in navigator)) return
  navigator.serviceWorker.register('/sw.js')
    .then(() => navigator.serviceWorker.ready)
    .then(registration => registration.active?.postMessage({ keep: performance.getEntriesByType('resource').map(entry => entry.name) }))
    .catch(() => {}) // ponytail: without a worker the page still works online; offline open is the only loss
}
