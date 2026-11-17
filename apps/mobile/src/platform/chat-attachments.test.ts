import assert from 'node:assert/strict'
import test from 'node:test'
import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import { attachmentImageType, downloadAttachment, validAttachment, verifyAttachment } from './chat-attachments.ts'

const bytes = new TextEncoder().encode('one attachment across pages')
async function fixture() { return { name: 'report.txt', size: bytes.length, hash: await sha256Hex(bytes) } }
test('download assembles advancing pages and verifies the final signed hash', async () => {
  const file = await fixture(), received: number[] = [], offsets: number[] = []
  assert.deepEqual(await downloadAttachment(file, async (attachment, offset) => {
    assert.equal(attachment, file)
    offsets.push(offset)
    const end = Math.min(bytes.length, offset + 5)
    return { bytes: bytes.slice(offset, end), size: bytes.length, next_offset: end === bytes.length ? null : end }
  }, value => received.push(value)), bytes)
  assert.equal(offsets[0], 0)
  assert.equal(received.at(-1), bytes.length)
})
test('incorrect metadata, size, digest and page advancement never become a usable local attachment', async () => {
  const file = await fixture()
  for (const bad of [{ ...file, hash: '../escape' }, { ...file, name: '../file.txt' }, { ...file, size: 64 * 1024 * 1024 + 1 }, { ...file, size: 1.5 }]) assert.equal(validAttachment(bad), false)
  await assert.rejects(verifyAttachment({ ...file, hash: '0'.repeat(64) }, bytes), /不符/)
  await assert.rejects(verifyAttachment(file, bytes.slice(1)), /不符/)
  for (const page of [
    { bytes: new Uint8Array(), next_offset: 0, size: bytes.length },
    { bytes, next_offset: null, size: bytes.length + 1 },
    { bytes: bytes.slice(0, 4), next_offset: null, size: bytes.length },
    { bytes: bytes.slice(0, 4), next_offset: 5, size: bytes.length },
    { bytes, next_offset: 0, size: bytes.length },
  ]) await assert.rejects(downloadAttachment(file, async () => page), /不完整/)
  await assert.rejects(downloadAttachment(file, async () => ({ bytes: new Uint8Array(bytes.length), size: bytes.length, next_offset: null })), /不符/)
})
test('native image display requires a recognized byte signature rather than filename or web content', () => {
  assert.equal(attachmentImageType(new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])), 'image/png')
  assert.equal(attachmentImageType(new Uint8Array([255, 216, 255])), 'image/jpeg')
  assert.equal(attachmentImageType(new TextEncoder().encode('GIF89a')), 'image/gif')
  assert.equal(attachmentImageType(new TextEncoder().encode('RIFFxxxxWEBP')), 'image/webp')
  assert.equal(attachmentImageType(new TextEncoder().encode('<svg></svg>')), null)
  assert.equal(attachmentImageType(new TextEncoder().encode('<html>')), null)
})
