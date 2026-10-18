import assert from "node:assert/strict"
import test from "node:test"
import { readFile } from "node:fs/promises"
import { stripTypeScriptTypes } from "node:module"
import { readStorage, writeStorage } from "./storage.ts"
import { prepareRpc, rpcAction } from "../../lib/rpc-action.ts"
import { parseIdentityScope, projectScopeKey, scopeKey } from "../../lib/scope.ts"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { inputText, parseConversation, parseNodes, parsePending, parsePendingBindings, parsePendingTasks } from "./model.ts"
import { parseReconcile, reconcileDecision } from "./reconcile.ts"
import type { OperationRequest } from "./types.ts"

async function portableLibrary(name: string): Promise<string> {
  const sourceUrl = new URL(`../../lib/${name}.ts`, import.meta.url)
  let source = stripTypeScriptTypes(await readFile(sourceUrl, "utf8"))
  for (const match of [...source.matchAll(/from ['"]([^'"]+)['"]/g)]) {
    const relative = match[1]
    const target = relative.endsWith('.js') ? new URL(relative, sourceUrl).href : await portableLibrary(relative.replace('./', ''))
    source = source.replace(match[0], `from ${JSON.stringify(target)}`)
  }
  return `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`
}

test("connection files and pasted cards use the same pinned target and reject mixed claims", async () => {
  const secure = await import(new URL('../../../generated/vibeface_secure.js', import.meta.url).href)
  await secure.default({ module_or_path: await readFile(new URL('../../../generated/vibeface_secure_bg.wasm', import.meta.url)) })
  const library = await import(await portableLibrary('connection-file'))
  const key = secure.Identity.fromSecretHex('09'.repeat(32))
  const savedLocation = Object.getOwnPropertyDescriptor(globalThis, 'location')
  Object.defineProperty(globalThis, 'location', { configurable: true, value: { protocol: 'https:', origin: 'https://client.example' } })
  try {
    const content = secure.createNodeConnectionCard('https://relay.example', key.publicKeyHex(), true, undefined)
    const file = new File([content], 'node.vibeface.json', { type: 'application/json' })
    const pasted = await library.inspectConnectionPackage(content)
    assert.deepEqual(await library.inspectConnectionPackage(await library.readConnectionFile(file)), pasted)
    assert.equal(pasted.kind, 'connection')
    assert.deepEqual(pasted.entry, { address: 'https://relay.example', node_id: key.nodeId(), relay: true })
    assert.equal(pasted.nodePublicKey, key.publicKeyHex())
    const raw = JSON.parse(content)
    for (const changed of [{ node_id: '00'.repeat(32) }, { capability: null }, { claim_nonce: null }, { unknown: true }]) {
      await assert.rejects(library.inspectConnectionPackage(JSON.stringify({ ...raw, ...changed })))
    }
    await assert.rejects(library.readConnectionFile(new File(['x'.repeat(65537)], 'too-large.json')))
    await assert.rejects(library.inspectConnectionPackage('中'.repeat(21846)))
  } finally {
    key.free()
    if (savedLocation) Object.defineProperty(globalThis, 'location', savedLocation)
    else Reflect.deleteProperty(globalThis, 'location')
  }
})

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, tasks: [], sessions: [], operations: [],
}
const request: OperationRequest = {
  space_id: "space", operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session", task_id: "task",
  binding: { binding: { space_id: "space", conversation_id: "chat", task_id: "task", task_ref: "a".repeat(64), authority_node: "b".repeat(64), generation: 1, target_node: "executor", session_id: "session", open_spec: { type: "open", executor: "codex", cwd: "/project" }, previous_binding_hash: null, previous_seal: null }, signature: "c".repeat(128) },
  expected_revision: 3, basis_heads: ["head"], resources: ["/project"], action: { type: "send", text: "original" },
}
test("conversation navigation stays in the trusted web and carries no credentials", () => {
  assert.equal(conversationHref("https://node.example/app?capability=secret&basis=%5B%5D#token", "next", { account_id: "a", space_id: "s", device_id: "d" }, "project"), "/app?conversation=next&account=a&space=s&device=d&project=project")
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["b", "a"])), ["a", "b"])
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["ok", 1])), ["a"])
})
test("missing integrity fields fail closed instead of authorizing writes", () => {
  assert.equal(isIncomplete(parseConversation(context, "chat").missing), false)
  assert.equal(isIncomplete(undefined), true)
  assert.equal(isIncomplete({ events: [], blobs: [] }), true)
  assert.throws(() => parseConversation({ ...context, missing: { events: [], blobs: [] } }, "chat"))
  assert.throws(() => parseConversation({ ...context, heads: ["head", 2] }, "chat"))
  assert.throws(() => parseConversation(context, "other"))
  assert.equal(isIncomplete(parseConversation({ ...context, missing: { events: [], blobs: [{ hash: "blob" }], conflicts: [] } }).missing), true)
})
test("capabilities parse the actual provider name and reject malformed node lists", () => {
  const nodes = parseNodes({ nodes: [{ node_id: "n", local: true, online: true, revoked: false, roles: ["scheduler"], capabilities: { manager: "cursor_agent", executors: ["codex"], roots: ["/project"] } }] })
  assert.equal(nodes[0]?.capabilities?.manager, "cursor_agent")
  assert.throws(() => parseNodes({ nodes: [{ node_id: "n" }] }))
})
test("only the exact executor operation can settle a pending write", () => {
  const operation = { request, state: "succeeded", result: {} }
  assert.equal(reconcileDecision(parseReconcile({ operation }), request), "settled")
  assert.equal(reconcileDecision(parseReconcile({ operation: { ...operation, state: "unknown" } }), request), "pending")
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "executor", checked_at_ms: 100 }), request), "missing")
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "other", checked_at_ms: 100 }), request), "pending")
  assert.throws(() => parseReconcile({ operation: null, state: "not_found", space_id: "space", target_node: "executor" }))
  assert.throws(() => reconcileDecision(parseReconcile({ operation: { ...operation, request: { ...request, operation_id: "other" } } }), request))
  assert.throws(() => reconcileDecision(parseReconcile({ operation: { ...operation, request: { ...request, space_id: "other-space" } } }), request))
  assert.throws(() => parseReconcile({ operation: { ...operation, request: { space_id: "space", operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session" } } }))
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "other-space", target_node: "executor", checked_at_ms: 100 }), request), "pending")
})
test("stored operations retain request contents and enforce the space", () => {
  const binding = { binding: { space_id: "space", conversation_id: "chat", task_id: "task", task_ref: "a".repeat(64), authority_node: "b".repeat(64), generation: 1, target_node: "executor", session_id: "session", open_spec: { type: "open", executor: "codex", cwd: "/project" }, previous_binding_hash: null, previous_seal: null }, signature: "c".repeat(128) }
  const full = { ...request, space_id: "space", task_id: "task", binding, expected_revision: 3, basis_heads: ["head"], resources: [], action: { type: "send", text: "original" } }
  const pending = { ...request, request: full }
  assert.deepEqual(parsePending([pending], "space")[0]?.request, full)
  assert.throws(() => parsePending([pending], "other-space"))
  assert.throws(() => parsePending([{ ...pending, target_node: "other" }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, space_id: undefined, network_id: "space" } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, task_id: null } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, binding: undefined } }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, binding: { ...binding, binding: { ...binding.binding, task_id: "other" } } } }], "space"))
})

