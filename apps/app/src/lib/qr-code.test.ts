import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import test from "node:test"
import { encodeQrCode } from "./qr-code.ts"

test("Web QR images preserve the existing encoder settings and complete link", () => {
  for (const [link, expected] of [
    ["vibeface://join/vibeface1test", "59acdbe3db956427b9992d8771a1e703dbcef8afacb6d6f43ab4e1ec5003e928"],
    ["https://example.com/recover?entry=https%3A%2F%2Fnode.example%2F&device_id=abcdef", "290000bb2bec50067d8a8a5ed32d2adc200be1fe37cdee984e644dc0d508cec5"],
  ]) {
    const result = encodeQrCode(link)
    assert.equal(result.kind, "encoded")
    if (result.kind === "encoded") assert.equal(createHash("sha256").update(result.image).digest("hex"), expected)
  }
})

test("Web QR encoding explicitly rejects an empty link", () => {
  assert.deepEqual(encodeQrCode(""), { kind: "empty" })
})

test("Web QR encoding reports an oversized link without truncating it", () => {
  const link = `https://example.com/${"a".repeat(5000)}`
  assert.deepEqual(encodeQrCode(link), { kind: "too-long" })
  assert.equal(encodeQrCode("https://example.com/").kind, "encoded")
})
