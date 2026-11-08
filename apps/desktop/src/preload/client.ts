import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron'
import { IPC, type DesktopNodeApi, type DesktopStatus, type PreferenceKey } from '../shared/contract.js'
import { CLIENT_IPC, type ClientPreloadApi } from '../shared/client-contract.js'

const desktop: DesktopNodeApi = {
  getStatus: () => ipcRenderer.invoke(IPC.getStatus),
  chooseDataDir: () => ipcRenderer.invoke(IPC.chooseDataDir),
  useDataDir: (dataDir: string) => ipcRenderer.invoke(IPC.useDataDir, String(dataDir)),
  forgetDataDir: () => ipcRenderer.invoke(IPC.forgetDataDir),
  startOwned: () => ipcRenderer.invoke(IPC.startOwned),
  attachExisting: () => ipcRenderer.invoke(IPC.attachExisting),
  stopOwned: () => ipcRenderer.invoke(IPC.stopOwned),
  focusClient: () => ipcRenderer.invoke(IPC.focusClient),
  reconnectClient: (showChat: boolean) => ipcRenderer.send(IPC.reconnectClient, Boolean(showChat)),
  discoverNodes: () => ipcRenderer.invoke(IPC.discoverNodes),
  connectDiscoveredNode: (candidateId: string) => ipcRenderer.invoke(IPC.connectDiscoveredNode, String(candidateId)),
  startDiscoveredNode: (candidateId: string) => ipcRenderer.invoke(IPC.startDiscoveredNode, String(candidateId)),
  chooseNodeMode: (mode: 'controller' | 'local') => ipcRenderer.invoke(IPC.chooseNodeMode, mode),
  installBackgroundNode: () => ipcRenderer.invoke(IPC.installBackgroundNode),
  startBackgroundNode: () => ipcRenderer.invoke(IPC.startBackgroundNode),
  stopBackgroundNode: () => ipcRenderer.invoke(IPC.stopBackgroundNode),
  removeBackgroundNode: () => ipcRenderer.invoke(IPC.removeBackgroundNode),
  updateBackgroundNode: () => ipcRenderer.invoke(IPC.updateBackgroundNode),
  setPreference: (key: PreferenceKey, value: boolean) =>
    ipcRenderer.invoke(IPC.setPreference, String(key), Boolean(value)),
  openExternal: (url: string) => ipcRenderer.invoke(IPC.openExternal, String(url)),
  onStatus: (handler: (status: DesktopStatus) => void) => {
    const listener = (_event: IpcRendererEvent, status: DesktopStatus) => handler(status)
    ipcRenderer.on(IPC.status, listener)
    return () => {
      ipcRenderer.removeListener(IPC.status, listener)
    }
  },
}

/** Purpose-specific access for the sealed client window at CLIENT_ORIGIN only. */
const api: ClientPreloadApi = {
  local: {
    discoverLocalSetup: () => ipcRenderer.invoke(CLIENT_IPC.localSetup),
    localEntry: (deviceId, launch) => ipcRenderer.invoke(CLIENT_IPC.localEntry, String(deviceId), launch),
  },
  carrier: {
    open: (deviceId, timeoutMs) => ipcRenderer.invoke(CLIENT_IPC.carrierOpen, String(deviceId), Number(timeoutMs)),
    openGateway: (deviceId, address, timeoutMs) => ipcRenderer.invoke(CLIENT_IPC.gatewayOpen, String(deviceId), String(address), Number(timeoutMs)),
    openLan: (deviceId, addrs, timeoutMs) => ipcRenderer.invoke(CLIENT_IPC.lanOpen, String(deviceId), addrs.map(String), Number(timeoutMs)),
    send: (id, bytes) => ipcRenderer.invoke(CLIENT_IPC.carrierSend, String(id), new Uint8Array(bytes)),
    receive: id => ipcRenderer.invoke(CLIENT_IPC.carrierReceive, String(id)),
    close: id => { void ipcRenderer.invoke(CLIENT_IPC.carrierClose, String(id)).catch(() => {}) },
  },
  desktop,
  showNodePanel: () => ipcRenderer.invoke(CLIENT_IPC.showNodePanel),
}

contextBridge.exposeInMainWorld('vibefaceClient', api)
