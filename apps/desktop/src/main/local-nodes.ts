import { createHash } from 'node:crypto'
import { isAbsolute } from 'node:path'
import { runControl, type CoreProgram } from './control-client.js'
import type { LocalNode, NodeSource } from '../shared/contract.js'

export type LocalNodes = { nodes: LocalNode[]; warnings: string[] }

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new TypeError('节点发现结果格式错误')
  return Object.fromEntries(Object.entries(value))
}

function text(value: unknown, max: number): string {
  if (typeof value !== 'string' || value.length === 0 || value.length > max || /[\x00-\x1f]/.test(value)) {
    throw new TypeError('节点发现结果包含无效文字')
  }
  return value
}

function source(value: unknown): NodeSource {
  if (value === 'remembered' || value === 'app_profile' || value === 'default_home' || value === 'user_service' || value === 'running_process') return value
  throw new TypeError('节点发现来源无效')
}

export function parseLocalNodes(value: unknown): LocalNodes {
  const result = record(value)
  const rows = result['nodes'], warnings = result['warnings']
  if (!Array.isArray(rows) || rows.length > 64 || !Array.isArray(warnings) || warnings.length > 128) {
    throw new TypeError('节点发现结果超出范围')
  }
  const seen = new Set<string>()
  const nodes = rows.map((item: unknown): LocalNode => {
    const row = record(item)
    const deviceId = text(row['device_id'], 64)
    if (!/^[a-f0-9]{64}$/.test(deviceId)) throw new TypeError('节点身份无效')
    const dataDir = text(row['data_dir'], 4096)
    if (!isAbsolute(dataDir) || seen.has(dataDir)) throw new TypeError('节点目录无效或重复')
    seen.add(dataDir)
    const rawSources = row['sources']
    if (!Array.isArray(rawSources) || rawSources.length === 0 || rawSources.length > 5) throw new TypeError('节点来源缺失')
    const sources = rawSources.map(source)
    if (new Set(sources).size !== sources.length) throw new TypeError('节点来源重复')
    const identity = { id: createHash('sha256').update(dataDir).update('\0').update(deviceId).digest('hex'), deviceId, dataDir, sources }
    if (row['status'] === 'unavailable') {
      const reason = row['unavailable_reason']
      if (reason !== 'identity_mismatch' && reason !== 'maintenance_unavailable' && reason !== 'inspection_unavailable') throw new TypeError('节点不可用原因无效')
      return { ...identity, kind: 'unavailable', reason }
    }
    if (row['status'] !== 'running' && row['status'] !== 'stopped') throw new TypeError('节点运行状态无效')
    if (typeof row['web_enabled'] !== 'boolean') throw new TypeError('节点接入状态无效')
    return { ...identity, kind: row['status'], localListen: text(row['local_listen'], 256), listen: text(row['listen'], 256),
      webEnabled: row['web_enabled'], coreVersion: text(row['core_version'], 160) }
  })
  return { nodes, warnings: warnings.map((warning: unknown) => {
    const code = text(warning, 80)
    if (!/^[a-z_]+$/.test(code)) throw new TypeError('节点发现提示无效')
    return code
  }) }
}

/** Read-only discovery; the native Core performs account, path, identity and health checks. */
export async function discoverLocalNodes(program: CoreProgram, profileDir: string, rememberedDataDirs: string[]): Promise<LocalNodes> {
  return parseLocalNodes(await runControl(program, { command: 'discover_local_nodes', app_profile_dir: profileDir,
    remembered_data_dirs: rememberedDataDirs }))
}
