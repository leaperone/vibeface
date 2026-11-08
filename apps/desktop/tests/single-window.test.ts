import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const mainSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/main/index.ts'), 'utf8')

describe('single-window desktop host', () => {
  it('does not create a separate launcher window', () => {
    expect(mainSource).not.toMatch(/createLauncherWindow/)
    expect(mainSource).not.toMatch(/launcherWindow/)
    expect(mainSource).not.toMatch(/serveLauncher/)
  })

  it('broadcasts node status to the sole client window', () => {
    expect(mainSource).toMatch(/clientWindow/)
    expect(mainSource).toMatch(/IPC\.status/)
  })

  it('routes the node panel shortcut into app settings', () => {
    expect(mainSource).toMatch(/DESKTOP_NODE_SETTINGS_PATH/)
    expect(mainSource).toMatch(/showNodePanel: openNodeSettings/)
  })
})
