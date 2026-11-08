import type { WebContents } from 'electron'

const DEFAULT_TIMEOUT_MS = 30_000

/** Waits for an in-flight main-frame navigation on the sole client window, with bounded cleanup. */
export function waitForWebContentsLoad(contents: WebContents, timeoutMs = DEFAULT_TIMEOUT_MS): Promise<void> {
  if (contents.isDestroyed()) return Promise.reject(new Error('客户端窗口已关闭'))
  if (!contents.isLoadingMainFrame()) return Promise.resolve()
  return new Promise((resolve, reject) => {
    let settled = false
    const finish = (error?: Error) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      contents.removeListener('did-finish-load', onLoad)
      contents.removeListener('did-fail-load', onFail)
      if (error) reject(error)
      else resolve()
    }
    const onLoad = () => finish()
    const onFail = (_event: unknown, code: number, description: string) => {
      finish(new Error(`客户端页面加载失败 (${code}): ${description}`))
    }
    const timer = setTimeout(() => finish(new Error('等待客户端页面加载超时')), timeoutMs)
    contents.once('did-finish-load', onLoad)
    contents.once('did-fail-load', onFail)
  })
}
