import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { hookHarness, moduleLoader } from '../../../app/src/lib/test-harness.ts'
import type { useClipboardAction } from '../lib/use-clipboard.ts'

function deferred() {
  let resolve = () => {}, reject = (_reason: Error) => {}
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}

function fixture(initialSource: unknown = 'visible value', duration = 1500) {
  const hooks = hookHarness()
  const requests: ({ value: string } & ReturnType<typeof deferred>)[] = []
  const announcements: string[] = []
  const timers = new Map<number, { run(): void; duration: number }>()
  let nextTimer = 0, stateWrites = 0
  const react = {
    ...hooks.react,
    useLayoutEffect: hooks.react.useEffect,
    useState(initial: unknown) {
      const [value, set] = hooks.react.useState(initial)
      assert(typeof set === 'function')
      return [value, (next: unknown) => { stateWrites++; Reflect.apply(set, undefined, [next]) }]
    },
  }
  const load = moduleLoader(createContext({
    setTimeout(run: () => void, duration: number) {
      const id = ++nextTimer
      timers.set(id, { run, duration })
      return id
    },
    clearTimeout(id: number) { timers.delete(id) },
  }), specifier => {
    if (specifier === 'react') return react
    if (specifier === 'react-native') return { AccessibilityInfo: { announceForAccessibility(text: string) { announcements.push(text) } } }
    if (specifier === 'expo-clipboard') return { setStringAsync(value: string) {
      const request = { value, ...deferred() }
      requests.push(request)
      return request.promise
    } }
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const loadedHook = load(fileURLToPath(new URL('../lib/use-clipboard.ts', import.meta.url))).useClipboardAction
  assert(typeof loadedHook === 'function')
  const hook = loadedHook
  let source = initialSource
  function render(nextSource = source): ReturnType<typeof useClipboardAction> {
    source = nextSource
    hooks.reset()
    const result: unknown = Reflect.apply(hook, undefined, [source, duration])
    assert(typeof result === 'object' && result !== null)
    assert('busy' in result && typeof result.busy === 'boolean')
    assert('copied' in result && (result.copied === null || typeof result.copied === 'string'))
    assert('error' in result && typeof result.error === 'string')
    assert('copy' in result && typeof result.copy === 'function')
    assert('reset' in result && typeof result.reset === 'function')
    const copy = result.copy, reset = result.reset
    for (const effect of hooks.afterRender.splice(0)) effect()
    return {
      busy: result.busy, copied: result.copied, error: result.error,
      async copy(value, key) {
        const outcome: unknown = await Reflect.apply(copy, undefined, [value, key])
        assert(typeof outcome === 'boolean')
        return outcome
      },
      reset() { assert.equal(Reflect.apply(reset, undefined, []), undefined) },
    }
  }
  function fireTimer() {
    const first = timers.entries().next().value
    assert(first)
    timers.delete(first[0])
    first[1].run()
  }
  return {
    render, requests, announcements, timers, fireTimer,
    writes: () => stateWrites,
    unmount() { for (const effect of hooks.effects.values()) effect.cleanup?.() },
  }
}

test('pending native copy has no success feedback; resolution announces once and expires', async () => {
  const f = fixture()
  const work = f.render().copy('visible value')
  assert.equal(f.requests[0].value, 'visible value')
  assert.equal(f.render().busy, true)
  assert.equal(f.render().copied, null)
  assert.deepEqual(f.announcements, [])
  assert.equal(f.timers.size, 0)
  f.requests[0].resolve()
  assert.equal(await work, true)
  assert.equal(f.render().busy, false)
  assert.equal(f.render().copied, 'value')
  assert.deepEqual(f.announcements, ['已复制'])
  assert.equal(f.timers.values().next().value?.duration, 1500)
  f.fireTimer()
  assert.equal(f.render().copied, null)
})

test('rejection gives a fixed error without exposing native errors or the copied token, and allows explicit retry', async () => {
  const f = fixture('one-time test token')
  const work = f.render().copy('one-time test token')
  f.requests[0].reject(new Error('native error containing one-time test token'))
  assert.equal(await work, false)
  assert.equal(f.render().busy, false)
  assert.equal(f.render().copied, null)
  assert.equal(f.render().error, '复制失败，请重试')
  assert.deepEqual(f.announcements, ['复制失败，请重试'])
  assert.equal(f.timers.size, 0)
  const retry = f.render().copy('one-time test token')
  assert.equal(f.render().error, '')
  f.requests[1].resolve()
  assert.equal(await retry, true)
  assert.equal(f.requests.length, 2)
  f.unmount()
  assert.equal(f.timers.size, 0)
})

test('a newer copy owns feedback even when an older operation finishes later', async () => {
  const f = fixture('invitation', 2000)
  const old = f.render().copy('invitation', 'link')
  const latest = f.render().copy('command invitation', 'command')
  f.requests[0].resolve()
  assert.equal(await old, false)
  assert.equal(f.render().busy, true)
  assert.equal(f.render().copied, null)
  assert.deepEqual(f.announcements, [])
  f.requests[1].resolve()
  assert.equal(await latest, true)
  assert.equal(f.render().copied, 'command')
  assert.equal(f.timers.values().next().value?.duration, 2000)
  f.unmount()
})

test('copying again resets the old success timer, including an already queued stale callback', async () => {
  const f = fixture()
  const first = f.render().copy('visible value')
  f.requests[0].resolve()
  await first
  const oldTimer = f.timers.values().next().value
  assert(oldTimer)
  const second = f.render().copy('visible value')
  assert.equal(f.timers.size, 0)
  assert.equal(f.render().copied, null)
  f.requests[1].resolve()
  await second
  oldTimer.run()
  assert.equal(f.render().copied, 'value')
  assert.equal(f.timers.size, 1)
  f.fireTimer()
  assert.equal(f.render().copied, null)
})

test('a changed source clears success and its timer, and invalidates old handlers and pending completions', async () => {
  const f = fixture('old source')
  const oldView = f.render()
  const work = oldView.copy('old source')
  f.render('new source')
  const writes = f.writes()
  f.requests[0].resolve()
  assert.equal(await work, false)
  assert.equal(f.writes(), writes)
  assert.deepEqual(f.announcements, [])
  assert.equal(await oldView.copy('old source'), false)
  assert.equal(f.requests.length, 1)
  const fresh = f.render().copy('new source')
  f.requests[1].resolve()
  await fresh
  assert.equal(f.render().copied, 'value')
  assert.equal(f.render('third source').copied, null)
  assert.equal(f.timers.size, 0)
})

test('reset invalidates a pending copy and an old rejection cannot replace newer success', async () => {
  const f = fixture()
  const oldView = f.render()
  const old = oldView.copy('first')
  oldView.reset()
  assert.equal(f.render().busy, false)
  const latest = f.render().copy('second')
  f.requests[1].resolve()
  await latest
  const writes = f.writes()
  f.requests[0].reject(new Error('late failure'))
  assert.equal(await old, false)
  assert.equal(f.writes(), writes)
  assert.equal(f.render().copied, 'value')
  assert.equal(f.render().error, '')
  assert.deepEqual(f.announcements, ['已复制'])
  f.unmount()
})

test('unmounted copy resolution and rejection make no state writes, announcements, timers, or retries', async () => {
  for (const outcome of ['resolve', 'reject']) {
    const f = fixture()
    const view = f.render()
    const work = view.copy('visible value')
    f.unmount()
    const writes = f.writes()
    if (outcome === 'resolve') f.requests[0].resolve()
    else f.requests[0].reject(new Error('native failure'))
    assert.equal(await work, false)
    assert.equal(f.writes(), writes)
    assert.deepEqual(f.announcements, [])
    assert.equal(f.timers.size, 0)
    assert.equal(await view.copy('visible value'), false)
    assert.equal(f.requests.length, 1)
  }
})
