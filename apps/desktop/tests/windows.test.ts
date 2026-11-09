import { beforeEach, describe, expect, it, vi } from 'vitest'
import { shell } from 'electron'
import { createClientWindow } from '../src/main/windows.js'

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
  it('keeps a same-origin page in the client window', () => {
    const window = createClientWindow('/fixture/main')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'vibeface-app://app/join')
    expect(event.preventDefault).not.toHaveBeenCalled()
    expect(shell.openExternal).not.toHaveBeenCalled()
  })

  it('cancels external navigation and opens it in the system browser', () => {
    const window = createClientWindow('/fixture/main')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'https://outside.example/path')
    expect(event.preventDefault).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledOnce()
    expect(shell.openExternal).toHaveBeenCalledWith('https://outside.example/path')
  })

  it('cancels a non-web scheme without invoking the system opener', () => {
    const window = createClientWindow('/fixture/main')
    const event = { preventDefault: vi.fn() }
    window.webContents.emit(eventName, event, 'file:///private/example')
    expect(event.preventDefault).toHaveBeenCalledOnce()
    expect(shell.openExternal).not.toHaveBeenCalled()
  })
})

describe('single client window', () => {
  it('creates only the sealed client window factory', () => {
    expect(typeof createClientWindow).toBe('function')
  })
})
