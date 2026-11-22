import { describeError, reportLog } from '@vibeface/client/lib/web-log.ts'

/**
 * Imported once by the root layout, as the polyfills are. Uncaught errors and unhandled rejections go to the
 * connected device's log, as on the web. The previous global handler still runs, so React Native's own red box and
 * crash reporting stay. Hermes keeps one rejection tracker, replaced here; in development its warning still reaches
 * LogBox through console.warn.
 */
type RejectionTracker = {
  enablePromiseRejectionTracker?(options: { allRejections: boolean; onUnhandled(id: number, rejection: unknown): void; onHandled(id: number): void }): void
}

const previous = ErrorUtils.getGlobalHandler()
ErrorUtils.setGlobalHandler((error, isFatal) => {
  const { message, stack } = describeError(error)
  // The log only sends asynchronously, so a fatal error that ends the app at once may never reach the device.
  reportLog({ level: 'error', event: 'mobile.uncaught', message, fields: { stack, fatal: isFatal === true } })
  previous(error, isFatal)
})
;(globalThis as { HermesInternal?: RejectionTracker | null }).HermesInternal?.enablePromiseRejectionTracker?.({
  allRejections: true,
  onUnhandled(id, rejection) {
    const { message, stack } = describeError(rejection)
    reportLog({ level: 'error', event: 'mobile.unhandled_rejection', message, fields: { stack } })
    if (__DEV__) console.warn(`Uncaught (in promise, id: ${id}): ${message}`)
  },
  onHandled() {},
})
