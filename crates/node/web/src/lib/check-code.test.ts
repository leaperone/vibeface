import assert from "node:assert/strict"
import test from "node:test"
import { checkCode } from "./check-code.ts"

test("check code is 12 stable digits that change with any id", async () => {
  const code = await checkCode("net", "inviter", "device")
  assert.match(code, /^\d{12}$/)
  assert.equal(await checkCode("net", "inviter", "device"), code)
  assert.notEqual(await checkCode("net", "inviter", "device2"), code)
  assert.notEqual(await checkCode("net2", "inviter", "device"), code)
})
