import { EventEmitter } from 'node:events'
import { describe, expect, it } from 'vitest'
import { waitForWebContentsLoad } from '../src/main/client-navigation.js'

type MockContents = EventEmitter & {
  isDestroyed: () => boolean
  isLoadingMainFrame: () => boolean
}

const contents = (loading: boolean): MockContents => {
  const emitter = new EventEmitter() as MockContents
  emitter.isDestroyed = () => false
  emitter.isLoadingMainFrame = () => loading
  return emitter
}

describe('waitForWebContentsLoad', () => {
  it('resolves immediately when the main frame is not loading', async () => {
    await expect(waitForWebContentsLoad(contents(false) as never)).resolves.toBeUndefined()
  })

  it('waits for did-finish-load before resolving', async () => {
    const live = contents(true)
    const pending = waitForWebContentsLoad(live as never, 500)
    live.emit('did-finish-load')
    await expect(pending).resolves.toBeUndefined()
  })

  it('rejects when did-fail-load fires', async () => {
    const live = contents(true)
    const pending = waitForWebContentsLoad(live as never, 500)
    live.emit('did-fail-load', {}, -3, 'aborted')
    await expect(pending).rejects.toThrow(/加载失败/)
  })
})
