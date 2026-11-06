import { existsSync } from 'node:fs'
import { join } from 'node:path'
import { app } from 'electron'

/** Directory containing vibeface-core, vibeface CLI, web/, etc. */
export function runtimeRoot(): string {
  const override = process.env.VIBEFACE_DESKTOP_RUNTIME?.trim()
  if (override && existsSync(override)) return override
  if (app.isPackaged && process.resourcesPath) {
    const packaged = join(process.resourcesPath, 'runtime')
    if (existsSync(packaged)) return packaged
  }
  const devCandidates = [
    join(process.cwd(), 'fixtures', 'runtime'),
    join(process.cwd(), 'apps', 'desktop', 'fixtures', 'runtime'),
  ]
  for (const path of devCandidates) {
    if (existsSync(path)) return path
  }
  if (override) return override
  throw new Error(
    'set VIBEFACE_DESKTOP_RUNTIME to a directory with vibeface-core and web/, or add apps/desktop/fixtures/runtime'
  )
}

export function coreBinary(): string {
  const fromEnv = process.env.VIBEFACE_DESKTOP_CORE?.trim()
  if (fromEnv && existsSync(fromEnv)) return fromEnv
  const root = runtimeRoot()
  const base = process.platform === 'win32' ? 'vibeface-core.exe' : 'vibeface-core'
  return join(root, base)
}

export function webRoot(): string {
  return join(runtimeRoot(), 'web')
}
