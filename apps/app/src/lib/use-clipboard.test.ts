import assert from "node:assert/strict"
import test from "node:test"
import { fileURLToPath } from "node:url"
import { createContext } from "node:vm"
import { hookHarness,moduleLoader } from "./test-harness.ts"

function deferred() {
  let resolve = () => {}, reject = (_cause: Error) => {}
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}

function fixture({ duration = 2000, mode = "pending" }: { duration?: number | null; mode?: "pending" | "missing" | "throws" } = {}) {
  const hooks = hookHarness()
  const requests: ({ value: string } & ReturnType<typeof deferred>)[] = []
  const notices: string[] = []
  const timers = new Map<number, { run(): void; duration: number }>()
  const setups: (() => (() => void) | void)[] = []
  let nextTimer = 0, writes = 0, current = true, scope: unknown = "source"
  const react = {
    ...hooks.react,
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      setups.push(run)
      hooks.react.useEffect(run, dependencies)
    },
    useState(initial: unknown) {
      const [value, set] = hooks.react.useState(initial)
      assert.ok(typeof set === "function")
      return [value, (next: unknown) => { writes++; Reflect.apply(set, undefined, [next]) }]
    },
  }
  const load = moduleLoader(createContext({
    navigator: mode === "missing" ? {} : { clipboard: { writeText(value: string) {
      if (mode === "throws") throw new Error("platform text must stay private")
      const request = { value, ...deferred() }; requests.push(request); return request.promise
    } } },
    setTimeout(run: () => void, duration: number) { const id = ++nextTimer; timers.set(id, { run, duration }); return id },
    clearTimeout(id: number) { timers.delete(id) },
  }), specifier => {
    assert.equal(specifier, "react")
    return react
  })
  const loadedHook = load(fileURLToPath(new URL("./use-clipboard.ts", import.meta.url))).useClipboard
  assert.ok(typeof loadedHook === "function")
  const hook = loadedHook
  function render(nextScope = scope, effects = true) {
    scope = nextScope; hooks.reset()
    const result: unknown = Reflect.apply(hook, undefined, [scope, { duration, isCurrent: () => current, onCopied: () => notices.push("copied"), onFailed: () => notices.push("failed") }])
    assert.ok(result && typeof result === "object" && "state" in result && "copied" in result && "failed" in result && "copy" in result && "reset" in result)
    assert.ok(result.state && typeof result.state === "object" && "status" in result.state)
    assert.ok(typeof result.copy === "function" && typeof result.reset === "function")
    const { copy, reset } = result
    const key = "key" in result.state ? result.state.key : null
    if (effects) flush()
    return { status: result.state.status, key, copied: result.copied, failed: result.failed,
      async copy(value: string, key?: string) { await Reflect.apply(copy, undefined, [value, key]) },
      reset() { Reflect.apply(reset, undefined, []) },
    }
  }
  function flush() { hooks.afterRender.splice(0).forEach(run => run()) }
  function unmount() { for (const effect of hooks.effects.values()) effect.cleanup?.() }
  return { render, requests, notices, timers, flush, unmount, writes: () => writes, invalidate: () => { current = false },
    strictReplay() {
      unmount()
      const setup = setups.at(-1); assert.ok(setup)
      const cleanup = setup()
      return () => { if (cleanup) cleanup() }
    },
  }
}

test("Web copy starts in the gesture stack and success gets a full two seconds on every attempt", async () => {
  const f = fixture()
  try {
    const first = f.render().copy("complete link", "link")
    assert.deepEqual(f.requests.map(({ value }) => value), ["complete link"])
    assert.equal(f.render().status, "pending")
    assert.equal(f.render().copied, false)
    f.requests[0].resolve(); await first
    assert.equal(f.render().key, "link")
    assert.equal(f.render().copied, true)
    const old = [...f.timers.values()][0]; assert.equal(old.duration, 2000)
    const second = f.render().copy("complete link", "link")
    assert.equal(f.timers.size, 0)
    f.requests[1].resolve(); await second
    assert.equal([...f.timers.values()][0].duration, 2000)
    old.run()
    assert.equal(f.render().copied, true)
    const latest = [...f.timers.values()][0]; latest.run()
    assert.equal(f.render().status, "idle")
  } finally { f.unmount() }
})

