import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { runControl } from '../src/main/control-client.js'
import { parseAccess, parseControlResponse, parseInspect } from '../src/main/control-types.js'
import { CONTROL_FIXTURE } from './fixtures.js'

describe('parseInspect', () => {
  const reply = {
    status: 'running',
    device_id: 'a'.repeat(64),
    data_dir: '/tmp/node',
    maintenance_socket: '/tmp/vibeface-x/maintenance.sock',
    listen: '127.0.0.1:4790',
    local_listen: '127.0.0.1:4791',
    public_url: 'http://127.0.0.1:4790',
    web_enabled: true,
    version: '0.0.1',
    write_state: null,
    peers: [],
    core_version: '0.0.1',
    error: null,
  }

  it('reads the fields Core actually reports', () => {
    const inspect = parseInspect(reply)
    expect(inspect.localListen).toBe('127.0.0.1:4791')
    expect(inspect.status).toBe('running')
    expect(inspect.error).toBeNull()
  })

  it('refuses a reply that is missing local_listen', () => {
    const { local_listen: _dropped, ...rest } = reply
    expect(() => parseInspect(rest)).toThrow(/local_listen/)
  })

  it('refuses a status Core never produces', () => {
    expect(() => parseInspect({ ...reply, status: 'pending' })).toThrow(/unknown status/)
  })

  it('refuses a reply that is not an object', () => {
    expect(() => parseInspect('running')).toThrow(/JSON object/)
  })
})

describe('parseAccess', () => {
  it('refuses a reply with no URL', () => {
    expect(() => parseAccess({ device_id: 'a' })).toThrow(/url/)
  })
})

describe('parseControlResponse', () => {
  it('keeps Core’s own failure text', () => {
    expect(parseControlResponse({ ok: false, error: '端口 4790 被占用' })).toEqual({
      ok: false,
      error: '端口 4790 被占用',
    })
  })

  it('refuses a response without ok', () => {
    expect(() => parseControlResponse({ result: {} })).toThrow(/missing ok/)
  })
})

describe('runControl against a protocol fixture', () => {
  let dataDir = ''

  beforeAll(async () => {
    dataDir = await mkdtemp(join(tmpdir(), 'vibeface-control-'))
    await writeFile(join(dataDir, 'config.json'), '{}')
    await writeFile(join(dataDir, 'identity.json'), '{}')
  })

  afterAll(async () => {
    await rm(dataDir, { recursive: true, force: true })
  })

  it('returns the parsed result of one bounded request', async () => {
    const result = await runControl(CONTROL_FIXTURE, { command: 'inspect', data_dir: dataDir })
    expect(parseInspect(result).localListen).toBe('127.0.0.1:4791')
  })

  it('raises Core’s own error text instead of a generic failure', async () => {
    await expect(
      runControl(CONTROL_FIXTURE, { command: 'inspect', data_dir: join(dataDir, 'absent') })
    ).rejects.toThrow(/run vibeface init first/)
  })

  it('reports an unreadable program rather than hanging', async () => {
    await expect(
      runControl({ command: join(dataDir, 'not-a-program'), args: ['--control'] }, { command: 'inspect', data_dir: dataDir })
    ).rejects.toThrow()
  })

  it('rejects a nominal JSON response when the control process exits non-zero', async () => {
    process.env['FIXTURE_CONTROL_EXIT_CODE'] = '1'
    try {
      await expect(runControl(CONTROL_FIXTURE, { command: 'inspect', data_dir: dataDir })).rejects.toThrow(
        /exited with code 1/
      )
    } finally {
      delete process.env['FIXTURE_CONTROL_EXIT_CODE']
    }
  })

  it('rejects a nominal JSON response when the control process exits by signal', async () => {
    process.env['FIXTURE_CONTROL_EXIT_SIGNAL'] = 'SIGTERM'
    try {
      await expect(runControl(CONTROL_FIXTURE, { command: 'inspect', data_dir: dataDir })).rejects.toThrow(/exited with/)
    } finally {
      delete process.env['FIXTURE_CONTROL_EXIT_SIGNAL']
    }
  })
})
