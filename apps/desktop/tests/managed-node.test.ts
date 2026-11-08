import { describe, expect, it } from 'vitest'
import { ManagedNodeController, commandTimeoutMs, parseManagedNodeStatus } from '../src/main/managed-node.js'

describe('ManagedNodeController CLI JSON boundary', () => {
  it('normalizes snake_case runtime_update into typed camelCase fields', () => {
    const raw = {
      ok: true,
      state: 'installed',
      device_id: 'a'.repeat(64),
      data_dir: '/Users/me/Library/Application Support/vibeface/profile/node',
      service_id: 'desktop-abcd',
      managed_root: '/Users/me/Library/Application Support/vibeface/profile/node-runtime',
      service_running: true,
      core_status: 'running',
      runtime_update: {
        bundled_version: '0.0.1-nightly.20261108.1',
        installed_version: '0.0.1-nightly.20261108.1',
        running_version: '0.0.1-nightly.20261108.1',
        availability: 'current',
      },
    }
    const status = parseManagedNodeStatus(raw)
    expect(status.runtime_update).toEqual({
      bundledVersion: '0.0.1-nightly.20261108.1',
      installedVersion: '0.0.1-nightly.20261108.1',
      runningVersion: '0.0.1-nightly.20261108.1',
      availability: 'current',
    })
    expect(status.state).toBe('installed')
    if (status.state === 'installed') expect(status.service_running).toBe(true)
  })

  it('rejects camelCase, empty, and unknown runtime metadata', () => {
    const valid = {
      ok: true,
      state: 'installed',
      device_id: 'device',
      data_dir: '/node',
      service_id: 'desktop-1',
      managed_root: '/runtime',
      service_running: true,
      core_status: 'running',
      runtime_update: {
        bundled_version: '0.0.1',
        installed_version: '0.0.1',
        running_version: null,
        availability: 'current',
      },
    }
    expect(() => parseManagedNodeStatus({
      ...valid,
      runtime_update: {
        bundledVersion: '0.0.1',
        installed_version: '0.0.1',
        running_version: null,
        availability: 'current',
      },
    })).toThrow(/invalid runtime update metadata/)
    expect(() => parseManagedNodeStatus({ ...valid, runtime_update: { ...valid.runtime_update, bundled_version: '' } })).toThrow(/bundled runtime version/)
    expect(() => parseManagedNodeStatus({ ...valid, runtime_update: { ...valid.runtime_update, availability: 'busy' } })).toThrow(/unsupported runtime update availability/)
    expect(() => parseManagedNodeStatus({ ...valid, extra: true })).toThrow(/unsupported result/)
    expect(() => parseManagedNodeStatus({ ...valid, core_status: 'busy' })).toThrow(/unsupported result/)
    expect(() => parseManagedNodeStatus({ ...valid, service_running: null })).toThrow(/unsupported result/)
  })
})

describe('ManagedNodeController deadlines', () => {
  it('keeps status short and covers the update transaction', () => {
    expect(commandTimeoutMs('status')).toBe(30_000)
    expect(commandTimeoutMs('install')).toBe(30_000)
    expect(commandTimeoutMs('start')).toBe(30_000)
    expect(commandTimeoutMs('update')).toBeGreaterThanOrEqual(120_000)
    expect(commandTimeoutMs('update')).toBeGreaterThan(commandTimeoutMs('status') + 45_000)
  })

  const absent = {
    ok: true,
    state: 'not_installed',
    device_id: null,
    data_dir: null,
    service_id: null,
    managed_root: null,
    service_running: null,
    core_status: null,
    runtime_update: {
      bundled_version: '0.0.1',
      installed_version: null,
      running_version: null,
      availability: 'not_installed',
    },
  }

  it('reads one JSON result and hides stderr from the caller', async () => {
    const payload = JSON.stringify(absent)
    const controller = new ManagedNodeController({
      profileDir: '/tmp/profile',
      bundledCli: { command: process.execPath, args: ['-e', `process.stdout.write(${JSON.stringify(payload)})`] },
      commandTimeoutMs: () => 2_000,
    })
    await expect(controller.status()).resolves.toMatchObject({ state: 'not_installed', runtime_update: { bundledVersion: '0.0.1', availability: 'not_installed' } })
    const leaking = new ManagedNodeController({
      profileDir: '/tmp/profile',
      bundledCli: { command: process.execPath, args: ['-e', 'process.stderr.write("token=secret"); process.exit(1)'] },
      commandTimeoutMs: () => 2_000,
    })
    await expect(leaking.status()).rejects.toThrow('managed Node operation failed')
    await expect(leaking.status()).rejects.not.toThrow(/secret/)
  })

  it('fails a command that exceeds its own deadline without treating that as success', async () => {
    const hung = new ManagedNodeController({
      profileDir: '/tmp/profile',
      bundledCli: { command: process.execPath, args: ['-e', 'setTimeout(() => {}, 30_000)'] },
      commandTimeoutMs: () => 200,
    })
    await expect(hung.updateBackground('device')).rejects.toThrow('managed Node operation failed')
  })
})
