import { contextBridge, ipcRenderer } from 'electron'

export type LauncherApi = {
  pickDataDir(): Promise<string | null>
  probeInspect(dataDir: string): Promise<unknown>
  attach(dataDir: string): Promise<void>
  startOwned(dataDir: string): Promise<void>
  stopOwned(): Promise<void>
  getAccessUrl(): Promise<string | null>
  openExternal(url: string): Promise<void>
  onStatus(handler: (state: unknown) => void): () => void
}

const api: LauncherApi = {
  pickDataDir: () => ipcRenderer.invoke<string | null>('launcher:pick-data-dir'),
  probeInspect: (dataDir) => ipcRenderer.invoke<unknown>('launcher:probe-inspect', dataDir),
  attach: (dataDir) => ipcRenderer.invoke<void>('launcher:attach', dataDir),
  startOwned: (dataDir) => ipcRenderer.invoke<void>('launcher:start-owned', dataDir),
  stopOwned: () => ipcRenderer.invoke<void>('launcher:stop-owned'),
  getAccessUrl: () => ipcRenderer.invoke<string | null>('launcher:get-access-url'),
  openExternal: (url) => ipcRenderer.invoke<void>('shell:open-external', url),
  onStatus: (handler) => {
    const channel = 'launcher:status'
    const wrapped = (_: unknown, payload: unknown) => handler(payload)
    ipcRenderer.on(channel, wrapped)
    return () => ipcRenderer.removeListener(channel, wrapped)
  },
}

contextBridge.exposeInMainWorld('vibefaceLauncher', api)
