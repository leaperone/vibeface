import { isAbsolute } from 'node:path'

/**
 * The only command line the desktop reads. `--profile-dir` exists so an e2e run keeps its
 * preferences, Chromium profile and owned Core away from the real user profile; `--attach`
 * makes a run connection-only, so it never starts or stops a Core.
 */
export type DesktopArgs = {
  profileDir: string | null
  attachOnly: boolean
}

export function parseArgs(argv: readonly string[]): DesktopArgs {
  let profileDir: string | null = null
  let attachOnly = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--attach') {
      attachOnly = true
      continue
    }
    if (argument === '--profile-dir') {
      const value = argv[index + 1]
      if (!value || !isAbsolute(value)) throw new Error('--profile-dir requires an absolute path')
      profileDir = value
      index += 1
      continue
    }
    const inline = argument?.startsWith('--profile-dir=') ? argument.slice('--profile-dir='.length) : null
    if (inline !== null) {
      if (!inline || !isAbsolute(inline)) throw new Error('--profile-dir requires an absolute path')
      profileDir = inline
    }
  }
  return { profileDir, attachOnly }
}
