import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import type { Attachment } from '@vibeface/client/chat/types.ts'

export type AttachmentPage = { bytes: Uint8Array; next_offset: number | null; size: number }
export type ReadAttachment = (attachment: Attachment, offset: number) => Promise<AttachmentPage>
const limit = 64 * 1024 * 1024

export function validAttachment(attachment: Attachment) {
  return /^[a-f0-9]{64}$/.test(attachment.hash) && Number.isSafeInteger(attachment.size) && attachment.size > 0 && attachment.size <= limit
    && attachment.name.length > 0 && new TextEncoder().encode(attachment.name).length <= 255 && !/[/\\\p{Cc}\p{Cs}]/u.test(attachment.name)
}
export async function verifyAttachment(attachment: Attachment, bytes: Uint8Array) {
  if (!validAttachment(attachment) || bytes.length !== attachment.size || await sha256Hex(bytes) !== attachment.hash) throw new Error('附件内容与消息记录不符，请重新下载。')
  return bytes
}
/** The callback is scoped by the route to its network and conversation. No URL is inferred from a hash. */
export async function downloadAttachment(attachment: Attachment, read: ReadAttachment, progress?: (received: number) => void) {
  if (!validAttachment(attachment)) throw new Error('附件记录无效，无法打开。')
  const bytes = new Uint8Array(attachment.size)
  let offset = 0
  while (offset < attachment.size) {
    const page = await read(attachment, offset)
    const end = offset + page.bytes.length
    if (page.size !== attachment.size || end > attachment.size || end <= offset || (page.next_offset !== null && (!Number.isSafeInteger(page.next_offset) || page.next_offset !== end || end === attachment.size)) || (page.next_offset === null && end !== attachment.size)) throw new Error('附件下载不完整，请重试。')
    bytes.set(page.bytes, offset)
    offset = end
    progress?.(offset)
  }
  return verifyAttachment(attachment, bytes)
}
/** Only formats identified from verified bytes can enter native Image. SVG and web content stay files. */
export function attachmentImageType(bytes: Uint8Array): 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp' | null {
  const matches = (at: number, signature: number[]) => signature.every((value, index) => bytes[at + index] === value)
  if (matches(0, [137, 80, 78, 71, 13, 10, 26, 10])) return 'image/png'
  if (matches(0, [255, 216, 255])) return 'image/jpeg'
  if (matches(0, [71, 73, 70, 56]) && (bytes[4] === 55 || bytes[4] === 57) && bytes[5] === 97) return 'image/gif'
  if (matches(0, [82, 73, 70, 70]) && matches(8, [87, 69, 66, 80])) return 'image/webp'
  return null
}
export function attachmentSize(size: number) {
  return size < 1000 ? `${size} 字节` : size < 1000000 ? `${(size / 1000).toFixed(1)} KB` : `${(size / 1000000).toFixed(1)} MB`
}
