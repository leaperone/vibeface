import { describe, expect, it, beforeAll, afterAll } from 'vitest'
import { join } from 'node:path'
import { runControl } from '../src/main/control-client.js'

describe('CoreLifecycle + fake core', () => {
  beforeAll(() => {
    process.env.VIBEFACE_DESKTOP_FAKE_CONTROL = join(import.meta.dirname, 'fake-core.mjs')
    process.env.VIBEFACE_DESKTOP_FAKE_INSPECT_JSON = JSON.stringify({
      status: 'running',
      device_id: 'aa',
      local_listen: '127.0.0.1:4791',
    })
  })
  afterAll(() => {
    delete process.env.VIBEFACE_DESKTOP_FAKE_CONTROL
    delete process.env.VIBEFACE_DESKTOP_FAKE_INSPECT_JSON
  })
  it('inspect and access use fixed bundled binary path', async () => {
    const inspect = await runControl('/nonexistent/vibeface-core', {
      command: 'inspect',
      data_dir: '/tmp/fake-data',
    })
    expect((inspect as { local_listen?: string }).local_listen).toBe('127.0.0.1:4791')
    const access = await runControl('/nonexistent/vibeface-core', {
      command: 'access',
      data_dir: '/tmp/fake-data',
      json: false,
    })
    expect((access as { url?: string }).url).toBeDefined()
  })
})
