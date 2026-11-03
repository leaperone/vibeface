import { reportLog } from "@vibeface/client/lib/web-log.ts"

function describe(cause: unknown): { message: string; stack?: string } {
  if (cause instanceof Error) return { message: `${cause.name}: ${cause.message}`, stack: cause.stack }
  return { message: typeof cause === "string" ? cause : JSON.stringify(cause) ?? String(cause) }
}

/** Uncaught errors and unhandled rejections; returns the uninstall function. */
export function installErrorReporting() {
  const onError = (event: ErrorEvent) => {
    const { message, stack } = describe(event.error ?? event.message)
    reportLog({ level: "error", event: "web.uncaught", message, fields: { stack, at: `${event.filename}:${event.lineno}:${event.colno}`, path: location.pathname } })
  }
  const onRejection = (event: PromiseRejectionEvent) => {
    const { message, stack } = describe(event.reason)
    reportLog({ level: "error", event: "web.unhandled_rejection", message, fields: { stack, path: location.pathname } })
  }
  window.addEventListener("error", onError)
  window.addEventListener("unhandledrejection", onRejection)
  return () => { window.removeEventListener("error", onError); window.removeEventListener("unhandledrejection", onRejection) }
}
