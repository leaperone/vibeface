import assert from "node:assert/strict"
import test from "node:test"
import { readFile } from "node:fs/promises"
import { stripTypeScriptTypes } from "node:module"
import { readStorage, writeStorage } from "./storage.ts"
import { prepareRpc, rpcAction } from "../../lib/rpc-action.ts"
import { parseIdentityScope, scopeKey } from "../../lib/scope.ts"
import { conversationHref } from "./entry-url.ts"
import { isIncomplete, mergeBasis } from "./labels.ts"
import { inputText, parseConversation, parseNodes, parsePending } from "./model.ts"
import { parseReconcile, reconcileDecision } from "./reconcile.ts"

const context = {
  conversation: { conversation_id: "chat", title: "test" }, messages: [], heads: ["head"],
  missing: { events: [], blobs: [], conflicts: [] }, tasks: [], sessions: [], operations: [],
}
const request = { space_id: "space", operation_id: "op", target_node: "executor", conversation_id: "chat", session_id: "session" }
test("conversation navigation stays in the trusted web and carries no credentials", () => {
  assert.equal(conversationHref("https://node.example/app?capability=secret&basis=%5B%5D#token", "next", { account_id: "a", space_id: "s", device_id: "d" }), "/app?conversation=next&account=a&space=s&device=d")
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
  assert.equal(reconcileDecision(parseReconcile({ operation: null, state: "not_found", space_id: "other-space", target_node: "executor", checked_at_ms: 100 }), request), "pending")
})
test("stored operations retain request contents and enforce the space", () => {
  const full = { ...request, space_id: "space", task_id: null, expected_revision: 3, basis_heads: ["head"], resources: [], action: { type: "send", text: "original" } }
  const pending = { ...request, request: full }
  assert.deepEqual(parsePending([pending], "space")[0]?.request, full)
  assert.throws(() => parsePending([pending], "other-space"))
  assert.throws(() => parsePending([{ ...pending, target_node: "other" }], "space"))
  assert.throws(() => parsePending([{ ...pending, request: { ...full, space_id: undefined, network_id: "space" } }], "space"))
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
  const boundary = await import(`data:text/javascript;base64,${Buffer.from(stripTypeScriptTypes(source.replace(/^import .*\n/gm, ""))).toString("base64")}`)
  assert.equal(boundary.subscriptionRenewalDelay(120000, 0), 60000)
  assert.equal(boundary.subscriptionRenewalDelay(1000, 800), 100)
  assert.throws(() => boundary.subscriptionRenewalDelay(1000, 1000))
  const domain = { kind: "account", id: `acc2_${"a".repeat(64)}` }
  const firstHead = { domain, authority_epoch: 1, sequence: 1, head: `h2_${"1".repeat(64)}` }
  const latestHead = { ...firstHead, sequence: 2, head: `h2_${"2".repeat(64)}` }
  const trust = { kind: "policy", accountId: domain.id, spaceId: `spc2_${"b".repeat(64)}`, pins: [{ domain, genesis_root: "a".repeat(64), minimum_head: firstHead }] }
  const latest = { ...trust, pins: [{ ...trust.pins[0], minimum_head: latestHead }] }
  assert.deepEqual(boundary.mergeAuthorizationTrust(trust, [latest]).pins[0].minimum_head, latestHead)
  assert.deepEqual(boundary.mergeAuthorizationTrust(latest, [trust]).pins[0].minimum_head, latestHead)
  assert.throws(() => boundary.mergeAuthorizationTrust({ ...trust, accountId: `acc2_${"f".repeat(64)}` }, [latest]))
  assert.throws(() => boundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], genesis_root: "f".repeat(64) }] }, [latest]))
  assert.throws(() => boundary.mergeAuthorizationTrust({ ...trust, pins: [{ ...trust.pins[0], minimum_head: { ...latestHead, head: `h2_${"3".repeat(64)}` } }] }, [latest]))
  const switchedAuthority = { ...firstHead, authority_epoch: 2, sequence: 0, head: `h2_${"4".repeat(64)}` }
  assert.deepEqual(boundary.mergeAuthorizationTrust(trust, [{ ...latest, pins: [{ ...latest.pins[0], minimum_head: switchedAuthority }] }]).pins[0].minimum_head, switchedAuthority)
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
