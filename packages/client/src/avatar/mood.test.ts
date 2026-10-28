import assert from "node:assert/strict"
import test from "node:test"
import { moodOf } from "./mood.ts"

test("a question waits first, then work, then the offline device", () => {
  assert.equal(moodOf({ busy: "replying", waiting: true, offline: true }), "waiting")
  assert.equal(moodOf({ busy: "thinking", waiting: false, offline: true }), "thinking")
  assert.equal(moodOf({ busy: "replying", waiting: false, offline: false }), "replying")
  assert.equal(moodOf({ busy: null, waiting: false, offline: true }), "asleep")
  assert.equal(moodOf({ busy: null, waiting: false, offline: false }), "idle")
})
