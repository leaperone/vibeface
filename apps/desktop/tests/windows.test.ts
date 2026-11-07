import { beforeEach, describe, expect, it, vi } from 'vitest'
import { shell } from 'electron'
import { createBusinessWindow, createLauncherWindow } from '../src/main/windows.js'

vi.mock('electron', async () => {
  const { EventEmitter } = await import('node:events')
  return {
    BrowserWindow: class {
      webContents = Object.assign(new EventEmitter(), { setWindowOpenHandler: vi.fn() })
      setMenuBarVisibility = vi.fn()
      loadURL = vi.fn().mockResolvedValue(undefined)
    },
    shell: { openExternal: vi.fn().mockResolvedValue(undefined) },
  }
})

beforeEach(() => vi.clearAllMocks())

describe.each(['will-navigate', 'will-redirect'])('%s boundary', (eventName) => {
  it('keeps a same-origin page in the business window', () => {
    const window = createBusinessWindow('http://127.0.0.1:4791')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'http://127.0.0.1:4791/join')
    expect(event.preventDefault).not.toHaveBeenCalled()
    expect(shell.openExternal).not.toHaveBeenCalled()
  })

  it('cancels external navigation and opens it in the system browser', () => {
    const window = createBusinessWindow('http://127.0.0.1:4791')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'https://outside.example/path')
    expect(event.preventDefault).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledWith('https://outside.example/path')
  })

  it('cancels a non-web scheme without invoking the system opener', () => {
    const window = createBusinessWindow('http://127.0.0.1:4791')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'file:///private/example')
    expect(event.preventDefault).toHaveBeenCalledOnce()
    expect(shell.openExternal).not.toHaveBeenCalled()
  })

  it('prevents the trusted launcher from following an external page', () => {
    const window = createLauncherWindow('/fixture/main')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'https://outside.example/path')
    expect(event.preventDefault).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledWith('https://outside.example/path')
  })
})