test("pending task signatures retain every bounded local attempt and reject old records", () => {
  const body = { method: "create_task", conversation_id: "chat", task_id: "task", title: "title", goal: "goal", acceptance: "acceptance", auto_followup: false }
  const first = { body: { ...body, authorization_id: "task:task" }, invocation: { nonce: "first" } }
  const second = { body: first.body, invocation: { nonce: "second" } }
  const pending = { ...body, attempts: [first, second] }
  assert.deepEqual(parsePendingTasks([pending])[0]?.attempts, [first, second])
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: undefined }]))
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: Array(9).fill(first) }]))
  assert.throws(() => parsePendingTasks([{ ...pending, attempts: [{ body, invocation: null }] }]))
})

test("unconfirmed bindings keep the original open intent and signed request", () => {
  const task = { task_id: "task", title: "title", goal: "goal", acceptance: "acceptance", detail: "", status: "open", task_creation: { hash: "creation" }, authority_public_key: "key", binding: null }
  const intent = { space_id: request.space_id, operation_id: request.operation_id, target_node: request.target_node, conversation_id: request.conversation_id, session_id: request.session_id, task_id: "task", expected_revision: 0, basis_heads: ["head"], resources: ["/project"], action: { type: "open", executor: "codex", cwd: "/project" } }
  const pending = { request: intent, task, attempt: { body: { session_id: request.session_id }, invocation: { nonce: "attempt" } } }
  assert.deepEqual(parsePendingBindings([pending], "space"), [pending])
  assert.throws(() => parsePendingBindings([pending], "other-space"))
  assert.throws(() => parsePendingBindings([{ ...pending, request: { ...intent, task_id: "other" } }], "space"))
  assert.throws(() => parsePendingBindings([{ ...pending, request: { ...intent, action: { type: "send", text: "do not replay" } } }], "space"))
})

