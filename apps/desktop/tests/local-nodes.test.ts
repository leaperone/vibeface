import { describe, expect, it } from 'vitest'
import { parseLocalNodes } from '../src/main/local-nodes.js'
import { resolve } from 'node:path'

const first = resolve('/fixture/Node A'), second = resolve('/fixture/Node B')
const row = (dataDir = first, status = 'running') => ({ status, data_dir: dataDir, device_id: 'a'.repeat(64),
  local_listen: '127.0.0.1:4791', listen: '0.0.0.0:4790', web_enabled: true, core_version: '0.0.1-nightly.20261107.1', sources: ['user_service'] })

describe('local Node discovery parsing', () => {
  it('retains the verified absolute directory and discovery source', () => {
    const catalog = parseLocalNodes({ nodes: [row()], warnings: [] })
    expect(catalog.nodes[0]?.dataDir).toBe(first)
    expect(catalog.nodes[0]?.sources).toEqual(['user_service'])
  })
  it('retains stopped and unverified states', () => {
    const catalog = parseLocalNodes({ nodes: [row(first, 'stopped'), { ...row(second, 'unavailable'), unavailable_reason: 'identity_mismatch' }], warnings: [] })
    expect(catalog.nodes.map(node => node.kind)).toEqual(['stopped', 'unavailable'])
  })
  it('rejects an unproven source, duplicate directory and unsafe result fields', () => {
    for (const nodes of [[{ ...row(), sources: ['port_listener'] }], [row(), row()], [{ ...row(), device_id: 'unknown' }], [{ ...row(), data_dir: 'relative' }]]) {
      expect(() => parseLocalNodes({ nodes, warnings: [] })).toThrow()
    }
    expect(() => parseLocalNodes({ nodes: [], warnings: ['raw error with private details'] })).toThrow()
  })
})
