import { contextBridge, ipcRenderer } from 'electron'
import { CLIENT_IPC, type ClientPreloadApi } from '../shared/client-contract.js'

/** Purpose-specific access for the sealed client window. No channel names or URLs are caller supplied. */
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
  showNodePanel: () => ipcRenderer.invoke(CLIENT_IPC.showNodePanel),
}

contextBridge.exposeInMainWorld('vibefaceClient', api)
