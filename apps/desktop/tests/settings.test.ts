import { mkdtemp, readFile, rm, writeFile, mkdir } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { defaultSettings, loadSettings, saveSettings } from '../src/main/settings.js'

describe('desktop settings', () => {
  let profileDir = ''

  beforeEach(async () => {
    profileDir = await mkdtemp(join(tmpdir(), 'vibeface-settings-'))
  })

  afterEach(async () => {
    await rm(profileDir, { recursive: true, force: true })
  })

  it('starts with no remembered directory and auto-open on', () => {
    expect(defaultSettings()).toEqual({ lastDataDir: null, preferences: { openBusinessOnConnect: true } })
  })

  it('round-trips through the profile directory', async () => {
    await saveSettings(profileDir, { lastDataDir: '/tmp/node', preferences: { openBusinessOnConnect: false } })
    expect(await loadSettings(profileDir)).toEqual({
      lastDataDir: '/tmp/node',
      preferences: { openBusinessOnConnect: false },
    })
  })

  it('ignores a relative path and unknown keys in a hand-edited file', async () => {
    await mkdir(profileDir, { recursive: true })
    await writeFile(
      join(profileDir, 'desktop-settings.json'),
      JSON.stringify({ lastDataDir: '../elsewhere', corePath: '/bin/sh', preferences: { openBusinessOnConnect: 'yes' } })
    )
    expect(await loadSettings(profileDir)).toEqual(defaultSettings())
  })

  it('falls back to defaults for an unreadable file instead of failing to start', async () => {
    await mkdir(profileDir, { recursive: true })
    await writeFile(join(profileDir, 'desktop-settings.json'), 'not json')
    expect(await loadSettings(profileDir)).toEqual(defaultSettings())
  })

  it('writes the file private to the user', async () => {
    await saveSettings(profileDir, defaultSettings())
    const content = await readFile(join(profileDir, 'desktop-settings.json'), 'utf8')
    expect(JSON.parse(content)).toEqual(defaultSettings())
  })
})
