import { Directory, File } from 'expo-file-system'
import * as Sharing from 'expo-sharing'
import type { Attachment } from '@vibeface/client/chat/types.ts'
import { attachmentImageType, downloadAttachment, validAttachment, verifyAttachment, type ReadAttachment } from './chat-attachments'
import { privateFiles } from './files'

export type LocalAttachment = { uri: string; mimeType: string; image: boolean }
const pending = new Set<Promise<unknown>>()
let clearing = false, epoch = 0
const changed = () => new Error('附件缓存正在清除，请完成后重试。')

/** Media cleanup owns only this set; privateFiles.erase can wait for both without a dependency cycle. */
async function mediaOperation<T>(operation: (current: () => boolean) => Promise<T>): Promise<T> {
  if (clearing) throw changed()
  const version = epoch
  const work = privateFiles.run(async fileCurrent => {
    const current = () => fileCurrent() && !clearing && version === epoch
    const result = await operation(current)
    if (!current()) throw changed()
    return result
  })
  pending.add(work)
  try { return await work } finally { pending.delete(work) }
}

/** Every descendant must be a direct child in the app-created scope/hash/name layout. */
function childName(parent: Directory, child: Directory | File) {
  const prefix = `${parent.uri.replace(/\/$/, '')}/`
  const relative = child.uri.startsWith(prefix) ? child.uri.slice(prefix.length).replace(/\/$/, '') : ''
  let name: string
  try { name = decodeURIComponent(relative) } catch { throw new Error('附件缓存路径无效，请清除缓存后重试。') }
  if (!name || name === '.' || name === '..' || /[/\\\p{Cc}]/u.test(name)) throw new Error('附件缓存路径超出应用目录，无法读取。')
  return name
}

export function attachmentCacheStats(): { bytes: number; files: number } {
  if (clearing) throw changed()
  const root = privateFiles.attachments
  if (!root.exists) return { bytes: 0, files: 0 }
  const stats = { bytes: 0, files: 0 }
  const visit = (directory: Directory, depth: number) => {
    for (const item of directory.list()) {
      const name = childName(directory, item)
      if (item instanceof Directory) {
        if (depth >= 2 || !/^[a-f0-9]{64}$/.test(name)) throw new Error('附件缓存目录无法识别，请清除缓存后重试。')
        visit(item, depth + 1)
      } else if (item instanceof File) {
        if (depth !== 2 || !item.exists) throw new Error('附件缓存已变化，请重试。')
        const size = item.size
        if (!Number.isSafeInteger(size) || size < 0 || !Number.isSafeInteger(stats.bytes + size)) throw new Error('附件缓存大小无法读取，请重试。')
        stats.bytes += size
        stats.files++
      } else throw new Error('附件缓存记录无法识别，请重试。')
    }
  }
  visit(root, 0)
  return stats
}

/** Invalidate first, drain existing local reads/writes/shares, then delete only the media root. */
export async function clearAttachmentCache(): Promise<void> {
  if (clearing) throw changed()
  clearing = true
  epoch++
  try {
    await privateFiles.run(async current => {
      await Promise.allSettled([...pending])
      if (!current()) throw new Error('应用内数据正在清除，请完成后重试。')
      if (privateFiles.attachments.exists) privateFiles.attachments.delete()
    })
  } finally { clearing = false }
}
function location(scope: string, attachment: Attachment) {
  if (!/^[a-f0-9]{64}$/.test(scope) || !validAttachment(attachment)) throw new Error('附件记录无效，无法打开。')
  const directory = new Directory(privateFiles.attachments, scope, attachment.hash)
  return { directory, file: new File(directory, attachment.name) }
}
/** Scope must hash network + device + conversation; cached bytes never cross that boundary. */
export async function localAttachment(scope: string, attachment: Attachment, read?: ReadAttachment, progress?: (received: number) => void): Promise<LocalAttachment> {
  return mediaOperation(async current => {
    const { directory, file } = location(scope, attachment)
    let bytes: Uint8Array | undefined
    if (file.exists) {
      try { bytes = await verifyAttachment(attachment, await file.bytes()) }
      catch { if (!current()) throw changed(); if (file.exists) file.delete() }
    }
    if (!bytes) {
      if (!read) throw new Error('附件尚未下载，请连接原网络后重试。')
      bytes = await downloadAttachment(attachment, async (file, offset) => {
        if (!current()) throw changed()
        return read(file, offset)
      }, received => { if (current()) progress?.(received) })
      if (!current()) throw changed()
      directory.create({ idempotent: true, intermediates: true })
      const temporary = new File(directory, `${crypto.randomUUID()}.tmp`)
      let moved = false
      try {
        await temporary.write(bytes)
        await verifyAttachment(attachment, await temporary.bytes())
        if (!current()) throw changed()
        if (file.exists) file.delete()
        await temporary.move(file)
        moved = true
      } finally { if (!moved && temporary.exists) temporary.delete() }
    }
    if (!current()) throw changed()
    const mimeType = attachmentImageType(bytes)
    return { uri: file.uri, mimeType: mimeType ?? 'application/octet-stream', image: mimeType !== null }
  })
}

export async function shareAttachment(local: LocalAttachment, title: string) {
  await mediaOperation(async current => {
    const file = new File(local.uri)
    const prefix = `${privateFiles.attachments.uri.replace(/\/$/, '')}/`
    const parts = file.uri.startsWith(prefix) ? file.uri.slice(prefix.length).split('/') : []
    const [scope, hash] = parts
    if (parts.length !== 3 || !scope || !hash || !/^[a-f0-9]{64}$/.test(scope) || !/^[a-f0-9]{64}$/.test(hash)) throw new Error('附件副本不属于应用缓存，无法分享。')
    childName(new Directory(privateFiles.attachments, scope, hash), file)
    if (!file.exists) throw new Error('本地副本已被清除，请重新下载。')
    if (!await Sharing.isAvailableAsync()) throw new Error('这个设备暂时无法打开系统分享，请稍后重试。')
    if (!current()) throw changed()
    await Sharing.shareAsync(local.uri, { mimeType: local.mimeType, dialogTitle: title })
  })
}