test("creation fields use the server UTF-8 byte limits and reject whitespace", () => {
  assert.equal(inputText("  名称  ", "会话名称", 512), "名称")
  assert.equal(inputText("中".repeat(170), "会话名称", 512), "中".repeat(170))
  assert.throws(() => inputText("中".repeat(171), "会话名称", 512), /512/)
  assert.throws(() => inputText("   ", "工单目标", 65536), /不能为空/)
  assert.equal(inputText("a".repeat(65536), "验收条件", 65536).length, 65536)
  assert.throws(() => inputText("中".repeat(21846), "验收条件", 65536), /65536/)
})


test("device vault encrypts secrets and rejects wrong passwords, tampering and old formats", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, "")).toString("base64")}`)
  const secret = "09".repeat(32)
  const password = "a local passphrase 123"
  const first = await transport.sealDeviceVault(secret, password)
  const second = await transport.sealDeviceVault(secret, password)
  assert.equal(await transport.openDeviceVault(first, password), secret)
  assert.equal(first.secret, undefined)
  assert.equal(JSON.stringify(first).includes(secret), false)
  assert.notEqual(first.salt, second.salt)
  assert.notEqual(first.iv, second.iv)
  assert.notEqual(first.ciphertext, second.ciphertext)
  await assert.rejects(transport.openDeviceVault(first, "wrong password"))
  await assert.rejects(transport.openDeviceVault({ ...first, ciphertext: `${first.ciphertext[0] === "0" ? "1" : "0"}${first.ciphertext.slice(1)}` }, password))
  await assert.rejects(transport.openDeviceVault({ ...first, iterations: 1 }, password))
  await assert.rejects(transport.openDeviceVault({ ...first, secret }, password))
  await assert.rejects(transport.openDeviceVault({ secret, created_at: 1 }, password))
  await assert.rejects(transport.sealDeviceVault(secret, "short"))
})

test("local PRF wrap authenticates metadata and keeps the independent password vault usable", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, "") + "\nexport { sealPrfVault, openPrfVault, parseProfile };\n").toString("base64")}`)
  const savedLocation = Object.getOwnPropertyDescriptor(globalThis, "location")
  Object.defineProperty(globalThis, "location", { configurable: true, value: { origin: "https://client.example", hostname: "client.example" } })
  try {
    const secret = "09".repeat(32), password = "an independent local passphrase"
    const profile = { format: "vibeface-device-profile-v3", id: "11111111-1111-4111-8111-111111111111", name: "account", public_key: "02".repeat(32), vault: await transport.sealDeviceVault(secret, password) }
    const salt = crypto.getRandomValues(new Uint8Array(32))
    const prf = new Uint8Array(32).fill(9)
    const passkey = await transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf })
    assert.deepEqual(prf, new Uint8Array(32))
    assert.equal(passkey.kdf, "HKDF-SHA256")
    assert.equal(passkey.salt.length, 64)
    assert.equal(passkey.iv.length, 24)
    assert.equal(passkey.ciphertext.length, 96)
    assert.equal(JSON.stringify(passkey).includes(secret), false)
    assert.deepEqual(transport.parseProfile(profile), profile)
    assert.deepEqual(transport.parseProfile({ ...profile, passkey }).vault, profile.vault)
    assert.equal(await transport.openDeviceVault(profile.vault, password), secret)
    const correct = new Uint8Array(32).fill(9)
    assert.equal(await transport.openPrfVault({ ...profile, passkey }, correct), secret)
    assert.deepEqual(correct, new Uint8Array(32))
    const second = await transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf: new Uint8Array(32).fill(9) })
    assert.notEqual(second.iv, passkey.iv)
    assert.notEqual(second.ciphertext, passkey.ciphertext)
    const wrong = new Uint8Array(32).fill(8)
    await assert.rejects(transport.openPrfVault({ ...profile, passkey }, wrong))
    assert.deepEqual(wrong, new Uint8Array(32))
    const changeHex = (value: string) => `${value[0] === "0" ? "1" : "0"}${value.slice(1)}`
    for (const changed of [
      { credential_id: "BAUG" }, { origin: "https://other.example", rp_id: "other.example" }, { rp_id: "other.example" },
      { salt: changeHex(passkey.salt) }, { iv: changeHex(passkey.iv) }, { ciphertext: changeHex(passkey.ciphertext) },
    ]) {
      const output = new Uint8Array(32).fill(9)
      await assert.rejects(transport.openPrfVault({ ...profile, passkey: { ...passkey, ...changed } }, output))
      assert.deepEqual(output, new Uint8Array(32))
    }
    await assert.rejects(transport.openPrfVault({ ...profile, id: "22222222-2222-4222-8222-222222222222", passkey }, new Uint8Array(32).fill(9)))
    await assert.rejects(transport.openPrfVault({ ...profile, public_key: "03".repeat(32), passkey }, new Uint8Array(32).fill(9)))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: { ...passkey, extra: true } }))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: { ...passkey, credential_id: "AQID=" } }))
    assert.throws(() => transport.parseProfile({ ...profile, passkey: null }))
    const short = new Uint8Array(31).fill(9)
    await assert.rejects(transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt, prf: short }))
    assert.deepEqual(short, new Uint8Array(31))
    const invalidSaltPrf = new Uint8Array(32).fill(9)
    await assert.rejects(transport.sealPrfVault(profile, secret, { credentialId: "AQID", salt: salt.slice(0, 31), prf: invalidSaltPrf }))
    assert.deepEqual(invalidSaltPrf, new Uint8Array(32))
  } finally {
    if (savedLocation) Object.defineProperty(globalThis, "location", savedLocation)
    else Reflect.deleteProperty(globalThis, "location")
  }
})

