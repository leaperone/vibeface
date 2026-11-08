import type { EventEmitter } from 'node:events'
import type { WebContents } from 'electron'

type ReloadableContents = EventEmitter & Pick<WebContents, 'isDestroyed' | 'reload'>

/** Observe the main-frame load before requesting a reload of the existing window. */
export function reloadClientContents(contents: ReloadableContents, timeoutMs = 30_000): Promise<void> {
  if (contents.isDestroyed()) return Promise.reject(new Error('客户端窗口已关闭'))
  return new Promise((resolve, reject) => {
    let settled = false
    const finish = (error?: Error) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      contents.removeListener('did-finish-load', onLoad)
      contents.removeListener('did-fail-load', onFail)
      contents.removeListener('destroyed', onClosed)
      if (error) reject(error)
      else resolve()
    }
    const onLoad = () => finish()
    const onClosed = () => finish(new Error('客户端窗口已关闭'))
    const onFail = (_event: unknown, code: number, description: string, _url: string, isMainFrame: boolean) => {
      if (isMainFrame === false) return
      finish(new Error(`客户端页面加载失败 (${code}): ${description}`))
    }
    const timer = setTimeout(() => finish(new Error('等待客户端页面加载超时')), timeoutMs)
    contents.once('did-finish-load', onLoad)
    contents.on('did-fail-load', onFail)
    contents.once('destroyed', onClosed)
    try { contents.reload() } catch { finish(new Error('无法重载客户端页面')) }
  })
}
