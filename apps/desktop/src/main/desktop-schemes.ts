import { protocol } from 'electron'

/** Register privileged custom schemes before the app is ready. */
export function registerDesktopSchemes(additional: Parameters<typeof protocol.registerSchemesAsPrivileged>[0] = []): void {
  protocol.registerSchemesAsPrivileged([...additional])
}
