/** Minimal Electron 45.x API surface (delivery installs full `electron` + types). */

declare global {
  namespace NodeJS {
    interface Process {
      resourcesPath?: string
    }
  }
}

declare module 'electron' {
  export type IpcRenderer = {
    invoke<T = unknown>(channel: string, ...args: unknown[]): Promise<T>
    on(channel: string, listener: (event: unknown, ...args: unknown[]) => void): void
    removeListener(channel: string, listener: (...args: unknown[]) => void): void
  }

  export interface DownloadItem {
    getFilename(): string
    setSavePath(path: string): void
  }

  export interface WebContents {
    send(channel: string, ...args: unknown[]): void
    setWindowOpenHandler(h: (details: { url: string }) => { action: string }): void
    on(event: string, listener: (event: unknown, ...args: unknown[]) => void): void
    session: Session
  }

  export interface Session {
    on(event: string, listener: (event: unknown, ...args: unknown[]) => void): void
    setPermissionRequestHandler(
      h: (wc: WebContents, permission: string, callback: (allow: boolean) => void) => void
    ): void
  }

  export class BrowserWindow {
    constructor(opts?: Record<string, unknown>)
    webContents: WebContents
    loadURL(url: string): Promise<void>
    show(): void
    hide(): void
    focus(): void
    isDestroyed(): boolean
    on(event: string, listener: (event: unknown, ...args: unknown[]) => void): void
    static getFocusedWindow(): BrowserWindow | null
  }

  export class Tray {
    constructor(image: unknown)
    setToolTip(t: string): void
    on(event: string, listener: () => void): void
  }

  export const app: {
    isPackaged: boolean
    getPath(name: string): string
    getAppPath(): string
    requestSingleInstanceLock(): boolean
    quit(): void
    exit(code: number): void
    setAppUserModelId(id: string): void
    setAsDefaultProtocolClient(proto: string): boolean
    whenReady(): Promise<void>
    on(event: string, listener: (...args: unknown[]) => void): void
  }

  export const session: { defaultSession: Session }
  export const clipboard: { writeText(t: string): void }
  export const ipcMain: {
    handle(
      channel: string,
      listener: (event: unknown, ...args: unknown[]) => void | Promise<unknown>
    ): void
  }
  export const dialog: {
    showOpenDialog(opts: Record<string, unknown>): Promise<{ canceled: boolean; filePaths: string[] }>
    showSaveDialog(win: BrowserWindow | undefined, opts: Record<string, unknown>): Promise<{ canceled: boolean; filePath?: string }>
    showMessageBox(opts: Record<string, unknown>): Promise<unknown>
  }
  export const Menu: {
    buildFromTemplate(t: unknown): unknown
    setApplicationMenu(m: unknown): void
  }
  export const nativeImage: {
    createFromPath(p: string): unknown
    createEmpty(): unknown
  }
  export const shell: { openExternal(url: string): Promise<void> }
  export class Notification {
    static isSupported(): boolean
    constructor(opts: { title: string; body: string })
    show(): void
  }
  export const contextBridge: { exposeInMainWorld(apiKey: string, api: Record<string, unknown>): void }
  export const ipcRenderer: IpcRenderer
}

export {}