test("local PRF profile writes preserve other profiles and reject changed or cancelled writers", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const shim = `const init = async () => {}; const Identity = { fromSecretHex(secret) {
    if (secret !== '09'.repeat(32)) throw new Error('wrong secret');
    return { secretHex: () => secret, publicKeyHex: () => '02'.repeat(32), nodeId: () => '03'.repeat(32), free() {} };
  } };`
  const transport = await import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, shim + "\n")).toString("base64")}`)
  const saved = new Map(["isSecureContext", "location", "navigator", "indexedDB"].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  const records = new Map<string, unknown>(), locks: string[] = [], writes: string[] = []
  let beforeUpdate: (() => void) | null = null
  let lockGate: Promise<void> | null = null
  const db = { close() {}, transaction(_store: string, mode: string) {
    let aborted = false, pending = 0
    const transaction = { oncomplete: () => {}, onerror: () => {}, onabort: () => {}, abort() { aborted = true; queueMicrotask(() => transaction.onabort()) }, objectStore: () => ({
      get: (id: string) => request(() => structuredClone(records.get(id))), getAll: () => request(() => structuredClone([...records.values()])),
      put: (value: unknown, id: string) => request(() => { writes.push(id); records.set(id, structuredClone(value)) }),
    }) }
    function request(action: () => unknown) {
      const output: { result: unknown; onsuccess: () => void } = { result: undefined, onsuccess: () => {} }
      pending++
      queueMicrotask(() => {
        if (aborted) return
        try { output.result = action(); output.onsuccess() } catch { transaction.abort() }
        pending--
        if (!pending && !aborted) queueMicrotask(() => { if (!pending && !aborted) transaction.oncomplete() })
      })
      return output
    }
    if (mode === "readwrite") beforeUpdate?.()
    return transaction
  } }
  const properties = {
    isSecureContext: true, location: { origin: "https://client.example", hostname: "client.example" },
    navigator: { locks: { async request(name: string, work: () => Promise<void>) { locks.push(name); if (lockGate) await lockGate; await work() } } },
    indexedDB: { open() { const output = { result: db, onsuccess: () => {} }; queueMicrotask(() => output.onsuccess()); return output } },
  }
  for (const [key, value] of Object.entries(properties)) Object.defineProperty(globalThis, key, { configurable: true, value })
  try {
    const password = "a separate local passphrase"
    const first = { format: "vibeface-device-profile-v3", id: "11111111-1111-4111-8111-111111111111", name: "first", public_key: "02".repeat(32), vault: await transport.sealDeviceVault("09".repeat(32), password) }
    const other = { ...first, id: "22222222-2222-4222-8222-222222222222", name: "second" }
    records.set(first.id, first); records.set(other.id, other)
    const options = () => ({ credentialId: "AQID", salt: crypto.getRandomValues(new Uint8Array(32)), prf: new Uint8Array(32).fill(9) })
    await transport.initClient({ mode: "unlock", profileId: first.id, password })
    const valid = options()
    await transport.protectClientVaultWithPrf(valid)
    assert.deepEqual(valid.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    assert.equal(locks[0], `vibeface-device-vault-v3:${first.id}:writer`)
    assert.deepEqual(records.get(other.id), other)
    const backup = JSON.parse(await transport.exportDeviceVault())
    assert.deepEqual(backup.vault, first.vault)
    const local = await transport.passkeyVaultUnlockOptions(first.id)
    assert.equal(local.credentialId, "AQID")
    assert.deepEqual(local.options.publicKey.allowCredentials, [{ type: "public-key", id: "AQID" }])
    assert.equal(local.options.publicKey.rpId, "client.example")
    assert.equal(local.options.publicKey.userVerification, "required")
    assert.deepEqual(local.salt, Uint8Array.from(backup.passkey.salt.match(/../g), (byte: string) => Number.parseInt(byte, 16)))
    assert.notEqual(local.options.publicKey.challenge, (await transport.passkeyVaultUnlockOptions(first.id)).options.publicKey.challenge)
    transport.lockClient()
    const unlockPrf = new Uint8Array(32).fill(9)
    assert.equal((await transport.initClient({ mode: "unlock_prf", profileId: first.id, prf: unlockPrf })).profileId, first.id)
    assert.deepEqual(unlockPrf, new Uint8Array(32))
    beforeUpdate = () => records.set(first.id, { ...backup, name: "changed in another tab" })
    const changed = options()
    await assert.rejects(transport.protectClientVaultWithPrf(changed), /保存/)
    assert.deepEqual(changed.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    beforeUpdate = null
    let releaseLock = () => {}
    lockGate = new Promise<void>(resolve => { releaseLock = resolve })
    const cancelled = options(), pending = transport.protectClientVaultWithPrf(cancelled)
    transport.lockClient(); releaseLock()
    await assert.rejects(pending, /改变/)
    assert.deepEqual(cancelled.prf, new Uint8Array(32))
    assert.deepEqual(writes, [first.id])
    const unavailable = options()
    await assert.rejects(transport.protectClientVaultWithPrf(unavailable), /解锁/)
    assert.deepEqual(unavailable.prf, new Uint8Array(32))
    const invalid = new Uint8Array(32).fill(9)
    await assert.rejects(transport.initClient({ mode: "unlock_prf", profileId: "invalid", prf: invalid }))
    assert.deepEqual(invalid, new Uint8Array(32))
    assert.deepEqual(records.get(other.id), other)
  } finally {
    transport.lockClient()
    for (const [key, descriptor] of saved) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
  }
})


test("local records and writer locks stay in their exact account space and device", () => {
  const scope = { account_id: "account:a", space_id: "space:a", device_id: "device:a" }
  const original = scopeKey(scope, "pending")
  assert.equal(scopeKey({ ...scope }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, account_id: "account:b" }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, space_id: "space:b" }, "pending"), original)
  assert.notEqual(scopeKey({ ...scope, device_id: "device:b" }, "pending"), original)
  assert.notEqual(scopeKey(scope, "frontiers"), original)
  assert.notEqual(scopeKey({ account_id: "a.b", space_id: "c", device_id: "d" }, "pending"), scopeKey({ account_id: "a", space_id: "b.c", device_id: "d" }, "pending"))
  assert.throws(() => scopeKey({ ...scope, account_id: "" }, "pending"))
  const first = `res2_${"a".repeat(64)}`, second = `res2_${"b".repeat(64)}`
  assert.notEqual(projectScopeKey(scope, first, "pending"), projectScopeKey(scope, second, "pending"))
  assert.notEqual(projectScopeKey(scope, first, "pending"), projectScopeKey(scope, first, "workspace-writer"))
  assert.throws(() => projectScopeKey(scope, "", "pending"))
  assert.throws(() => projectScopeKey(scope, scope.account_id, "pending"))
})


test("unknown writes remain in their original scope while another account is active", () => {
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window")
  const records = new Map<string, string>()
  Object.defineProperty(globalThis, "window", { configurable: true, value: { localStorage: { getItem: (key: string) => records.get(key) ?? null, setItem: (key: string, value: string) => records.set(key, value) } } })
  try {
    const first = { account_id: "account:a", space_id: "space:a", device_id: "device:a" }
    const second = { account_id: "account:b", space_id: "space:b", device_id: "device:b" }
    const unknown = { ...request, state: "unknown" }
    writeStorage(scopeKey(first, "pending"), [unknown], true)
    assert.deepEqual(readStorage(scopeKey(second, "pending"), [], true), [])
    writeStorage(scopeKey(second, "pending"), [{ ...request, operation_id: "other" }], true)
    assert.deepEqual(readStorage(scopeKey(first, "pending"), [], true), [unknown])
    assert.equal(reconcileDecision(parseReconcile({ operation: { request, state: "unknown" } }), unknown), "pending")
  } finally {
    if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow)
    else Reflect.deleteProperty(globalThis, "window")
  }
})

test("only the new typed verified scope crosses the client boundary", () => {
  const scope = { account_id: `acc2_${"a".repeat(64)}`, space_id: `spc2_${"b".repeat(64)}`, device_id: `dev2_${"c".repeat(64)}` }
  assert.deepEqual(parseIdentityScope(scope), scope)
  assert.throws(() => parseIdentityScope({ ...scope, account_id: scope.device_id }))
  assert.throws(() => parseIdentityScope({ network_id: "a".repeat(64), subject_id: "b".repeat(64) }))
  assert.throws(() => parseIdentityScope({ ...scope, actor: "service" }))
  assert.throws(() => Reflect.apply(scopeKey, undefined, [undefined, "pending"]))
})


test("locking during initialization releases candidates and preserves temporary mode when storage fails", async () => {
  const source = await readFile(new URL("../../../transport.js", import.meta.url), "utf8")
  const originalSecure = Object.getOwnPropertyDescriptor(globalThis, "isSecureContext")
  const originalDatabase = Object.getOwnPropertyDescriptor(globalThis, "indexedDB")
  Object.defineProperty(globalThis, "isSecureContext", { configurable: true, value: true })
  Object.defineProperty(globalThis, "indexedDB", { configurable: true, value: { open() { const request: { onerror?: () => void } = {}; queueMicrotask(() => request.onerror?.()); return request } } })
  async function fixture(scenario: string) {
    const shim = `
      const scenario = ${JSON.stringify(scenario)};
      const initGate = Promise.withResolvers(), kdfGate = Promise.withResolvers(), entered = Promise.withResolvers();
      const stats = { generated: 0, freed: 0 };
      const init = () => initGate.promise;
      const Identity = { generate() {
        stats.generated++; let released = false;
        return { secretHex: () => '01'.repeat(32), publicKeyHex: () => '02'.repeat(32), nodeId: () => '03'.repeat(32),
          free() { if (released) throw new Error('double free'); released = true; stats.freed++; } };
      } };
      const crypto = { getRandomValues: bytes => globalThis.crypto.getRandomValues(bytes), randomUUID: () => globalThis.crypto.randomUUID(),
        subtle: { importKey: async () => ({}), deriveKey: async () => { entered.resolve(); await kdfGate.promise; return {}; }, encrypt: async () => new Uint8Array(48).buffer } };
      export const probe = { initGate, kdfGate, entered, stats };
    `
    return import(`data:text/javascript;base64,${Buffer.from(source.replace(/^import init,.*\n/, shim)).toString("base64")}`)
  }
  try {
    const duringCrypto = await fixture("crypto")
    duringCrypto.probe.initGate.resolve()
    const creating = duringCrypto.initClient({ mode: "create", name: "test", password: "test passphrase 123" })
    await duringCrypto.probe.entered.promise
    duringCrypto.lockClient()
    assert.equal(duringCrypto.probe.stats.freed, 1)
    duringCrypto.probe.kdfGate.resolve()
    await assert.rejects(creating, /取消/)
    assert.equal(duringCrypto.probe.stats.freed, 1)
    assert.equal((await duringCrypto.clientVaultStatus()).status, "unavailable")
    assert.equal((await duringCrypto.initClient({ mode: "temporary" })).profileId, null)
    await assert.rejects(duringCrypto.initClient({ mode: "create", name: "other", password: "test passphrase 123" }), /先锁定/)
    duringCrypto.lockClient()
    assert.equal(duringCrypto.probe.stats.freed, 2)
    const duringInit = await fixture("init")
    const canceled = duringInit.initClient({ mode: "create", name: "test", password: "test passphrase 123" })
    duringInit.lockClient()
    duringInit.probe.kdfGate.resolve()
    duringInit.probe.initGate.resolve()
    await assert.rejects(canceled, /取消/)
    assert.equal(duringInit.probe.stats.generated, 0)
  } finally {
    if (originalSecure) Object.defineProperty(globalThis, "isSecureContext", originalSecure)
    else Reflect.deleteProperty(globalThis, "isSecureContext")
    if (originalDatabase) Object.defineProperty(globalThis, "indexedDB", originalDatabase)
    else Reflect.deleteProperty(globalThis, "indexedDB")
  }
})


test("saved policy heads cannot move backwards or replace an account root", async () => {
  const source = await readFile(new URL("../../lib/authorization.ts", import.meta.url), "utf8")
  const trustBoundary = await import("../../lib/authorization-trust.ts")
  const boundary = await import(`data:text/javascript;base64,${Buffer.from(stripTypeScriptTypes(source.replace(/^(?:import .*|export \{ .* from .*)\n/gm, ""))).toString("base64")}`)
  assert.equal(boundary.subscriptionRenewalDelay(120000, 0), 60000)
  assert.equal(boundary.subscriptionRenewalDelay(1000, 800), 100)
  assert.throws(() => boundary.subscriptionRenewalDelay(1000, 1000))
  const domain: Parameters<typeof trustBoundary.mergeAuthorizationTrust>[0]["pins"][number]["domain"] = { kind: "account", id: `acc2_${"a".repeat(64)}` }
  const firstHead = { domain, authority_epoch: 1, sequence: 1, head: `h2_${"1".repeat(64)}` }
  const latestHead = { ...firstHead, sequence: 2, head: `h2_${"2".repeat(64)}` }
  const trust: Parameters<typeof trustBoundary.mergeAuthorizationTrust>[0] = { kind: "policy", accountId: domain.id, spaceId: `spc2_${"b".repeat(64)}`, pins: [{ domain, genesis_root: "a".repeat(64), minimum_head: firstHead }] }
  const latest = { ...trust, pins: [{ ...trust.pins[0], minimum_head: latestHead }] }
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(trust, [latest]).pins[0].minimum_head, latestHead)
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(latest, [trust]).pins[0].minimum_head, latestHead)
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, accountId: `acc2_${"f".repeat(64)}` }, [latest]))
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], genesis_root: "f".repeat(64) }] }, [latest]))
  assert.throws(() => trustBoundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], minimum_head: { ...latestHead, head: `h2_${"3".repeat(64)}` } }] }, [latest]))
  const switchedAuthority = { ...firstHead, authority_epoch: 2, sequence: 0, head: `h2_${"4".repeat(64)}` }
  assert.deepEqual(trustBoundary.mergeAuthorizationTrust(trust, [{ ...latest, pins: [{ ...latest.pins[0], minimum_head: switchedAuthority }] }]).pins[0].minimum_head, switchedAuthority)
})


test("RPC action and authorization identity come from the exact local request", () => {
  const first = prepareRpc({ method: "operate", params: { request: { operation_id: "original", action: { type: "send" } } }, spaceId: "space", authorizationId: "random" })
  assert.equal(first.authorizationId, "original")
  assert.equal(first.action, "execution_send")
  assert.equal(first.body.space_id, "space")
  assert.equal(first.body.authorization_id, "original")
  assert.equal(rpcAction("update_task", { status: "accepted" }), "task_accept")
  assert.equal(rpcAction("update_task", { status: "cancelled" }), "task_cancel")
  assert.equal(rpcAction("update_task", { status: "running" }), "task_progress")
  const read = prepareRpc({ method: "reconcile", params: { operation_id: "original" }, spaceId: "space", authorizationId: "new-query" })
  assert.deepEqual(read.body, { operation_id: "original", method: "reconcile", space_id: "space", authorization_id: "new-query" })
  assert.equal(read.authorizationId, "new-query")
  assert.deepEqual(prepareRpc({ method: "message", params: { message_id: "same" }, spaceId: "space", authorizationId: "a" }).body, prepareRpc({ method: "message", params: { message_id: "same" }, spaceId: "space", authorizationId: "b" }).body)
  assert.throws(() => prepareRpc({ method: "conversation", params: { space_id: "other" }, spaceId: "space", authorizationId: "read" }))
  assert.throws(() => prepareRpc({ method: "conversation", params: { method: "export" }, spaceId: "space", authorizationId: "read" }))
  assert.throws(() => rpcAction("unrecognized", {}))
  assert.throws(() => rpcAction("operate", { request: { action: { type: "unrecognized" } } }))
  assert.throws(() => prepareRpc({ method: "message", params: {}, spaceId: "space", authorizationId: "read" }))
})

test("Passkey requests pin the client RP and keep PRF output out of the Node response", async () => {
  const moduleUrl = (source: string) => `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
  const transport = moduleUrl((await readFile(new URL("../../../transport.js", import.meta.url), "utf8")).replace(/^import init,.*\n/, ""))
  const nodeSource = (await readFile(new URL("../../lib/node.ts", import.meta.url), "utf8")).replace("'../../transport.js'", JSON.stringify(transport))
  const node = moduleUrl(stripTypeScriptTypes(nodeSource))
  const source = (await readFile(new URL("../../lib/passkey.ts", import.meta.url), "utf8")).replace("'./node'", JSON.stringify(node))
  const passkey = await import(moduleUrl(stripTypeScriptTypes(source)))
  const saved = new Map(["isSecureContext", "PublicKeyCredential", "AuthenticatorAttestationResponse", "location", "navigator"].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  let calls = 0
  class Attestation {
    clientDataJSON = Uint8Array.from([4, 5]).buffer
    attestationObject = Uint8Array.from([6, 7]).buffer
    getTransports() { return ["internal"] }
  }
  class Credential {
    id = "AQID"
    rawId = Uint8Array.from([1, 2, 3]).buffer
    response = new Attestation()
    getClientExtensionResults() { return { prf: { results: { first: new Uint8Array(32).fill(9).buffer } } } }
  }
  const controller = new AbortController()
  const properties = {
    isSecureContext: true, PublicKeyCredential: Credential, AuthenticatorAttestationResponse: Attestation,
    location: { hostname: "client.example", origin: "https://client.example", protocol: "https:" },
    navigator: { credentials: { create: async (options: CredentialCreationOptions) => {
      calls++
      assert.equal(options.signal, controller.signal)
      assert.equal(options.publicKey?.rp.id, "client.example")
      const challenge = options.publicKey?.challenge
      assert.ok(challenge instanceof ArrayBuffer)
      assert.deepEqual(new Uint8Array(challenge), new Uint8Array(32).fill(3))
      const salt = options.publicKey?.extensions?.prf?.eval?.first
      assert.ok(salt instanceof ArrayBuffer)
      assert.deepEqual(new Uint8Array(salt), new Uint8Array(32).fill(4))
      return new Credential()
    } } },
  }
  for (const [key, value] of Object.entries(properties)) Object.defineProperty(globalThis, key, { configurable: true, value })
  try {
    const options = { publicKey: {
      rp: { id: "client.example", name: "Vibeface" }, user: { id: "AQID", name: "account", displayName: "Vibeface account" },
      challenge: passkey.encodeBase64Url(new Uint8Array(32).fill(3).buffer), pubKeyCredParams: [{ type: "public-key", alg: -7 }],
      attestation: "none", authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "required" }, excludeCredentials: [],
    } }
    const result = await passkey.createPasskey(options, controller.signal, new Uint8Array(32).fill(4))
    assert.deepEqual(result.response, { id: "AQID", rawId: "AQID", type: "public-key", response: { clientDataJSON: "BAU", attestationObject: "Bgc", transports: ["internal"] }, clientExtensionResults: {} })
    assert.deepEqual(result.prf, new Uint8Array(32).fill(9))
    assert.equal(JSON.stringify(result.response).includes("prf"), false)
    await assert.rejects(passkey.createPasskey({ publicKey: { ...options.publicKey, rp: { id: "node.example", name: "other" } } }, controller.signal), /站点/)
    assert.equal(calls, 1)
    assert.throws(() => passkey.decodeBase64Url("AQID="))
    assert.throws(() => passkey.decodeBase64Url("AB"))
    assert.deepEqual(new Uint8Array(passkey.decodeBase64Url("AQID")), Uint8Array.from([1, 2, 3]))
  } finally {
    for (const [key, descriptor] of saved) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
  }
})
