import assert from 'node:assert/strict'
import test from 'node:test'
import type { ClientPreloadApi } from '../../../desktop/src/shared/client-contract.ts'
import { desktopLocalPage } from './desktop-local-page.ts'

test('desktop invitation fragments preserve the original join URL and origin, then clear from history', () => {
  const original = 'https://node.example/join#vibeface1fixture'
  const location = new URL('vibeface-app://app/join')
  location.hash = new URLSearchParams({ invitation: original }).toString()
  let cleared: string | URL | null = null
  const history = { replaceState: (_state: unknown, _title: string, url?: string | URL | null) => { cleared = url ?? null } }
  Object.assign(globalThis, { location, history, addEventListener: () => {}, removeEventListener: () => {} })
  const unavailable = async (): Promise<never> => { throw new Error('Node operation is outside this invitation test') }
  const native: ClientPreloadApi = {
    local: { discoverLocalSetup: async () => null, localEntry: async () => null },
    carrier: { open: async () => '', openGateway: async () => null, openLan: async () => '', send: async () => {}, receive: async () => null, close: () => {} },
    desktop: {
      getStatus: async () => ({ runtime: { kind: 'ready', corePath: '/core' }, core: { kind: 'none' }, hostPhase: 'ready', clientVisible: true, preferences: { openBusinessOnConnect: true }, profileDir: '/profile', version: '0.0.1', attachOnly: false, clientAvailable: true, nodeMode: 'controller', selectedNode: null, discovery: { kind: 'idle' }, backgroundNode: { kind: 'absent' } }),
      chooseDataDir: unavailable, useDataDir: unavailable, forgetDataDir: unavailable,
      startOwned: unavailable, attachExisting: unavailable, stopOwned: unavailable,
      focusClient: unavailable, setPreference: unavailable, openExternal: async () => {},
      reconnectClient: () => {},
      onStatus: () => () => {}, discoverNodes: unavailable, connectDiscoveredNode: unavailable,
      startDiscoveredNode: unavailable, chooseNodeMode: unavailable, installBackgroundNode: unavailable,
      startBackgroundNode: unavailable, stopBackgroundNode: unavailable, removeBackgroundNode: unavailable, updateBackgroundNode: unavailable,
    },
    showNodePanel: async () => {},
  }
  const launch = desktopLocalPage(native).takeLaunch()
  assert.equal(launch.invitation, original)
  assert.equal(String(cleared), 'vibeface-app://app/join')
  assert.equal(launch.invitation.includes('vibeface-app://'), false)
})
