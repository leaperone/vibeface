import assert from "node:assert/strict"
import test from "node:test"
import { conversationHref, entryHref, validEntry } from "./entry-url.ts"
import { mergeBasis } from "./labels.ts"
import { reconcileDecision, shouldDropPending } from "./reconcile.ts"

test("entry links only carry conversation and basis", () => {
  const href = entryHref("https://node.example:8080/", "conv-1", ["head-a"])
  const url = new URL(href)
  assert.equal(url.origin, "https://node.example:8080")
  assert.deepEqual([...url.searchParams.keys()], ["conversation", "basis"])
  assert.equal(url.searchParams.get("conversation"), "conv-1")
  assert.equal(url.username, "")
  assert.equal(url.password, "")
  assert.equal(url.hash, "")
  assert.equal(url.searchParams.get("password"), null)
  assert.equal(url.searchParams.get("csrf"), null)
  assert.equal(url.searchParams.get("cookie"), null)
})

test("entry addresses reject secrets, paths, and non-http origins", () => {
  assert.throws(() => validEntry("https://user:secret@node.example"), /不包含账号/)
  assert.throws(() => validEntry("https://node.example/private"), /不包含账号/)
  assert.throws(() => validEntry("https://node.example/?csrf=token"), /不包含账号/)
  assert.equal(validEntry("http://192.168.1.10:8080/"), "http://192.168.1.10:8080")
})

test("long basis is left off the entry url", () => {
  const href = entryHref("https://node.example", "conv-1", ["x".repeat(6000)])
  assert.equal(new URL(href).searchParams.get("basis"), null)
})

test("in-page conversation navigation drops secrets and basis", () => {
  const href = conversationHref("https://user:pw@node.example/app?csrf=1&cookie=2&password=3&basis=%5B%5D&conversation=old#token", "next")
  const url = new URL(href)
  assert.equal(url.username, "")
  assert.equal(url.password, "")
  assert.equal(url.hash, "")
  assert.equal(url.searchParams.get("conversation"), "next")
  assert.equal(url.searchParams.get("basis"), null)
  assert.equal(url.searchParams.get("csrf"), null)
  assert.equal(url.searchParams.get("cookie"), null)
  assert.equal(url.searchParams.get("password"), null)
})

test("basis from another entry merges string heads only", () => {
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["b", "a"])), ["a", "b"])
  assert.deepEqual(mergeBasis(["a"], "not-json"), ["a"])
  assert.deepEqual(mergeBasis(["a"], JSON.stringify(["ok", 1])), ["a"])
})

test("unknown operations are reconciled and never resent", () => {
  assert.equal(reconcileDecision({ state: "succeeded" }, { target_node: "n1" }), "settled")
  assert.equal(reconcileDecision({ state: "failed" }, { target_node: "n1" }), "settled")
  assert.equal(reconcileDecision({ state: "rejected" }, { target_node: "n1" }), "settled")
  assert.equal(reconcileDecision({ state: "not_found", target_node: "n1", operation: null }, { target_node: "n1" }), "missing")
  assert.equal(reconcileDecision({ state: "not_found", target_node: "other", operation: null }, { target_node: "n1" }), "pending")
  assert.equal(reconcileDecision({ operation: { state: "running" } }, { target_node: "n1" }), "pending")
  assert.equal(shouldDropPending(undefined), false)
  assert.equal(shouldDropPending(500), false)
  assert.equal(shouldDropPending(409), true)
  assert.equal(shouldDropPending(400), true)
})