test("Web denial, missing API and synchronous throws are handled without false success; retry clears failure", async () => {
  for (const mode of ["missing", "throws", "pending"] satisfies Array<"missing" | "throws" | "pending">) {
    const f = fixture({ mode })
    try {
      const work = f.render().copy("private token")
      if (mode === "pending") f.requests[0].reject(new Error("private token"))
      await work
      assert.equal(f.render().failed, true)
      assert.equal(f.render().copied, false)
      assert.equal(f.timers.size, 0)
      assert.deepEqual(f.notices, ["failed"])
      if (mode === "pending") {
        const retry = f.render().copy("private token")
        assert.equal(f.render().failed, false)
        f.requests[1].resolve(); await retry
        assert.equal(f.render().copied, true)
      }
    } finally { f.unmount() }
  }
})

test("only the latest key can publish, whether the older operation resolves or rejects", async () => {
  for (const outcome of ["resolve", "reject"] satisfies Array<"resolve" | "reject">) {
    const f = fixture()
    try {
      const old = f.render().copy("old link", "link"), latest = f.render().copy("new command", "command")
      f.requests[1].resolve(); await latest
      if (outcome === "resolve") f.requests[0].resolve()
      else f.requests[0].reject(new Error("denied"))
      await old
      assert.equal(f.render().status, "copied")
      assert.equal(f.render().key, "command")
      assert.deepEqual(f.notices, ["copied"])
      assert.equal(f.timers.size, 1)
    } finally { f.unmount() }
  }
})

test("source replacement hides feedback before cleanup and invalidates old handlers and completions", async () => {
  const f = fixture()
  try {
    const view = f.render(), success = view.copy("first")
    f.requests[0].resolve(); await success
    const timer = [...f.timers.values()][0]
    assert.equal(f.render("new source", false).status, "idle")
    const writes = f.writes()
    timer.run()
    await view.copy("first")
    assert.equal(f.writes(), writes)
    assert.equal(f.requests.length, 1)
    f.flush()
    assert.equal(f.timers.size, 0)
    const pending = f.render().copy("new source")
    f.render("third source", false)
    const before = f.writes()
    f.requests[1].reject(new Error("old refusal")); await pending
    assert.equal(f.writes(), before)
    assert.deepEqual(f.notices, ["copied"])
    f.flush()
  } finally { f.unmount() }
})

test("reset, unmount, Strict Mode replay and connection invalidation discard pending completion", async () => {
  for (const stop of ["reset", "unmount", "strict", "connection"]) for (const outcome of ["resolve", "reject"]) {
    const f = fixture()
    let cleanup = () => {}
    try {
      const view = f.render(), work = view.copy("old")
      if (stop === "reset") view.reset()
      if (stop === "unmount") f.unmount()
      if (stop === "strict") cleanup = f.strictReplay()
      if (stop === "connection") f.invalidate()
      const writes = f.writes()
      if (outcome === "resolve") f.requests[0].resolve()
      else f.requests[0].reject(new Error("late refusal"))
      await work
      assert.equal(f.writes(), writes)
      assert.equal(f.notices.length, 0)
      assert.equal(f.timers.size, 0)
      if (stop === "strict" || stop === "reset") {
        const fresh = f.render().copy("fresh")
        f.requests[1].resolve(); await fresh
        assert.equal(f.render().copied, true)
      }
    } finally { cleanup(); f.unmount() }
  }
})

test("persistent feedback survives retry failure and resets with its source without a timer", async () => {
  const f = fixture({ duration: null })
  try {
    const first = f.render().copy("node link")
    f.requests[0].resolve(); await first
    assert.equal(f.render().copied, true)
    const retry = f.render().copy("node link")
    assert.equal(f.render().copied, true)
    f.requests[1].reject(new Error("denied")); await retry
    assert.equal(f.render().copied, true)
    assert.equal(f.timers.size, 0)
    assert.equal(f.render("another node").copied, false)
  } finally { f.unmount() }
})


test("cleanup cancels success timers and an already queued timer cannot write after cleanup", async () => {
  for (const strict of [false, true]) {
    const f = fixture()
    let cleanup = () => {}
    try {
      const work = f.render().copy("complete value")
      f.requests[0].resolve(); await work
      const timer = [...f.timers.values()][0]
      if (strict) cleanup = f.strictReplay()
      else f.unmount()
      assert.equal(f.timers.size, 0)
      const writes = f.writes()
      timer.run()
      assert.equal(f.writes(), writes)
    } finally { cleanup(); f.unmount() }
  }
})
