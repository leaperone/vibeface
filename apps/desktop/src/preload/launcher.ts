import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron'
import { IPC, type LauncherApi, type LauncherStatus, type PreferenceKey } from '../shared/contract.js'

/**
 * The only privileged bridge in the app, and only the trusted launcher window loads it.
 * Each method is a fixed channel with fixed arity, so the renderer can never name a
 * channel, a path outside the directory picker, or a program to run. The main process
 * re-checks the sender and every payload before it reaches Core.
 */
const api: LauncherApi = {
  getStatus: () => ipcRenderer.invoke(IPC.getStatus),
  chooseDataDir: () => ipcRenderer.invoke(IPC.chooseDataDir),
  useDataDir: (dataDir: string) => ipcRenderer.invoke(IPC.useDataDir, String(dataDir)),
  forgetDataDir: () => ipcRenderer.invoke(IPC.forgetDataDir),
  startOwned: () => ipcRenderer.invoke(IPC.startOwned),
  attachExisting: () => ipcRenderer.invoke(IPC.attachExisting),
  stopOwned: () => ipcRenderer.invoke(IPC.stopOwned),
  openBusiness: () => ipcRenderer.invoke(IPC.openBusiness),
  discoverNodes: () => ipcRenderer.invoke(IPC.discoverNodes),
  connectDiscoveredNode: (candidateId: string) => ipcRenderer.invoke(IPC.connectDiscoveredNode, String(candidateId)),
  startDiscoveredNode: (candidateId: string) => ipcRenderer.invoke(IPC.startDiscoveredNode, String(candidateId)),
  chooseNodeMode: (mode: 'controller' | 'local') => ipcRenderer.invoke(IPC.chooseNodeMode, mode),
  installBackgroundNode: () => ipcRenderer.invoke(IPC.installBackgroundNode),
  startBackgroundNode: () => ipcRenderer.invoke(IPC.startBackgroundNode),
  stopBackgroundNode: () => ipcRenderer.invoke(IPC.stopBackgroundNode),
  removeBackgroundNode: () => ipcRenderer.invoke(IPC.removeBackgroundNode),
  setPreference: (key: PreferenceKey, value: boolean) =>
    ipcRenderer.invoke(IPC.setPreference, String(key), Boolean(value)),
  openExternal: (url: string) => ipcRenderer.invoke(IPC.openExternal, String(url)),
  onStatus: (handler: (status: LauncherStatus) => void) => {
    const listener = (_event: IpcRendererEvent, status: LauncherStatus) => handler(status)
    ipcRenderer.on(IPC.status, listener)
    return () => {
      ipcRenderer.removeListener(IPC.status, listener)
    }
  },
}

contextBridge.exposeInMainWorld('vibefaceLauncher', api)
