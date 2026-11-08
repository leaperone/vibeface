import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ClientPreloadApi } from '../src/shared/client-contract.js'
import type { DesktopStatus } from '../src/shared/contract.js'
import { IPC } from '../src/shared/contract.js'

const mocks = vi.hoisted(() => ({
  expose: vi.fn((_name: string, _api: ClientPreloadApi) => {}),
  invoke: vi.fn(), send: vi.fn(), on: vi.fn(), removeListener: vi.fn(),
}))
vi.mock('electron', () => ({
  contextBridge: { exposeInMainWorld: mocks.expose },
  ipcRenderer: { invoke: mocks.invoke, send: mocks.send, on: mocks.on, removeListener: mocks.removeListener },
}))
const confirmed: DesktopStatus = {
  runtime: { kind: 'ready', corePath: '/fixture/core' }, core: { kind: 'none' }, clientVisible: true,
  preferences: { openBusinessOnConnect: true }, profileDir: '/fixture/profile', version: '0.0.1',
  attachOnly: false, clientAvailable: true, nodeMode: 'controller', selectedNode: null,
  discovery: { kind: 'ready', nodes: [], warnings: [] }, backgroundNode: { kind: 'absent' },
}
async function api() {
  await import('../src/preload/client.js')
  const published = mocks.expose.mock.calls[0]?.[1]
  if (!published) throw new Error('Client preload did not publish its API')
  return published
}
beforeEach(() => { vi.resetModules(); vi.clearAllMocks() })

describe('client node lifecycle acknowledgements', () => {
  it('returns the mutation result before a separate renderer reconnection request', async () => {
    const client = await api()
    let finish: (value: DesktopStatus) => void = () => {}
    mocks.invoke.mockReturnValueOnce(new Promise<DesktopStatus>(resolve => { finish = resolve }))
    const operation = client.desktop.startOwned()
    expect(mocks.invoke).toHaveBeenCalledWith(IPC.startOwned)
    expect(mocks.send).not.toHaveBeenCalled()
    finish(confirmed)
    expect(await operation).toEqual(confirmed)
    client.desktop.reconnectClient(true)
    expect(mocks.send).toHaveBeenCalledWith(IPC.reconnectClient, true)
  })

  it('focuses the existing window without implicitly requesting a document reload', async () => {
    const client = await api()
    mocks.invoke.mockResolvedValueOnce(confirmed)
    expect(await client.desktop.focusClient()).toEqual(confirmed)
    expect(mocks.invoke).toHaveBeenCalledWith(IPC.focusClient)
    expect(mocks.send).not.toHaveBeenCalled()
  })
})
