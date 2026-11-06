import { contextBridge, ipcRenderer } from 'electron'

/** Narrow surface — business window must not receive arbitrary shell paths. */
export type BusinessApi = {
  notify(body: { title: string; body: string; tag?: string }): Promise<void>
  requestShow(): Promise<void>
  onShow(handler: () => void): () => void
  onDeepLink(handler: (link: string) => void): () => void
}

const api: BusinessApi = {
  notify: (payload) => ipcRenderer.invoke<void>('business:notify', payload),
  requestShow: () => ipcRenderer.invoke<void>('business:request-show'),
  onShow: (handler) => {
    const ch = 'business:show'
    const fn = () => handler()
    ipcRenderer.on(ch, fn)
    return () => ipcRenderer.removeListener(ch, fn)
  },
  onDeepLink: (handler) => {
    const ch = 'business:deeplink'
    const fn = (_event: unknown, ...args: unknown[]) => {
      const link = args[0]
      if (typeof link === 'string') handler(link)
    }
    ipcRenderer.on(ch, fn)
    return () => ipcRenderer.removeListener(ch, fn)
  },
}

contextBridge.exposeInMainWorld('vibefaceDesktop', api)
