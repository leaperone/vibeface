import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { isAbsolute, join } from 'node:path'
import type { Preferences } from '../shared/contract.js'

/**
 * Desktop preferences kept in the profile directory. Only the last chosen data directory
 * and plain switches are stored here: no access URL, no local token, no node credential.
 */
export type Settings = {
  lastDataDir: string | null
  preferences: Preferences
}

const FILE = 'desktop-settings.json'
const MAX_BYTES = 64 * 1024

export const defaultSettings = (): Settings => ({
  lastDataDir: null,
  preferences: { openBusinessOnConnect: true },
})

function parse(value: unknown): Settings {
  const settings = defaultSettings()
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return settings
  const source: Record<string, unknown> = Object.fromEntries(Object.entries(value))
  const lastDataDir = source['lastDataDir']
  if (typeof lastDataDir === 'string' && isAbsolute(lastDataDir)) settings.lastDataDir = lastDataDir
  const preferences = source['preferences']
  if (typeof preferences === 'object' && preferences !== null && !Array.isArray(preferences)) {
    const read: Record<string, unknown> = Object.fromEntries(Object.entries(preferences))
    if (typeof read['openBusinessOnConnect'] === 'boolean') {
      settings.preferences.openBusinessOnConnect = read['openBusinessOnConnect']
    }
  }
  return settings
}

export async function loadSettings(profileDir: string): Promise<Settings> {
  try {
    const raw = await readFile(join(profileDir, FILE), 'utf8')
    if (raw.length > MAX_BYTES) return defaultSettings()
    return parse(JSON.parse(raw))
  } catch {
    return defaultSettings()
  }
}

export async function saveSettings(profileDir: string, settings: Settings): Promise<void> {
  await mkdir(profileDir, { recursive: true })
  const target = join(profileDir, FILE)
  const scratch = `${target}.tmp`
  await writeFile(scratch, `${JSON.stringify(settings, null, 2)}\n`, { encoding: 'utf8', mode: 0o600 })
  await rename(scratch, target)
}
