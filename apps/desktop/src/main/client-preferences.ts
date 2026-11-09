import { lstat, mkdir, readFile, rename, unlink, writeFile } from 'node:fs/promises'
import { isAbsolute, join } from 'node:path'
import { randomUUID } from 'node:crypto'
import type { DesktopStatus } from '../shared/contract.js'

export type ClientPreferences = {
  nodeMode: DesktopStatus['nodeMode']
  selectedNode: DesktopStatus['selectedNode']
  rememberedDataDirs: string[]
}
const FILE = 'client-preferences.json'
export const defaultClientPreferences = (): ClientPreferences => ({ nodeMode: 'undecided', selectedNode: null, rememberedDataDirs: [] })
const safePath = (value: unknown): value is string => typeof value === 'string' && value.length <= 4096 && isAbsolute(value) && !/[\x00-\x1f]/.test(value)

export async function loadClientPreferences(profileDir: string): Promise<ClientPreferences> {
  try {
    const file = join(profileDir, FILE)
    const metadata = await lstat(file)
    if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > 64 * 1024) return defaultClientPreferences()
    const value: unknown = JSON.parse(await readFile(file, 'utf8'))
    if (typeof value !== 'object' || value === null || Array.isArray(value)) return defaultClientPreferences()
    const result = defaultClientPreferences()
    if ('nodeMode' in value && (value.nodeMode === 'undecided' || value.nodeMode === 'controller' || value.nodeMode === 'local')) result.nodeMode = value.nodeMode
    if ('rememberedDataDirs' in value && Array.isArray(value.rememberedDataDirs)) result.rememberedDataDirs = [...new Set(value.rememberedDataDirs.filter(safePath))].slice(0, 16)
    if ('selectedNode' in value && typeof value.selectedNode === 'object' && value.selectedNode !== null &&
        'dataDir' in value.selectedNode && safePath(value.selectedNode.dataDir) && 'deviceId' in value.selectedNode &&
        typeof value.selectedNode.deviceId === 'string' && /^[a-f0-9]{64}$/.test(value.selectedNode.deviceId)) {
      result.selectedNode = { dataDir: value.selectedNode.dataDir, deviceId: value.selectedNode.deviceId }
    }
    return result
  } catch {
    return defaultClientPreferences()
  }
}

/** This actor's file is separate from CoreSession preferences; no token, invite or private key is stored. */
export async function saveClientPreferences(profileDir: string, value: ClientPreferences): Promise<void> {
  await mkdir(profileDir, { recursive: true })
  const target = join(profileDir, FILE), temporary = `${target}.${randomUUID()}.tmp`
  const stored = { nodeMode: value.nodeMode,
    selectedNode: value.selectedNode ? { dataDir: value.selectedNode.dataDir, deviceId: value.selectedNode.deviceId } : null,
    rememberedDataDirs: [...value.rememberedDataDirs] }
  try {
    await writeFile(temporary, `${JSON.stringify(stored)}\n`, { encoding: 'utf8', mode: 0o600, flag: 'wx' })
    await rename(temporary, target)
  } finally {
    await unlink(temporary).catch(() => undefined)
  }
}
