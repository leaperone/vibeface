import { describe, expect, it } from 'vitest'
import { parseManagedNodeStatus } from '../src/main/managed-node.js'

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
  })
})
